//! SHA-256 (FIPS 180-4) and the `sha256sum` utility built on it.
//!
//! The digest is streamed through a fixed block, so hashing a file costs a
//! buffer rather than the file.

use std::io::Read;

use super::input::{Input, open, sources, split_lines, trim_newline};
use super::opts::{Opt, Opts};
use super::{Ctx, Tool};

const USAGE: &str = "sha256sum [-bc] [file...]";

/// Bytes pulled per `read(2)` while digesting.
const READ_CHUNK: usize = 64 * 1024;

pub static TOOLS: &[Tool] = &[Tool {
    name: "sha256sum",
    desc: "Compute or check SHA-256 digests",
    usage: USAGE,
    run: sha256sum,
}];

const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// A SHA-256 in progress: the chaining state, the partial block, and the
/// message length the padding has to encode.
pub struct Sha256 {
    state: [u32; 8],
    block: [u8; 64],
    filled: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: H0,
            block: [0; 64],
            filled: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total = self.total.wrapping_add(bytes.len() as u64);
        if self.filled > 0 {
            let want = (64 - self.filled).min(bytes.len());
            self.block[self.filled..self.filled + want].copy_from_slice(&bytes[..want]);
            self.filled += want;
            bytes = &bytes[want..];
            if self.filled < 64 {
                return;
            }
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        let mut chunks = bytes.chunks_exact(64);
        for chunk in chunks.by_ref() {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            compress(&mut self.state, &block);
        }
        let tail = chunks.remainder();
        self.block[..tail.len()].copy_from_slice(tail);
        self.filled = tail.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.block[self.filled] = 0x80;
        self.filled += 1;
        if self.filled > 56 {
            self.block[self.filled..].fill(0);
            let block = self.block;
            compress(&mut self.state, &block);
            self.filled = 0;
        }
        self.block[self.filled..56].fill(0);
        self.block[56..].copy_from_slice(&bits.to_be_bytes());
        let block = self.block;
        compress(&mut self.state, &block);

        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

fn compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    for i in 16..64 {
        let x = w[i - 15];
        let y = w[i - 2];
        let s0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let s1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }

    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

fn sha256sum(ctx: &mut Ctx, argv: &[&[u8]]) -> i32 {
    let mut check = false;
    let mut binary = false;
    let mut opts = Opts::new(argv, "bc");
    for opt in opts.by_ref() {
        match opt {
            Opt::Flag(b'b') => binary = true,
            Opt::Flag(b'c') => check = true,
            Opt::Long(name, _) if name == b"check" => check = true,
            Opt::Long(name, _) if name == b"binary" => binary = true,
            Opt::Long(name, _) => {
                ctx.warn_at(name, b"invalid option");
                return ctx.usage(USAGE);
            }
            Opt::Unknown(flag) => {
                ctx.warn_at(&[flag], b"invalid option");
                return ctx.usage(USAGE);
            }
            _ => {}
        }
    }
    let operands = opts.operands();
    let mut buffer = vec![0u8; READ_CHUNK];

    if check {
        return verify(ctx, sources(operands), &mut buffer);
    }

    let mut status = 0;
    for operand in sources(operands) {
        let Some(digest) = digest_operand(ctx, operand, &mut buffer) else {
            status = 1;
            continue;
        };
        write_hex(ctx, &digest);
        ctx.out.s(if binary { " *" } else { "  " });
        ctx.out.write(operand);
        ctx.out.nl();
    }
    status
}

fn verify(ctx: &mut Ctx, operands: &[&[u8]], buffer: &mut [u8]) -> i32 {
    let mut status = 0;
    let mut mismatched = 0u64;
    let mut malformed = 0u64;
    let mut checked = 0u64;

    for operand in operands {
        let Some(mut list) = open(ctx, operand) else {
            status = 1;
            continue;
        };
        let mut text = Vec::new();
        if let Err(error) = list.read_to_end(&mut text) {
            ctx.warn_io(operand, &error);
            status = 1;
            continue;
        }
        for raw in split_lines(&text) {
            let line = trim_newline(raw);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            let Some((expected, name)) = parse_line(line) else {
                malformed += 1;
                continue;
            };
            checked += 1;
            let Some(digest) = digest_operand(ctx, name, buffer) else {
                ctx.out.write(name);
                ctx.out.s(": FAILED open or read");
                ctx.out.nl();
                mismatched += 1;
                status = 1;
                continue;
            };
            ctx.out.write(name);
            if hex(&digest).as_bytes() == expected.to_ascii_lowercase() {
                ctx.out.s(": OK");
            } else {
                ctx.out.s(": FAILED");
                mismatched += 1;
                status = 1;
            }
            ctx.out.nl();
        }
    }

    // The summary belongs after the per-file results a reader is watching,
    // and `warn` goes out unbuffered.
    ctx.out.flush();
    if malformed > 0 {
        let mut message = malformed.to_string();
        message.push_str(" line(s) are improperly formatted");
        ctx.warn(message.as_bytes());
    }
    if mismatched > 0 {
        let mut message = mismatched.to_string();
        message.push_str(" computed checksum(s) did NOT match");
        ctx.warn(message.as_bytes());
    }
    if checked == 0 && status == 0 {
        ctx.warn(b"no properly formatted checksum lines found");
        return 1;
    }
    status
}

/// `<64 hex><space><space-or-*><name>`, the format the default output writes.
fn parse_line(line: &[u8]) -> Option<(&[u8], &[u8])> {
    if line.len() < 66 {
        return None;
    }
    let (digest, rest) = line.split_at(64);
    if !digest.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if rest[0] != b' ' && rest[0] != b'\t' {
        return None;
    }
    let rest = &rest[1..];
    let name = match rest.first() {
        Some(b' ') | Some(b'*') => &rest[1..],
        _ => rest,
    };
    if name.is_empty() {
        return None;
    }
    Some((digest, name))
}

fn digest_operand(ctx: &mut Ctx, operand: &[u8], buffer: &mut [u8]) -> Option<[u8; 32]> {
    let mut input: Input = open(ctx, operand)?;
    let mut hasher = Sha256::new();
    loop {
        match input.read(buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) => {
                ctx.warn_io(operand, &error);
                return None;
            }
        }
    }
    Some(hasher.finish())
}

fn write_hex(ctx: &mut Ctx, digest: &[u8; 32]) {
    let text = hex(digest);
    ctx.out.s(&text);
}

fn hex(digest: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}
