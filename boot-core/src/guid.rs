//! GUIDs in `EFI_GUID`'s in-memory layout, the one GPT entries, device paths
//! and variable namespaces store: the first three fields little-endian, the
//! last eight bytes in order.

use core::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Guid(pub [u8; 16]);

/// Which byte of the stored GUID each hex pair of the registry spelling
/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` names.
const SPELLING_ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
const DASHES: [usize; 4] = [8, 13, 18, 23];
const SPELLING_LEN: usize = 36;

const fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

const fn is_dash_at(at: usize) -> bool {
    at == DASHES[0] || at == DASHES[1] || at == DASHES[2] || at == DASHES[3]
}

impl Guid {
    pub const ZERO: Guid = Guid([0; 16]);

    /// The registry spelling, in either case.
    pub const fn parse(text: &str) -> Option<Guid> {
        let s = text.as_bytes();
        if s.len() != SPELLING_LEN {
            return None;
        }
        let mut digits = [0u8; 32];
        let (mut at, mut n) = (0, 0);
        while at < SPELLING_LEN {
            if is_dash_at(at) {
                if s[at] != b'-' {
                    return None;
                }
            } else {
                match hex_value(s[at]) {
                    Some(v) => digits[n] = v,
                    None => return None,
                }
                n += 1;
            }
            at += 1;
        }
        let mut out = [0u8; 16];
        let mut pair = 0;
        while pair < 16 {
            out[SPELLING_ORDER[pair]] = digits[2 * pair] << 4 | digits[2 * pair + 1];
            pair += 1;
        }
        Some(Guid(out))
    }

    /// [`Guid::parse`] for a constant, refused at compile time if malformed.
    pub const fn from_spelling(text: &str) -> Guid {
        match Guid::parse(text) {
            Some(guid) => guid,
            None => panic!("not a GUID spelling"),
        }
    }

    pub const fn is_zero(&self) -> bool {
        let mut i = 0;
        while i < 16 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// The registry spelling, lowercase, as Linux names a partition.
    pub fn spelling(&self) -> [u8; SPELLING_LEN] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = [b'-'; SPELLING_LEN];
        let mut at = 0;
        for &byte in &SPELLING_ORDER {
            if is_dash_at(at) {
                at += 1;
            }
            let value = self.0[byte];
            out[at] = HEX[usize::from(value >> 4)];
            out[at + 1] = HEX[usize::from(value & 0xF)];
            at += 2;
        }
        out
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = self.spelling();
        f.write_str(core::str::from_utf8(&spelling).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn the_esp_type_round_trips_through_its_stored_layout() {
        let esp = Guid::from_spelling("C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
        assert_eq!(
            esp.0,
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b
            ]
        );
        assert_eq!(esp.to_string(), "c12a7328-f81f-11d2-ba4b-00a0c93ec93b");
    }

    #[test]
    fn a_misplaced_dash_or_a_stray_character_is_no_guid() {
        assert!(Guid::parse("c12a7328f-81f-11d2-ba4b-00a0c93ec93b").is_none());
        assert!(Guid::parse("c12a7328-f81f-11d2-ba4b-00a0c93ec93").is_none());
        assert!(Guid::parse("g12a7328-f81f-11d2-ba4b-00a0c93ec93b").is_none());
        assert!(Guid::parse("").is_none());
    }

    #[test]
    fn zero_is_recognised() {
        assert!(Guid::ZERO.is_zero());
        assert!(!Guid::from_spelling("00000000-0000-0000-0000-000000000001").is_zero());
    }
}
