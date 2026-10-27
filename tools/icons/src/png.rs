//! An RGBA PNG: each row filtered as PNG's heuristic picks, deflated with
//! greedy LZ77 matches under the fixed Huffman codes (RFC 1951 §3.2.6).

use slopos_abi::draw::Color32;

pub fn encode(width: u32, height: u32, pixels: &[Color32]) -> Vec<u8> {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib(&filtered(width as usize, pixels)));
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &b| {
        (0..8).fold(crc ^ u32::from(b), |c, _| {
            (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg())
        })
    })
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    match (pa <= pb && pa <= pc, pb <= pc) {
        (true, _) => a,
        (false, true) => b,
        (false, false) => c,
    }
}

/// Each row behind the filter whose bytes, read as signed, sum smallest.
fn filtered(width: usize, pixels: &[Color32]) -> Vec<u8> {
    let raw: Vec<u8> = pixels
        .iter()
        .flat_map(|p| [p.red(), p.green(), p.blue(), p.alpha()])
        .collect();
    let stride = width * 4;
    let zero = vec![0u8; stride];
    let mut out = Vec::with_capacity(raw.len() + raw.len() / stride);
    for (y, row) in raw.chunks_exact(stride).enumerate() {
        let up = if y == 0 {
            &zero[..]
        } else {
            &raw[(y - 1) * stride..y * stride]
        };
        let at = |i: usize, back: &[u8]| if i >= 4 { back[i - 4] } else { 0 };
        let candidates: [Vec<u8>; 4] = [
            row.to_vec(),
            (0..stride)
                .map(|i| row[i].wrapping_sub(at(i, row)))
                .collect(),
            (0..stride).map(|i| row[i].wrapping_sub(up[i])).collect(),
            (0..stride)
                .map(|i| row[i].wrapping_sub(paeth(at(i, row), up[i], at(i, up))))
                .collect(),
        ];
        let cost = |bytes: &Vec<u8>| {
            bytes
                .iter()
                .map(|&b| u32::from((b as i8).unsigned_abs()))
                .sum::<u32>()
        };
        let best = (0..4).min_by_key(|&f| cost(&candidates[f])).unwrap_or(0);
        out.push([0, 1, 2, 4][best]);
        out.extend_from_slice(&candidates[best]);
    }
    out
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(&deflate(data));
    let (a, b) = data.iter().fold((1u32, 0u32), |(a, b), &x| {
        let a = (a + u32::from(x)) % 65_521;
        (a, (b + a) % 65_521)
    });
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

struct Bits {
    out: Vec<u8>,
    acc: u32,
    used: u32,
}

impl Bits {
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= value << self.used;
        self.used += count;
        while self.used >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.used -= 8;
        }
    }

    /// A Huffman code, which deflate writes from its first bit.
    fn code(&mut self, code: u32, len: u32) {
        self.put(code.reverse_bits() >> (32 - len), len);
    }

    fn literal(&mut self, symbol: u32) {
        match symbol {
            0..=143 => self.code(0x30 + symbol, 8),
            144..=255 => self.code(0x190 + symbol - 144, 9),
            256..=279 => self.code(symbol - 256, 7),
            _ => self.code(0xC0 + symbol - 280, 8),
        }
    }
}

const LENGTH_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const WINDOW: usize = 32_768;
const MAX_MATCH: usize = 258;
const CHAIN: usize = 64;

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut bits = Bits {
        out: Vec::new(),
        acc: 0,
        used: 0,
    };
    bits.put(1, 1);
    bits.put(1, 2);
    let key = |i: usize| {
        (usize::from(data[i]) << 16 | usize::from(data[i + 1]) << 8 | usize::from(data[i + 2]))
            % 65_521
    };
    let mut head = vec![usize::MAX; 65_521];
    let mut prev = vec![usize::MAX; data.len()];
    let insert = |i: usize, head: &mut [usize], prev: &mut [usize]| {
        if i + 2 < data.len() {
            let k = key(i);
            prev[i] = head[k];
            head[k] = i;
        }
    };
    let mut i = 0;
    while i < data.len() {
        let mut best = (0usize, 0usize);
        if i + 2 < data.len() {
            let mut at = head[key(i)];
            for _ in 0..CHAIN {
                if at == usize::MAX || i - at > WINDOW {
                    break;
                }
                let len = data[at..]
                    .iter()
                    .zip(&data[i..])
                    .take(MAX_MATCH)
                    .take_while(|(a, b)| a == b)
                    .count();
                if len > best.0 {
                    best = (len, i - at);
                }
                at = prev[at];
            }
        }
        if best.0 >= 3 {
            let (len, dist) = (best.0 as u32, best.1 as u32);
            let l = LENGTH_BASE.iter().rposition(|&b| b <= len).unwrap_or(0);
            bits.literal(257 + l as u32);
            bits.put(len - LENGTH_BASE[l], LENGTH_EXTRA[l]);
            let d = DIST_BASE.iter().rposition(|&b| b <= dist).unwrap_or(0);
            bits.code(d as u32, 5);
            bits.put(dist - DIST_BASE[d], DIST_EXTRA[d]);
            for j in i..i + best.0 {
                insert(j, &mut head, &mut prev);
            }
            i += best.0;
        } else {
            bits.literal(u32::from(data[i]));
            insert(i, &mut head, &mut prev);
            i += 1;
        }
    }
    bits.literal(256);
    if bits.used > 0 {
        bits.out.push(bits.acc as u8);
    }
    bits.out
}
