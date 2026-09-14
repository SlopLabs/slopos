//! RFC 1951 DEFLATE and RFC 1952 gzip framing.
//!
//! No compression crate exists for this target and none may be added, so the
//! wire format is implemented here from the specifications. `gzip` and
//! `tar -z` are the callers.

/// A decompressed stream is capped at 64 MiB: a crafted stream expands without
/// bound otherwise, and nothing this userland ships is larger.
const MAX_OUTPUT: usize = 64 << 20;

const MAX_BITS: usize = 15;
const WINDOW: usize = 32768;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const HASH_BITS: usize = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;
const CHUNK: usize = 32768;
const NONE: u32 = u32::MAX;

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// The order the code-length code lengths arrive in, RFC 1951 section 3.2.7.
const CLEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

#[derive(Clone, Copy)]
pub enum Error {
    Truncated,
    StoredLength,
    BlockType,
    Code,
    Distance,
    TooLarge,
    NotGzip,
    Method,
    Flags,
    Crc,
    Length,
}

impl Error {
    pub fn message(&self) -> &'static str {
        match self {
            Error::Truncated => "unexpected end of compressed data",
            Error::StoredLength => "stored block length mismatch",
            Error::BlockType => "invalid compressed block type",
            Error::Code => "invalid Huffman code",
            Error::Distance => "match distance precedes start of stream",
            Error::TooLarge => "decompressed data exceeds the 64 MiB limit",
            Error::NotGzip => "not in gzip format",
            Error::Method => "unsupported compression method",
            Error::Flags => "unsupported gzip header flags",
            Error::Crc => "CRC check failed",
            Error::Length => "length check failed",
        }
    }
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    fn bit(&mut self) -> Result<u32, Error> {
        let byte = *self.data.get(self.pos).ok_or(Error::Truncated)?;
        let value = (byte >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(value as u32)
    }

    fn bits(&mut self, count: u32) -> Result<u32, Error> {
        let mut value = 0;
        for shift in 0..count {
            value |= self.bit()? << shift;
        }
        Ok(value)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// A canonical Huffman decoder held as per-length counts plus the symbols in
/// code order, which is the representation RFC 1951 section 3.2.2 describes.
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn decode(&self, bits: &mut Bits) -> Result<u16, Error> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=MAX_BITS {
            code |= bits.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Error::Code)
    }
}

fn build(lengths: &[u8]) -> Result<Huffman, Error> {
    let mut counts = [0u16; MAX_BITS + 1];
    for &len in lengths {
        if len as usize > MAX_BITS {
            return Err(Error::Code);
        }
        counts[len as usize] += 1;
    }
    counts[0] = 0;

    // An over-subscribed set has no canonical assignment; an incomplete one is
    // legal and shows up as the single-symbol distance tree.
    let mut left = 1i32;
    for len in 1..=MAX_BITS {
        left <<= 1;
        left -= counts[len] as i32;
        if left < 0 {
            return Err(Error::Code);
        }
    }

    let mut offsets = [0u16; MAX_BITS + 2];
    for len in 1..=MAX_BITS {
        offsets[len + 1] = offsets[len] + counts[len];
    }
    let mut symbols = vec![0u16; lengths.len()];
    for (symbol, &len) in lengths.iter().enumerate() {
        if len != 0 {
            symbols[offsets[len as usize] as usize] = symbol as u16;
            offsets[len as usize] += 1;
        }
    }
    Ok(Huffman { counts, symbols })
}

fn fixed_trees() -> Result<(Huffman, Huffman), Error> {
    let mut lit = [0u8; 288];
    for (symbol, len) in lit.iter_mut().enumerate() {
        *len = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Ok((build(&lit)?, build(&[5u8; 30])?))
}

fn dynamic_trees(bits: &mut Bits) -> Result<(Huffman, Huffman), Error> {
    let hlit = bits.bits(5)? as usize + 257;
    let hdist = bits.bits(5)? as usize + 1;
    let hclen = bits.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return Err(Error::Code);
    }

    let mut clen = [0u8; 19];
    for &slot in CLEN_ORDER.iter().take(hclen) {
        clen[slot] = bits.bits(3)? as u8;
    }
    let code_tree = build(&clen)?;

    let total = hlit + hdist;
    let mut lengths = vec![0u8; total];
    let mut i = 0;
    while i < total {
        let symbol = code_tree.decode(bits)?;
        match symbol {
            0..=15 => {
                lengths[i] = symbol as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err(Error::Code);
                }
                let previous = lengths[i - 1];
                let run = 3 + bits.bits(2)? as usize;
                if i + run > total {
                    return Err(Error::Code);
                }
                for _ in 0..run {
                    lengths[i] = previous;
                    i += 1;
                }
            }
            17 => {
                let run = 3 + bits.bits(3)? as usize;
                if i + run > total {
                    return Err(Error::Code);
                }
                i += run;
            }
            18 => {
                let run = 11 + bits.bits(7)? as usize;
                if i + run > total {
                    return Err(Error::Code);
                }
                i += run;
            }
            _ => return Err(Error::Code),
        }
    }
    Ok((build(&lengths[..hlit])?, build(&lengths[hlit..])?))
}

