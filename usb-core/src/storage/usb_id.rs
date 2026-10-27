//! What `usb_id` names a stick's vendor and product by.

use crate::device::string;

/// How much of a device string `usb_id` reads into a name.
pub const STRING_MAX: usize = 63;

/// Whether an INQUIRY field, as [`super::scsi::Inquiry`] holds it, is blank.
pub fn blank(field: &[u8]) -> bool {
    field.iter().all(|&b| b == b' ')
}

/// The vendor or product part: the INQUIRY `field` unless it is blank; else
/// the device's string `descriptor` as Linux decodes it, cut at
/// [`STRING_MAX`] bytes, unless it is absent, unreadable or empty; else `id`
/// in hex.
pub fn part<'a>(
    field: &'a [u8],
    descriptor: Option<&[u8]>,
    id: u16,
    out: &'a mut [u8; STRING_MAX],
) -> &'a [u8] {
    if !blank(field) {
        return field;
    }
    match descriptor.and_then(|bytes| string::utf8(bytes, out)) {
        Some(len) if len > 0 => &out[..len],
        _ => {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            for (i, slot) in out[..4].iter_mut().enumerate() {
                *slot = DIGITS[usize::from(id >> (12 - 4 * i) & 0xf)];
            }
            &out[..4]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::scsi::Inquiry;
    use std::vec::Vec;

    fn utf16(text: &[u16]) -> Vec<u8> {
        let mut bytes = std::vec![(2 + 2 * text.len()) as u8, 3];
        for unit in text {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }

    fn ascii(text: &str) -> Vec<u16> {
        text.bytes().map(u16::from).collect()
    }

    fn vendor(field: &[u8], descriptor: Option<&[u8]>) -> Vec<u8> {
        let mut answer = [0u8; 36];
        answer[8..8 + field.len()].copy_from_slice(field);
        let inquiry = Inquiry::parse(&answer).unwrap();
        let mut out = [0u8; STRING_MAX];
        part(&inquiry.vendor, descriptor, 0x46f4, &mut out).to_vec()
    }

    #[test]
    fn a_field_inquiry_fills_names_the_part() {
        let acme = utf16(&ascii("Other"));
        assert_eq!(vendor(b"QEMU", Some(&acme)), b"QEMU    ");
        assert_eq!(vendor(b"ACME\x7f", Some(&acme)), b"ACME    ");
        assert_eq!(vendor(b"AB\x01 CD", Some(&acme)), b"AB  CD  ");
        assert_eq!(vendor("Café".as_bytes(), Some(&acme)), b"Caf     ");
    }

    #[test]
    fn a_blank_field_takes_the_device_string_then_the_id() {
        let named = utf16(&[0x20, 0x41, 0xd800, 0x43, 0x6d, 0xe9, 0x20]);
        assert_eq!(vendor(&[0xff; 8], Some(&named)), " ACm\u{e9} ".as_bytes());
        assert_eq!(vendor(b"\0QEMU", Some(&utf16(&ascii("  ")))), b"  ");
        assert_eq!(vendor(&[0xff; 8], None), b"46f4");
        assert_eq!(vendor(&[0xff; 8], Some(&utf16(&[]))), b"46f4");
        assert_eq!(vendor(&[0xff; 8], Some(&utf16(&[0, 0x41]))), b"46f4");
        assert_eq!(vendor(&[0xff; 8], Some(&[4, 2, 0x41, 0])), b"46f4");
        assert_eq!(vendor(&[0xff; 8], Some(&utf16(&[0xd800]))), b"46f4");
    }

    #[test]
    fn a_device_string_is_cut_at_63_bytes() {
        let long = utf16(&ascii(&"x".repeat(80)));
        assert_eq!(vendor(&[], Some(&long)), "x".repeat(63).as_bytes());
        let mut text = ascii(&"y".repeat(62));
        text.push(0xe9);
        let cut = vendor(&[], Some(&utf16(&text)));
        assert_eq!(cut.len(), 63);
        assert_eq!(cut[62], 0xc3, "cut inside a character, as udev reads it");
    }
}
