//! String descriptors (USB 2.0 §9.6.7): index 0 lists languages, the rest are
//! UTF-16LE.

use super::descriptor::kind;

/// US English, which a device that lists no language is asked in.
pub const ENGLISH: u16 = 0x0409;

pub const MAX_LEN: u16 = 255;

fn body(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 2 || bytes[1] != kind::STRING {
        return None;
    }
    let len = usize::from(bytes[0]).min(bytes.len());
    bytes.get(2..len)
}

/// The first language string index 0 lists.
pub fn first_language(bytes: &[u8]) -> Option<u16> {
    let body = body(bytes)?;
    (body.len() >= 2).then(|| u16::from_le_bytes([body[0], body[1]]))
}

/// The text as printable ASCII, `?` for anything else; returns bytes written.
pub fn decode(bytes: &[u8], out: &mut [u8]) -> usize {
    let Some(body) = body(bytes) else {
        return 0;
    };
    let mut written = 0;
    for (pair, slot) in body.chunks_exact(2).zip(out.iter_mut()) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        *slot = match u8::try_from(unit) {
            Ok(c) if (0x20..0x7f).contains(&c) => c,
            _ => b'?',
        };
        written += 1;
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn languages_and_text_decode_within_blength() {
        assert_eq!(first_language(&[4, 3, 0x09, 0x04]), Some(ENGLISH));
        assert_eq!(first_language(&[2, 3]), None);
        assert_eq!(first_language(&[4, 2, 0x09, 0x04]), None);
        let text = [12, 3, b'Q', 0, b'E', 0, b'M', 0, b'U', 0, 0xe9, 0, b'x', 0];
        let mut out = [0u8; 16];
        let n = decode(&text, &mut out);
        assert_eq!(&out[..n], b"QEMU?");
        let mut short = [0u8; 2];
        assert_eq!(decode(&text, &mut short), 2);
        let odd = [5, 3, b'A', 0, b'B'];
        assert_eq!(decode(&odd, &mut out), 1);
        let lying = [200, 3, b'A', 0];
        assert_eq!(decode(&lying, &mut out), 1);
        for len in 0..text.len() {
            let _ = decode(&text[..len], &mut out);
            let _ = first_language(&text[..len]);
        }
    }
}