fn stored_block(bits: &mut Bits, out: &mut Vec<u8>) -> Result<(), Error> {
    bits.align();
    if bits.pos + 4 > bits.data.len() {
        return Err(Error::Truncated);
    }
    let len = u16::from_le_bytes([bits.data[bits.pos], bits.data[bits.pos + 1]]) as usize;
    let nlen = u16::from_le_bytes([bits.data[bits.pos + 2], bits.data[bits.pos + 3]]);
    if nlen != !(len as u16) {
        return Err(Error::StoredLength);
    }
    bits.pos += 4;
    if bits.pos + len > bits.data.len() {
        return Err(Error::Truncated);
    }
    if out.len() + len > MAX_OUTPUT {
        return Err(Error::TooLarge);
    }
    out.extend_from_slice(&bits.data[bits.pos..bits.pos + len]);
    bits.pos += len;
    Ok(())
}

fn coded_block(
    bits: &mut Bits,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), Error> {
    loop {
        let symbol = lit.decode(bits)?;
        if symbol < 256 {
            if out.len() + 1 > MAX_OUTPUT {
                return Err(Error::TooLarge);
            }
            out.push(symbol as u8);
            continue;
        }
        if symbol == 256 {
            return Ok(());
        }
        let index = symbol as usize - 257;
        if index >= LEN_BASE.len() {
            return Err(Error::Code);
        }
        let len = LEN_BASE[index] as usize + bits.bits(LEN_EXTRA[index] as u32)? as usize;
        let code = dist.decode(bits)? as usize;
        if code >= DIST_BASE.len() {
            return Err(Error::Code);
        }
        let distance = DIST_BASE[code] as usize + bits.bits(DIST_EXTRA[code] as u32)? as usize;
        if distance == 0 || distance > out.len() {
            return Err(Error::Distance);
        }
        if out.len() + len > MAX_OUTPUT {
            return Err(Error::TooLarge);
        }
        let mut src = out.len() - distance;
        for _ in 0..len {
            let byte = out[src];
            out.push(byte);
            src += 1;
        }
    }
}

/// Decode a raw DEFLATE stream: stored, fixed-Huffman and dynamic-Huffman
/// blocks. Bytes after the final block are ignored, as a framing layer owns
/// whatever follows.
pub fn inflate(input: &[u8]) -> Result<Vec<u8>, Error> {
    let mut bits = Bits {
        data: input,
        pos: 0,
        bit: 0,
    };
    let (fixed_lit_tree, fixed_dist_tree) = fixed_trees()?;
    let mut out = Vec::new();
    loop {
        let last = bits.bit()?;
        match bits.bits(2)? {
            0 => stored_block(&mut bits, &mut out)?,
            1 => coded_block(&mut bits, &mut out, &fixed_lit_tree, &fixed_dist_tree)?,
            2 => {
                let (lit, dist) = dynamic_trees(&mut bits)?;
                coded_block(&mut bits, &mut out, &lit, &dist)?;
            }
            _ => return Err(Error::BlockType),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    held: u32,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            acc: 0,
            held: 0,
        }
    }

    fn bits(&mut self, value: u32, count: u32) {
        if count == 0 {
            return;
        }
        self.acc |= (value & ((1u32 << count) - 1)) << self.held;
        self.held += count;
        while self.held >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.held -= 8;
        }
    }

    /// A Huffman code travels most-significant bit first while everything else
    /// in DEFLATE is least-significant first, so the code is reversed here.
    fn code(&mut self, code: u32, len: u32) {
        let mut reversed = 0;
        for i in 0..len {
            reversed |= ((code >> i) & 1) << (len - 1 - i);
        }
        self.bits(reversed, len);
    }

    fn align(&mut self) {
        if self.held > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.held = 0;
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

enum Token {
    Lit(u8),
    Match(u16, u16),
}

fn fixed_lit(symbol: u16) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol as u32, 8),
        144..=255 => (0x190 + symbol as u32 - 144, 9),
        256..=279 => (symbol as u32 - 256, 7),
        _ => (0xc0 + symbol as u32 - 280, 8),
    }
}

