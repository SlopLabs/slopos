//! String descriptors (USB 2.0 §9.6.7): index 0 lists languages, the rest are
//! UTF-16LE.

use super::descriptor::kind;

/// US English, which a device that lists no language is asked in.
pub const ENGLISH: u16 = 0x0409;

pub const MAX_LEN: u16 = 255;
pub const MAX_CHARS: usize = (MAX_LEN as usize - 2) / 2;

fn body(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 2 || bytes[1] != kind::STRING {
        return None;
    }
    let len = usize::from(bytes[0]).min(bytes.len());
    bytes.get(2..len)
}

/// The language strings are asked in, from what index 0 answered, as Linux
/// takes it: `None` to ask for no strings at all.
pub fn language(answer: &[u8]) -> Option<u16> {
    match *answer {
        [_, ty, ..] if ty != kind::STRING => Some(ENGLISH),
        [length, ..] => match usize::from(length).min(answer.len()) & !1 {
            0 => None,
            2 => Some(ENGLISH),
            _ => Some(u16::from_le_bytes([answer[2], answer[3]])),
        },
        [] => None,
    }
}

/// The characters as Linux decodes a string descriptor: up to U+0000, an
/// unpaired surrogate skipped and a high one last ending the text.
fn chars(bytes: &[u8]) -> Option<impl Iterator<Item = char> + '_> {
    let mut units = body(bytes)?
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .peekable();
    let next = core::iter::from_fn(move || {
        loop {
            let unit = units.next()?;
            let scalar = match unit {
                0 => return None,
                0xd800..=0xdbff => match units.peek().copied() {
                    None => return None,
                    Some(low @ 0xdc00..=0xdfff) => {
                        units.next();
                        0x10000 + ((u32::from(unit) & 0x3ff) << 10) + (u32::from(low) & 0x3ff)
                    }
                    Some(_) => continue,
                },
                0xdc00..=0xdfff => continue,
                _ => u32::from(unit),
            };
            if let Some(c) = char::from_u32(scalar) {
                return Some(c);
            }
        }
    });
    Some(next.fuse())
}

/// The text in UTF-8 as Linux holds it; the bytes written, cut off where
/// `out` is full.
pub fn utf8(bytes: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut written = 0;
    for c in chars(bytes)? {
        for &byte in c.encode_utf8(&mut [0; 4]).as_bytes() {
            let Some(slot) = out.get_mut(written) else {
                return Some(written);
            };
            *slot = byte;
            written += 1;
        }
    }
    Some(written)
}

