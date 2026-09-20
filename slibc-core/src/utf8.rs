//! Restartable UTF-8 ↔ UTF-32 conversion.
//!
//! The entire conversion state is one accumulator plus two counters, which is
//! what lets it live in the eight bytes C gives `mbstate_t` and survive a
//! round trip through one.

/// A partially decoded character: `acc` holds the scalar bits gathered so far,
/// `seen` the bytes consumed and `need` the sequence's total length.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct MbState {
    pub acc: u32,
    pub seen: u8,
    pub need: u8,
}

impl MbState {
    #[inline]
    pub fn is_initial(&self) -> bool {
        self.need == 0
    }

    /// Reads back a state stored in an `mbstate_t`. A combination this encoder
    /// cannot produce means the caller handed over an uninitialised object, so
    /// it reads as initial rather than as a half-decoded character.
    #[inline]
    pub fn from_raw(raw: [u32; 2]) -> Self {
        let seen = (raw[1] & 0xff) as u8;
        let need = ((raw[1] >> 8) & 0xff) as u8;
        if !(2..=4).contains(&need) || seen == 0 || seen >= need {
            return Self::default();
        }
        // The bytes to come add `6 * (need - seen)` low bits, so the sequence
        // must finish in `acc << shift ..= (acc << shift) | mask`: reachable
        // exactly when that window holds a scalar of this length.
        let shift = 6 * (need - seen) as u32;
        let mask = (1u32 << shift) - 1;
        let (lo, hi) = match need {
            2 => (0x80u32, 0x7ffu32),
            3 => (0x800, 0xffff),
            _ => (0x1_0000, 0x10_ffff),
        };
        let acc = raw[0];
        if acc < (lo >> shift) || acc > (hi >> shift) {
            return Self::default();
        }
        if need == 3 && (acc << shift) >= 0xd800 && ((acc << shift) | mask) <= 0xdfff {
            return Self::default();
        }
        Self { acc, seen, need }
    }