fn length_code(len: usize) -> (u16, u32, u32) {
    let mut i = LEN_BASE.len() - 1;
    while LEN_BASE[i] as usize > len {
        i -= 1;
    }
    (
        257 + i as u16,
        LEN_EXTRA[i] as u32,
        (len - LEN_BASE[i] as usize) as u32,
    )
}

fn dist_code(distance: usize) -> (u16, u32, u32) {
    let mut i = DIST_BASE.len() - 1;
    while DIST_BASE[i] as usize > distance {
        i -= 1;
    }
    (
        i as u16,
        DIST_EXTRA[i] as u32,
        (distance - DIST_BASE[i] as usize) as u32,
    )
}

fn hash3(bytes: &[u8]) -> usize {
    (((bytes[0] as usize) << 10) ^ ((bytes[1] as usize) << 5) ^ bytes[2] as usize) & (HASH_SIZE - 1)
}

fn insert(input: &[u8], pos: usize, head: &mut [u32], prev: &mut [u32]) {
    if pos + MIN_MATCH <= input.len() {
        let slot = hash3(&input[pos..]);
        prev[pos & (WINDOW - 1)] = head[slot];
        head[slot] = pos as u32;
    }
}

/// Walk the hash chain for `pos`, answering the longest match and its
/// distance. Window-modulo `prev` slots can alias a later position, so the
/// walk terminates on the `tries` bound and on a candidate not behind `pos`.
fn find_match(
    input: &[u8],
    pos: usize,
    head: &[u32],
    prev: &[u32],
    chain: usize,
    nice: usize,
) -> (usize, usize) {
    let max = (input.len() - pos).min(MAX_MATCH);
    if max < MIN_MATCH {
        return (0, 0);
    }
    let floor = pos.saturating_sub(WINDOW);
    let mut candidate = head[hash3(&input[pos..])];
    let mut best_len = 0;
    let mut best_dist = 0;
    let mut tries = chain;
    while candidate != NONE && tries > 0 {
        let at = candidate as usize;
        if at < floor || at >= pos {
            break;
        }
        let mut len = 0;
        while len < max && input[at + len] == input[pos + len] {
            len += 1;
        }
        if len > best_len {
            best_len = len;
            best_dist = pos - at;
            if len >= nice {
                break;
            }
        }
        candidate = prev[at & (WINDOW - 1)];
        tries -= 1;
    }
    if best_len >= MIN_MATCH {
        (best_len, best_dist)
    } else {
        (0, 0)
    }
}

fn fixed_cost(tokens: &[Token]) -> usize {
    let mut bits = 3 + 7;
    for token in tokens {
        bits += match *token {
            Token::Lit(byte) => fixed_lit(byte as u16).1 as usize,
            Token::Match(len, distance) => {
                let (symbol, extra, _) = length_code(len as usize);
                let (_, dist_extra, _) = dist_code(distance as usize);
                fixed_lit(symbol).1 as usize + extra as usize + 5 + dist_extra as usize
            }
        };
    }
    bits
}

fn emit_fixed(writer: &mut BitWriter, tokens: &[Token], last: bool) {
    writer.bits(last as u32, 1);
    writer.bits(1, 2);
    for token in tokens {
        match *token {
            Token::Lit(byte) => {
                let (code, len) = fixed_lit(byte as u16);
                writer.code(code, len);
            }
            Token::Match(length, distance) => {
                let (symbol, extra, extra_value) = length_code(length as usize);
                let (code, len) = fixed_lit(symbol);
                writer.code(code, len);
                writer.bits(extra_value, extra);
                let (dist_symbol, dist_extra, dist_value) = dist_code(distance as usize);
                writer.code(dist_symbol as u32, 5);
                writer.bits(dist_value, dist_extra);
            }
        }
    }
    let (code, len) = fixed_lit(256);
    writer.code(code, len);
}

fn emit_stored(writer: &mut BitWriter, data: &[u8], last: bool) {
    writer.bits(last as u32, 1);
    writer.bits(0, 2);
    writer.align();
    let len = data.len() as u16;
    writer.raw(&len.to_le_bytes());
    writer.raw(&(!len).to_le_bytes());
    writer.raw(data);
}