/// The text as `usb_id` takes a serial number: none at all when a character
/// is below 0x20, above 0x7f or a `,`, or when `out` cannot hold it.
pub fn identity(bytes: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut written = 0;
    for c in chars(bytes)? {
        let byte = u8::try_from(c)
            .ok()
            .filter(|&b| (0x20..=0x7f).contains(&b) && b != b',')?;
        *out.get_mut(written)? = byte;
        written += 1;
    }
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_read_within_blength() {
        let text = [12, 3, b'Q', 0, b'E', 0, b'M', 0, b'U', 0, 0xe9, 0, b'x', 0];
        let mut out = [0u8; 16];
        let n = utf8(&text, &mut out).unwrap();
        assert_eq!(&out[..n], "QEMU\u{e9}".as_bytes());
        let mut short = [0u8; 2];
        assert_eq!(utf8(&text, &mut short), Some(2));
        assert_eq!(utf8(&[5, 3, b'A', 0, b'B'], &mut out), Some(1));
        assert_eq!(utf8(&[200, 3, b'A', 0], &mut out), Some(1));
        for len in 0..text.len() {
            let _ = language(&text[..len]);
            let _ = utf8(&text[..len], &mut out);
            let _ = identity(&text[..len], &mut out);
        }
    }

    fn utf16(text: &[u16]) -> std::vec::Vec<u8> {
        let mut bytes = std::vec![(2 + 2 * text.len()) as u8, 3];
        for unit in text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn identified(text: &[u16]) -> Option<std::vec::Vec<u8>> {
        let mut out = [0u8; MAX_CHARS];
        let len = identity(&utf16(text), &mut out)?;
        Some(out[..len].to_vec())
    }

    #[test]
    fn identity_takes_a_serial_as_usb_id_does() {
        let ascii = |s: &str| s.bytes().map(u16::from).collect::<std::vec::Vec<_>>();
        assert_eq!(
            identified(&ascii("slopos-usb")).as_deref(),
            Some(&b"slopos-usb"[..])
        );
        assert_eq!(
            identified(&ascii(" A~\x7f")).as_deref(),
            Some(&b" A~\x7f"[..])
        );
        assert_eq!(identified(&ascii("AB\0CD")).as_deref(), Some(&b"AB"[..]));
        assert_eq!(identified(&[]).as_deref(), Some(&b""[..]));
        for refused in ["A,B", "A\x1fB", "A\tB"] {
            assert_eq!(identified(&ascii(refused)), None, "{refused:?}");
        }
        for unit in [0x80, 0xe9, 0x100, 0xfffe] {
            assert_eq!(identified(&[u16::from(b'A'), unit]), None, "{unit:#x}");
        }
        assert_eq!(
            identified(&[u16::from(b'A'), 0xd83d, 0xde00]),
            None,
            "a paired surrogate is a character past 0x7f"
        );
        assert_eq!(identified(&[0x41, 0xd800]).as_deref(), Some(&b"A"[..]));
        assert_eq!(
            identified(&[0x41, 0xd800, 0x42, 0xdc00, 0x43]).as_deref(),
            Some(&b"ABC"[..]),
            "unpaired surrogates are skipped"
        );
        let longest = std::vec![u16::from(b'S'); MAX_CHARS];
        assert_eq!(identified(&longest).map(|s| s.len()), Some(MAX_CHARS));
        let mut short = [0u8; 2];
        assert_eq!(identity(&utf16(&ascii("ABC")), &mut short), None);
        assert_eq!(identity(&[6, 2, b'A', 0, b'B', 0], &mut short), None);
        assert_eq!(identity(&[5, 3, b'A', 0, b'B'], &mut short), Some(1));
    }

    #[test]
    fn utf8_decodes_as_linux_does() {
        let mut out = [0u8; 16];
        let mut decoded = |text: &[u16]| {
            let len = utf8(&utf16(text), &mut out).unwrap();
            out[..len].to_vec()
        };
        assert_eq!(decoded(&[0x43, 0x61, 0x66, 0xe9]), "Café".as_bytes());
        assert_eq!(decoded(&[0xd83d, 0xde00]), "\u{1f600}".as_bytes());
        assert_eq!(decoded(&[0x41, 0xdc00, 0xd800, 0x42]), b"AB");
        assert_eq!(decoded(&[0x41, 0, 0x42]), b"A");
        assert_eq!(decoded(&[0x41, 0xd800]), b"A");
        let mut cut = [0u8; 4];
        let len = utf8(&utf16(&[0x41, 0x42, 0x43, 0xe9]), &mut cut).unwrap();
        assert_eq!(&cut[..len], b"ABC\xc3", "cut at the byte, as udev reads it");
        assert_eq!(utf8(&[4, 2, 0x41, 0], &mut cut), None);
    }

    #[test]
    fn language_is_taken_as_linux_takes_it() {
        assert_eq!(language(&[4, 3, 0x07, 0x04]), Some(0x0407));
        assert_eq!(language(&[6, 3, 0x07, 0x04, 0x09, 0x04]), Some(0x0407));
        assert_eq!(language(&[4, 2, 0x07, 0x04]), Some(ENGLISH), "not a string");
        assert_eq!(language(&[2, 3]), Some(ENGLISH));
        assert_eq!(language(&[3, 3, 0x07]), Some(ENGLISH));
        assert_eq!(language(&[200, 3, 0x07]), Some(ENGLISH));
        assert_eq!(language(&[]), None);
        assert_eq!(language(&[4]), None);
        assert_eq!(language(&[1, 3, 0x07, 0x04]), None);
        assert_eq!(language(&[0, 3, 0x07, 0x04]), None);
    }
}