    #[inline]
    pub fn to_raw(self) -> [u32; 2] {
        [self.acc, (self.seen as u32) | ((self.need as u32) << 8)]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Done(u32),
    More,
    Invalid,
}

/// Feeds one byte. `Invalid` leaves `state` initial, so the same state is
/// usable for the next character without the caller clearing it.
pub fn decode_step(state: &mut MbState, byte: u8) -> Step {
    if state.need == 0 {
        let (acc, need) = match byte {
            0x00..=0x7f => return Step::Done(byte as u32),
            0xc2..=0xdf => ((byte & 0x1f) as u32, 2),
            0xe0..=0xef => ((byte & 0x0f) as u32, 3),
            0xf0..=0xf4 => ((byte & 0x07) as u32, 4),
            _ => return Step::Invalid,
        };
        *state = MbState { acc, seen: 1, need };
        return Step::More;
    }

    if byte & 0xc0 != 0x80 {
        *state = MbState::default();
        return Step::Invalid;
    }

    // A three- or four-byte lead cannot say on its own whether the sequence is
    // overlong, a surrogate or past U+10FFFF; its first continuation byte can,
    // which keeps the rejection on the byte that caused it.
    if state.seen == 1 {
        let rejected = match state.need {
            3 => (state.acc == 0x0 && byte < 0xa0) || (state.acc == 0xd && byte >= 0xa0),
            4 => (state.acc == 0x0 && byte < 0x90) || (state.acc == 0x4 && byte >= 0x90),
            _ => false,
        };
        if rejected {
            *state = MbState::default();
            return Step::Invalid;
        }
    }

    state.acc = (state.acc << 6) | ((byte & 0x3f) as u32);
    state.seen += 1;
    if state.seen == state.need {
        let scalar = state.acc;
        *state = MbState::default();
        Step::Done(scalar)
    } else {
        Step::More
    }
}

/// Encodes one scalar value, answering its byte count. `None` for a surrogate
/// or anything above U+10FFFF.
pub fn encode(cp: u32, out: &mut [u8; 4]) -> Option<usize> {
    match cp {
        0x0000..=0x007f => {
            out[0] = cp as u8;
            Some(1)
        }
        0x0080..=0x07ff => {
            out[0] = 0xc0 | (cp >> 6) as u8;
            out[1] = 0x80 | (cp & 0x3f) as u8;
            Some(2)
        }
        0x0800..=0xffff => {
            if (0xd800..=0xdfff).contains(&cp) {
                return None;
            }
            out[0] = 0xe0 | (cp >> 12) as u8;
            out[1] = 0x80 | ((cp >> 6) & 0x3f) as u8;
            out[2] = 0x80 | (cp & 0x3f) as u8;
            Some(3)
        }
        0x1_0000..=0x10_ffff => {
            out[0] = 0xf0 | (cp >> 18) as u8;
            out[1] = 0x80 | ((cp >> 12) & 0x3f) as u8;
            out[2] = 0x80 | ((cp >> 6) & 0x3f) as u8;
            out[3] = 0x80 | (cp & 0x3f) as u8;
            Some(4)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(cp: u32) -> usize {
        let mut buf = [0u8; 4];
        let len = encode(cp, &mut buf).expect("boundary scalar must encode");
        let mut state = MbState::default();
        for (i, byte) in buf[..len].iter().enumerate() {
            match decode_step(&mut state, *byte) {
                Step::Done(got) => {
                    assert_eq!(i + 1, len, "U+{cp:04X} completed before its last byte");
                    assert_eq!(got, cp);
                    assert!(state.is_initial());
                    return len;
                }
                Step::More => assert!(!state.is_initial()),
                Step::Invalid => panic!("U+{cp:04X} rejected its own encoding"),
            }
        }
        panic!("U+{cp:04X} never completed");
    }

    /// How the run settles, and after how many bytes.
    fn feed(bytes: &[u8]) -> (Step, usize) {
        let mut state = MbState::default();
        for (i, byte) in bytes.iter().enumerate() {
            match decode_step(&mut state, *byte) {
                Step::More => {}
                settled => return (settled, i + 1),
            }
        }
        (Step::More, bytes.len())
    }

    #[test]
    fn boundary_scalars_round_trip() {
        assert_eq!(round_trip(0x0000), 1);
        assert_eq!(round_trip(0x007f), 1);
        assert_eq!(round_trip(0x0080), 2);
        assert_eq!(round_trip(0x07ff), 2);
        assert_eq!(round_trip(0x0800), 3);
        assert_eq!(round_trip(0xffff), 3);
        assert_eq!(round_trip(0x1_0000), 4);
        assert_eq!(round_trip(0x10_ffff), 4);
    }

    #[test]
    fn encodes_known_vectors() {
        let mut buf = [0u8; 4];
        assert_eq!(encode(0x24, &mut buf), Some(1));
        assert_eq!(&buf[..1], &[0x24]);
        assert_eq!(encode(0xa3, &mut buf), Some(2));
        assert_eq!(&buf[..2], &[0xc2, 0xa3]);
        assert_eq!(encode(0x20ac, &mut buf), Some(3));
        assert_eq!(&buf[..3], &[0xe2, 0x82, 0xac]);
        assert_eq!(encode(0x1_0348, &mut buf), Some(4));
        assert_eq!(&buf[..4], &[0xf0, 0x90, 0x8d, 0x88]);
    }

    #[test]
    fn continuation_byte_in_initial_state_is_invalid() {
        assert_eq!(feed(&[0x80]), (Step::Invalid, 1));
        assert_eq!(feed(&[0xbf]), (Step::Invalid, 1));
    }

    #[test]
    fn overlong_encodings_are_invalid() {
        assert_eq!(feed(&[0xc0, 0xaf]), (Step::Invalid, 1));
        assert_eq!(feed(&[0xc1, 0xbf]), (Step::Invalid, 1));
        assert_eq!(feed(&[0xe0, 0x80, 0xaf]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xe0, 0x9f, 0xbf]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xf0, 0x80, 0x80, 0xaf]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xf0, 0x8f, 0xbf, 0xbf]), (Step::Invalid, 2));
    }