/// LZ77 over a 32 KiB window with fixed-Huffman blocks, re-emitted stored when
/// the codes do not beat the bytes. No dynamic-Huffman encoding: `level` only
/// picks how far the match finder searches.
pub fn deflate(input: &[u8], level: u8) -> Vec<u8> {
    let (chain, nice) = match level {
        0 => (0, 0),
        1..=3 => (16, 32),
        4..=6 => (128, 128),
        _ => (1024, MAX_MATCH),
    };
    let mut writer = BitWriter::new(input.len() / 2 + 64);
    let mut head = vec![NONE; HASH_SIZE];
    let mut prev = vec![NONE; WINDOW];
    let mut tokens: Vec<Token> = Vec::new();
    let mut pos = 0;
    loop {
        let start = pos;
        // A match may run past the chunk edge; the stored fallback then covers
        // exactly the bytes the tokens consumed.
        let edge = (start + CHUNK).min(input.len());
        tokens.clear();
        while pos < edge {
            let (len, distance) = if chain > 0 {
                find_match(input, pos, &head, &prev, chain, nice)
            } else {
                (0, 0)
            };
            if len >= MIN_MATCH {
                tokens.push(Token::Match(len as u16, distance as u16));
                for step in 0..len {
                    insert(input, pos + step, &mut head, &mut prev);
                }
                pos += len;
            } else {
                tokens.push(Token::Lit(input[pos]));
                insert(input, pos, &mut head, &mut prev);
                pos += 1;
            }
        }
        let last = pos >= input.len();
        let raw = &input[start..pos];
        if fixed_cost(&tokens) < 3 + 7 + 32 + raw.len() * 8 {
            emit_fixed(&mut writer, &tokens, last);
        } else {
            emit_stored(&mut writer, raw, last);
        }
        if last {
            return writer.finish();
        }
    }
}

const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// CRC-32 with the reflected gzip polynomial, RFC 1952 section 8.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc = CRC_TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Frame `data` as a single-member gzip stream: the 10-byte header, an
/// optional FNAME, the deflate data, then CRC32 and ISIZE.
pub fn gzip_wrap(data: &[u8], level: u8, name: Option<&[u8]>, mtime: u32) -> Vec<u8> {
    let named = name.and_then(|bytes| {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        if end == 0 { None } else { Some(&bytes[..end]) }
    });
    let mut out = Vec::with_capacity(data.len() / 2 + 32);
    out.extend_from_slice(&[0x1f, 0x8b, 0x08]);
    out.push(if named.is_some() { 0x08 } else { 0x00 });
    out.extend_from_slice(&mtime.to_le_bytes());
    out.push(0x00);
    out.push(0x03);
    if let Some(bytes) = named {
        out.extend_from_slice(bytes);
        out.push(0);
    }
    out.extend_from_slice(&deflate(data, level));
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

fn read_cstr(input: &[u8], pos: &mut usize) -> Result<Vec<u8>, Error> {
    let start = *pos;
    while *pos < input.len() && input[*pos] != 0 {
        *pos += 1;
    }
    if *pos >= input.len() {
        return Err(Error::Truncated);
    }
    let field = input[start..*pos].to_vec();
    *pos += 1;
    Ok(field)
}

/// Decode a gzip stream, answering the data and the stored original name. The
/// CRC32/ISIZE trailer is checked. One member per stream: the trailer is the
/// last eight bytes of the input, so RFC 1952 concatenation is not decoded.
pub fn gzip_unwrap(input: &[u8]) -> Result<(Vec<u8>, Option<Vec<u8>>), Error> {
    if input.len() < 18 {
        return Err(
            if input.len() >= 2 && (input[0] != 0x1f || input[1] != 0x8b) {
                Error::NotGzip
            } else {
                Error::Truncated
            },
        );
    }
    if input[0] != 0x1f || input[1] != 0x8b {
        return Err(Error::NotGzip);
    }
    if input[2] != 0x08 {
        return Err(Error::Method);
    }
    let flags = input[3];
    if flags & 0xe0 != 0 {
        return Err(Error::Flags);
    }
    let mut pos = 10;
    if flags & 0x04 != 0 {
        if pos + 2 > input.len() {
            return Err(Error::Truncated);
        }
        let extra = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
        pos += 2;
        if pos + extra > input.len() {
            return Err(Error::Truncated);
        }
        pos += extra;
    }
    let mut name = None;
    if flags & 0x08 != 0 {
        name = Some(read_cstr(input, &mut pos)?);
    }
    if flags & 0x10 != 0 {
        read_cstr(input, &mut pos)?;
    }
    if flags & 0x02 != 0 {
        if pos + 2 > input.len() {
            return Err(Error::Truncated);
        }
        pos += 2;
    }
    if pos + 8 > input.len() {
        return Err(Error::Truncated);
    }

    let trailer = input.len() - 8;
    let data = inflate(&input[pos..trailer])?;
    let crc = u32::from_le_bytes([
        input[trailer],
        input[trailer + 1],
        input[trailer + 2],
        input[trailer + 3],
    ]);
    let size = u32::from_le_bytes([
        input[trailer + 4],
        input[trailer + 5],
        input[trailer + 6],
        input[trailer + 7],
    ]);
    if crc32(&data) != crc {
        return Err(Error::Crc);
    }
    if data.len() as u32 != size {
        return Err(Error::Length);
    }
    Ok((data, name))
}
