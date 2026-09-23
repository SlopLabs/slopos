//! PEM (RFC 7468) certificate blocks.

use alloc::vec::Vec;

fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Standard base64, padding optional; whitespace is skipped.
pub fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    let mut padding = 0usize;
    for &c in text {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None;
        }
        acc = (acc << 6) | u32::from(base64_value(c)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    (padding <= 2 && acc & ((1 << bits) - 1) == 0).then_some(out)
}

/// The DER of every `CERTIFICATE` block, in order; `None` for a block whose
/// base64 does not decode.
pub fn certificates(pem: &[u8]) -> Vec<Option<Vec<u8>>> {
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    const END: &[u8] = b"-----END CERTIFICATE-----";
    let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(start) = find(rest, BEGIN) {
        let body = &rest[start + BEGIN.len()..];
        let Some(end) = find(body, END) else {
            break;
        };
        out.push(base64_decode(&body[..end]));
        rest = &body[end + END.len()..];
    }
    out
}

pub fn encode_certificate(der: &[u8]) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::from(&b"-----BEGIN CERTIFICATE-----\n"[..]);
    let mut line = 0;
    for chunk in der.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            let c = if i <= chunk.len() {
                ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]
            } else {
                b'='
            };
            out.push(c);
        }
        line += 4;
        if line == 64 {
            out.push(b'\n');
            line = 0;
        }
    }
    if line != 0 {
        out.push(b'\n');
    }
    out.extend_from_slice(b"-----END CERTIFICATE-----\n");
    out
}