    #[test]
    fn surrogates_are_invalid() {
        assert_eq!(feed(&[0xed, 0xa0, 0x80]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xed, 0xbf, 0xbf]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xed, 0x9f, 0xbf]), (Step::Done(0xd7ff), 3));
        let mut buf = [0u8; 4];
        assert_eq!(encode(0xd800, &mut buf), None);
        assert_eq!(encode(0xdfff, &mut buf), None);
    }

    #[test]
    fn scalars_past_the_last_plane_are_invalid() {
        assert_eq!(feed(&[0xf4, 0x90, 0x80, 0x80]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xf5, 0x80, 0x80, 0x80]), (Step::Invalid, 1));
        assert_eq!(feed(&[0xff]), (Step::Invalid, 1));
        assert_eq!(feed(&[0xfe]), (Step::Invalid, 1));
        let mut buf = [0u8; 4];
        assert_eq!(encode(0x11_0000, &mut buf), None);
        assert_eq!(encode(u32::MAX, &mut buf), None);
    }

    #[test]
    fn truncated_sequence_is_invalid_at_the_bad_byte() {
        assert_eq!(feed(&[0xe2, 0x28, 0xa1]), (Step::Invalid, 2));
        assert_eq!(feed(&[0xf0, 0x9f, 0x92]), (Step::More, 3));
        assert_eq!(feed(&[0xc2]), (Step::More, 1));
    }

    #[test]
    fn invalid_leaves_the_state_usable() {
        let mut state = MbState::default();
        assert_eq!(decode_step(&mut state, 0xe2), Step::More);
        assert_eq!(decode_step(&mut state, 0x41), Step::Invalid);
        assert!(state.is_initial());
        assert_eq!(decode_step(&mut state, 0x41), Step::Done(0x41));
    }

    #[test]
    fn state_carries_a_sequence_split_across_runs() {
        let mut first = MbState::default();
        assert_eq!(decode_step(&mut first, 0xf0), Step::More);
        assert_eq!(decode_step(&mut first, 0x9f), Step::More);

        let mut second = MbState::from_raw(first.to_raw());
        assert!(!second.is_initial());
        assert_eq!(decode_step(&mut second, 0x92), Step::More);
        assert_eq!(decode_step(&mut second, 0xa9), Step::Done(0x1_f4a9));
        assert!(second.is_initial());
        assert_eq!(second.to_raw(), [0, 0]);
    }

    #[test]
    fn raw_round_trips_partial_states() {
        assert_eq!(MbState::default().to_raw(), [0, 0]);
        assert!(MbState::from_raw([0, 0]).is_initial());

        for prefix in [
            &[0xc2u8][..],
            &[0xe2][..],
            &[0xe2, 0x82][..],
            &[0xf0][..],
            &[0xf0, 0x9f][..],
            &[0xf0, 0x9f, 0x92][..],
        ] {
            let mut state = MbState::default();
            for byte in prefix {
                assert_eq!(decode_step(&mut state, *byte), Step::More);
            }
            assert_eq!(MbState::from_raw(state.to_raw()), state);
        }

        assert!(MbState::from_raw([0xdead_beef, 0xffff_ffff]).is_initial());
        assert!(MbState::from_raw([0, 0x0000_0101]).is_initial());
    }

    /// Whether some run of continuation bytes finishes `state` as a scalar
    /// that really is `need` bytes long.
    fn completes(state: MbState) -> bool {
        let mut buf = [0u8; 4];
        for tail in 0x80u8..=0xbf {
            let mut probe = state;
            match decode_step(&mut probe, tail) {
                Step::Done(cp) => {
                    if encode(cp, &mut buf) == Some(state.need as usize) {
                        return true;
                    }
                }
                Step::More => {
                    if completes(probe) {
                        return true;
                    }
                }
                Step::Invalid => {}
            }
        }
        false
    }

    #[test]
    fn raw_accumulators_no_encoder_produces_read_as_initial() {
        // `mbstate_t` is the caller's object, so the counters can be a state
        // the encoder reaches while `acc` is anything at all.
        let seen_2_of_3 = 2 | (3 << 8);
        let seen_3_of_4 = 3 | (4 << 8);
        let seen_2_of_4 = 2 | (4 << 8);

        // One 0x80 away from U+D800, U+FFFFFFC0 and U+110000.
        assert!(MbState::from_raw([0x360, seen_2_of_3]).is_initial());
        assert!(MbState::from_raw([0xffff_ffff, seen_3_of_4]).is_initial());
        assert!(MbState::from_raw([0x110, seen_2_of_4]).is_initial());
        // Accumulators only an overlong sequence would carry.
        assert!(MbState::from_raw([0x1, 1 | (2 << 8)]).is_initial());
        assert!(MbState::from_raw([0x1f, seen_2_of_3]).is_initial());

        // The neighbours the encoder does reach, including the 0xed lead whose
        // surrogate rejection belongs to its next byte rather than to `acc`.
        assert!(!MbState::from_raw([0xd, 1 | (3 << 8)]).is_initial());
        assert!(!MbState::from_raw([0x340, seen_2_of_3]).is_initial());
        assert!(!MbState::from_raw([0x4, 1 | (4 << 8)]).is_initial());
        assert!(!MbState::from_raw([0x10f, seen_2_of_4]).is_initial());
    }

    #[test]
    fn from_raw_accepts_exactly_the_reachable_states() {
        for need in 2u8..=4 {
            for seen in 1..need {
                let shift = 6 * (need - seen) as u32;
                for acc in 0..=(0x10_ffffu32 >> shift) {
                    let raw = [acc, (seen as u32) | ((need as u32) << 8)];
                    let state = MbState::from_raw(raw);
                    assert_eq!(
                        !state.is_initial(),
                        completes(MbState { acc, seen, need }),
                        "acc {acc:#x}, {seen} of {need} bytes"
                    );
                    if !state.is_initial() {
                        assert_eq!(state.to_raw(), raw);
                    }
                }
            }
        }
    }
}
