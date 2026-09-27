//! The codesets `iconv` (POSIX `<iconv.h>`) converts between, one character
//! at a time: every conversion goes through a Unicode scalar value.
//!
//! All of them are stateless, so a conversion descriptor carries nothing but
//! its two codesets and a shift-state reset has nothing to reset.

use crate::utf8::{self, MbState, Step};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
    Utf8,
    Ascii,
    Latin1,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

/// The codeset a name spells. Case and the `-`/`_` separators are ignored,
/// so `utf8`, `UTF-8` and `Utf_8` are one name. `WCHAR_T` is this target's
/// 32-bit little-endian `wchar_t`.
pub fn charset(name: &[u8]) -> Option<Charset> {
    let mut key = [0u8; 24];
    let mut len = 0;
    for &b in name {
        if b == b'-' || b == b'_' {
            continue;
        }
        *key.get_mut(len)? = b.to_ascii_uppercase();
        len += 1;
    }
    Some(match &key[..len] {
        b"UTF8" => Charset::Utf8,
        b"ASCII" | b"USASCII" | b"ANSIX3.41968" | b"646" => Charset::Ascii,
        b"ISO88591" | b"LATIN1" | b"L1" => Charset::Latin1,
        b"UTF16LE" => Charset::Utf16Le,
        b"UTF16BE" => Charset::Utf16Be,
        b"UTF32LE" | b"WCHART" => Charset::Utf32Le,
        b"UTF32BE" => Charset::Utf32Be,
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// A scalar value and the bytes it took.
    Scalar(u32, usize),
    /// The input ends inside a character.
    Incomplete,
    /// The input is not a character of the codeset.
    Invalid,
}

pub fn decode(from: Charset, input: &[u8]) -> Decoded {
    let Some(&first) = input.first() else {
        return Decoded::Incomplete;
    };
    match from {
        Charset::Ascii if first < 0x80 => Decoded::Scalar(u32::from(first), 1),
        Charset::Ascii => Decoded::Invalid,
        Charset::Latin1 => Decoded::Scalar(u32::from(first), 1),
        Charset::Utf8 => {
            let mut state = MbState::default();
            for (i, &byte) in input.iter().enumerate() {
                match utf8::decode_step(&mut state, byte) {
                    Step::Done(scalar) => return Decoded::Scalar(scalar, i + 1),
                    Step::Invalid => return Decoded::Invalid,
                    Step::More => {}
                }
            }
            Decoded::Incomplete
        }
        Charset::Utf16Le | Charset::Utf16Be => {
            let unit = |at: usize| -> Option<u32> {
                let pair = [*input.get(at)?, *input.get(at + 1)?];
                Some(u32::from(if from == Charset::Utf16Le {
                    u16::from_le_bytes(pair)
                } else {
                    u16::from_be_bytes(pair)
                }))
            };
            let Some(high) = unit(0) else {
                return Decoded::Incomplete;
            };
            match high {
                0xd800..=0xdbff => match unit(2) {
                    None => Decoded::Incomplete,
                    Some(low @ 0xdc00..=0xdfff) => {
                        Decoded::Scalar(0x1_0000 + ((high - 0xd800) << 10) + (low - 0xdc00), 4)
                    }
                    Some(_) => Decoded::Invalid,
                },
                0xdc00..=0xdfff => Decoded::Invalid,
                _ => Decoded::Scalar(high, 2),
            }
        }
        Charset::Utf32Le | Charset::Utf32Be => {
            let Some(bytes) = input.get(..4) else {
                return Decoded::Incomplete;
            };
            let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
            let scalar = if from == Charset::Utf32Le {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            if scalar > 0x10_ffff || (0xd800..=0xdfff).contains(&scalar) {
                Decoded::Invalid
            } else {
                Decoded::Scalar(scalar, 4)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoded {
    Wrote(usize),
    /// `out` has no room for the whole character.
    NoRoom,
    /// The codeset has no such character.
    Unrepresentable,
}

pub fn encode(to: Charset, scalar: u32, out: &mut [u8]) -> Encoded {
    if scalar > 0x10_ffff || (0xd800..=0xdfff).contains(&scalar) {
        return Encoded::Unrepresentable;
    }
    let mut buf = [0u8; 4];
    let len = match to {
        Charset::Ascii | Charset::Latin1 => {
            let limit = if to == Charset::Ascii { 0x80 } else { 0x100 };
            if scalar >= limit {
                return Encoded::Unrepresentable;
            }
            buf[0] = scalar as u8;
            1
        }
        Charset::Utf8 => match utf8::encode(scalar, &mut buf) {
            Some(len) => len,
            None => return Encoded::Unrepresentable,
        },
        Charset::Utf16Le | Charset::Utf16Be => {
            let units: [u16; 2];
            let count = if scalar >= 0x1_0000 {
                let v = scalar - 0x1_0000;
                units = [0xd800 + (v >> 10) as u16, 0xdc00 + (v & 0x3ff) as u16];
                2
            } else {
                units = [scalar as u16, 0];
                1
            };
            for (i, unit) in units[..count].iter().enumerate() {
                let bytes = if to == Charset::Utf16Le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                buf[i * 2..i * 2 + 2].copy_from_slice(&bytes);
            }
            count * 2
        }
        Charset::Utf32Le => {
            buf = scalar.to_le_bytes();
            4
        }
        Charset::Utf32Be => {
            buf = scalar.to_be_bytes();
            4
        }
    };
    match out.get_mut(..len) {
        Some(dst) => {
            dst.copy_from_slice(&buf[..len]);
            Encoded::Wrote(len)
        }
        None => Encoded::NoRoom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ignore_case_and_separators() {
        assert_eq!(charset(b"UTF-8"), Some(Charset::Utf8));
        assert_eq!(charset(b"utf8"), Some(Charset::Utf8));
        assert_eq!(charset(b"iso-8859-1"), Some(Charset::Latin1));
        assert_eq!(charset(b"UTF-16LE"), Some(Charset::Utf16Le));
        assert_eq!(charset(b"WCHAR_T"), Some(Charset::Utf32Le));
        assert_eq!(charset(b"EBCDIC-US"), None);
        assert_eq!(charset(b"UTF-8//TRANSLIT"), None);
        assert_eq!(charset(b"A-VERY-LONG-NAME-THAT-FITS-NO-KEY"), None);
    }

    #[test]
    fn utf8_decodes_one_character_and_says_why_it_stops() {
        assert_eq!(
            decode(Charset::Utf8, b"\xc3\xa9x"),
            Decoded::Scalar(0xe9, 2)
        );
        assert_eq!(decode(Charset::Utf8, b"\xf0\x9f\x98"), Decoded::Incomplete);
        assert_eq!(decode(Charset::Utf8, b"\xed\xa0\x80"), Decoded::Invalid);
        assert_eq!(decode(Charset::Utf8, b"\xc0\xaf"), Decoded::Invalid);
        assert_eq!(decode(Charset::Utf8, b""), Decoded::Incomplete);
    }

    #[test]
    fn utf16_pairs_surrogates_and_rejects_lone_ones() {
        assert_eq!(
            decode(Charset::Utf16Le, &[0x3d, 0xd8, 0x00, 0xde]),
            Decoded::Scalar(0x1_f600, 4)
        );
        assert_eq!(decode(Charset::Utf16Be, &[0xd8, 0x3d]), Decoded::Incomplete);
        assert_eq!(decode(Charset::Utf16Be, &[0xdc, 0x00]), Decoded::Invalid);
        assert_eq!(
            decode(Charset::Utf16Be, &[0xd8, 0x3d, 0x00, 0x41]),
            Decoded::Invalid
        );
        assert_eq!(decode(Charset::Utf16Be, &[0x00]), Decoded::Incomplete);
    }

    #[test]
    fn narrow_codesets_bound_what_they_hold() {
        assert_eq!(decode(Charset::Ascii, b"\x80"), Decoded::Invalid);
        assert_eq!(decode(Charset::Latin1, b"\xff"), Decoded::Scalar(0xff, 1));
        let mut out = [0u8; 4];
        assert_eq!(
            encode(Charset::Ascii, 0xe9, &mut out),
            Encoded::Unrepresentable
        );
        assert_eq!(encode(Charset::Latin1, 0xe9, &mut out), Encoded::Wrote(1));
        assert_eq!(out[0], 0xe9);
        assert_eq!(
            encode(Charset::Latin1, 0x100, &mut out),
            Encoded::Unrepresentable
        );
    }

    #[test]
    fn encoders_round_trip_and_need_room_for_the_whole_character() {
        for to in [
            Charset::Utf8,
            Charset::Utf16Le,
            Charset::Utf16Be,
            Charset::Utf32Le,
            Charset::Utf32Be,
        ] {
            for scalar in [0x41, 0xe9, 0x20ac, 0x1_f600, 0x10_ffff] {
                let mut out = [0u8; 4];
                let Encoded::Wrote(len) = encode(to, scalar, &mut out) else {
                    panic!("{to:?} could not encode {scalar:#x}");
                };
                assert_eq!(decode(to, &out[..len]), Decoded::Scalar(scalar, len));
                assert_eq!(encode(to, scalar, &mut out[..len - 1]), Encoded::NoRoom);
            }
        }
        let mut out = [0u8; 4];
        assert_eq!(
            encode(Charset::Utf8, 0xd800, &mut out),
            Encoded::Unrepresentable
        );
        assert_eq!(
            encode(Charset::Utf32Le, 0xdfff, &mut out),
            Encoded::Unrepresentable
        );
    }
}
