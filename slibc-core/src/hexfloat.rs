//! C99's hexadecimal floating constants, which `core`'s parser cannot read:
//! `0x1.8p1`, in `strtod`'s dialect of the grammar where the binary exponent
//! is optional rather than mandatory.
//!
//! A hexadecimal significand is exact in binary, so there is no big-integer
//! step: the digits accumulate into a `u64`, everything past its capacity
//! collapses into one sticky bit, and a single round-to-nearest-ties-to-even
//! lands on the target format. That format's significand width is a parameter
//! so `f32` rounds once instead of twice through `f64`.

/// A `scan_f64` result. `consumed` is how many bytes the subject sequence is.
#[derive(Clone, Copy, Debug)]
pub struct Hex64 {
    pub value: f64,
    pub consumed: usize,
    pub range_error: bool,
}

/// A `scan_f32` result. `consumed` is how many bytes the subject sequence is.
#[derive(Clone, Copy, Debug)]
pub struct Hex32 {
    pub value: f32,
    pub consumed: usize,
    pub range_error: bool,
}

/// Whether `bytes` opens a hexadecimal subject sequence, which is what
/// `scan_f64` and `scan_f32` require of their input.
pub fn is_hex_prefix(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == b'0' && matches!(bytes[1], b'x' | b'X')
}

/// `bytes` begins at the `0` of `0x`, so [`is_hex_prefix`] holds of it; the
/// caller owns the sign and applies it.
pub fn scan_f64(bytes: &[u8]) -> Hex64 {
    let scanned = scan(bytes, 53, 1023);
    Hex64 {
        value: f64::from_bits(scanned.bits),
        consumed: scanned.consumed,
        range_error: scanned.range_error,
    }
}

/// `bytes` begins at the `0` of `0x`, so [`is_hex_prefix`] holds of it; the
/// caller owns the sign and applies it.
pub fn scan_f32(bytes: &[u8]) -> Hex32 {
    let scanned = scan(bytes, 24, 127);
    Hex32 {
        value: f32::from_bits(scanned.bits as u32),
        consumed: scanned.consumed,
        range_error: scanned.range_error,
    }
}

struct Scan {
    bits: u64,
    consumed: usize,
    range_error: bool,
}

/// The largest accumulator that another digit cannot overflow, which leaves at
/// least 61 significand bits captured before anything becomes sticky.
const CAPACITY: u64 = 1 << 60;

fn hex_digit(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'f' => Some((byte - b'a') as u32 + 10),
        b'A'..=b'F' => Some((byte - b'A') as u32 + 10),
        _ => None,
    }
}

fn scan(bytes: &[u8], precision: u32, emax: i32) -> Scan {
    debug_assert!(is_hex_prefix(bytes));

    let mut mantissa: u64 = 0;
    let mut sticky = false;
    let mut bexp: i64 = 0;
    let mut digits = 0usize;
    let mut dot = false;
    let mut at = 2;

    while at < bytes.len() {
        let byte = bytes[at];
        if byte == b'.' && !dot {
            dot = true;
            at += 1;
            continue;
        }
        let Some(digit) = hex_digit(byte) else { break };
        digits += 1;
        if mantissa < CAPACITY {
            mantissa = mantissa * 16 + digit as u64;
            if dot {
                bexp -= 4;
            }
        } else {
            sticky |= digit != 0;
            if !dot {
                bexp += 4;
            }
        }
        at += 1;
    }

    // `0x` with no hex digit after it is not a hexadecimal sequence: the
    // subject is the `0` alone, and the caller resumes at the `x`.
    if digits == 0 {
        return Scan {
            bits: 0,
            consumed: 1,
            range_error: false,
        };
    }

    if at < bytes.len() && matches!(bytes[at], b'p' | b'P') {
        let mut cursor = at + 1;
        let negative = bytes.get(cursor) == Some(&b'-');
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            cursor += 1;
        }
        if matches!(bytes.get(cursor), Some(b'0'..=b'9')) {
            let mut value: i64 = 0;
            while let Some(byte @ b'0'..=b'9') = bytes.get(cursor).copied() {
                // Saturating, so a million-digit exponent cannot wrap back
                // into a finite result.
                value = value
                    .saturating_mul(10)
                    .saturating_add((byte - b'0') as i64);
                cursor += 1;
            }
            bexp = if negative {
                bexp.saturating_sub(value)
            } else {
                bexp.saturating_add(value)
            };
            at = cursor;
        }
    }

    let (bits, range_error) = round(mantissa, sticky, bexp, precision, emax);
    Scan {
        bits,
        consumed: at,
        range_error,
    }
}

/// `mantissa * 2^bexp`, plus `sticky` for anything below its last bit, as the
/// bits of a positive IEEE binary float with `precision` significand bits and
/// `emax` as its largest exponent. `bexp` may arrive saturated. The flag is
/// C's `ERANGE` condition.
fn round(mut mantissa: u64, sticky: bool, mut bexp: i64, precision: u32, emax: i32) -> (u64, bool) {
    if mantissa == 0 {
        return (0, false);
    }

    let emax = emax as i64;
    let emin = 1 - emax;
    let floor_weight = emin - (precision as i64 - 1);
    let infinity = ((2 * emax + 1) as u64) << (precision - 1);

    if bexp > emax + 1 {
        return (infinity, true);
    }
    if bexp <= floor_weight - (u64::BITS as i64 + 1) {
        return (0, true);
    }

    let significant = 64 - mantissa.leading_zeros() as i64;
    // The larger of the two drops, so a subnormal rounds once here instead of
    // once to `precision` and again onto the subnormal grid.
    let drop = (significant - precision as i64).max(floor_weight - bexp);
    if drop > 0 {
        let last = (drop - 1) as u32;
        let half = mantissa >> last & 1 == 1;
        let below = sticky || mantissa & ((1 << last) - 1) != 0;
        mantissa = if drop >= 64 {
            0
        } else {
            mantissa >> drop as u32
        };
        bexp += drop;
        if half && (below || mantissa & 1 == 1) {
            mantissa += 1;
        }
        if mantissa == 0 {
            return (0, true);
        }
        if mantissa >> precision != 0 {
            mantissa >>= 1;
            bexp += 1;
        }
    }

    let significant = 64 - mantissa.leading_zeros() as i64;
    let exponent = bexp + significant - 1;
    if exponent > emax {
        return (infinity, true);
    }
    if exponent < emin {
        return (mantissa << (bexp - floor_weight) as u32, false);
    }
    let fraction = (mantissa << (precision as i64 - significant) as u32) - (1 << (precision - 1));
    (
        ((exponent + emax) as u64) << (precision - 1) | fraction,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits64(text: &[u8]) -> u64 {
        scan_f64(text).value.to_bits()
    }

    fn bits32(text: &[u8]) -> u32 {
        scan_f32(text).value.to_bits()
    }

    #[test]
    fn not_a_hexadecimal_sequence() {
        assert!(!is_hex_prefix(b""));
        assert!(!is_hex_prefix(b"0"));
        assert!(!is_hex_prefix(b"1.5"));
        assert!(!is_hex_prefix(b".5p1"));
        assert!(!is_hex_prefix(b"08"));
        assert!(is_hex_prefix(b"0x"));
        assert!(is_hex_prefix(b"0X1.8P1"));
    }

    #[test]
    fn zero_ex_converts_the_zero_alone() {
        for text in [&b"0x"[..], b"0X", b"0xg", b"0x.p1", b"0x."] {
            let scanned = scan_f64(text);
            assert_eq!(scanned.value, 0.0);
            assert_eq!(scanned.consumed, 1);
            assert!(!scanned.range_error);
        }
        assert_eq!(scan_f32(b"0x").consumed, 1);
    }

    #[test]
    fn subject_sequence_length() {
        assert_eq!(scan_f64(b"0x1p0").consumed, 5);
        assert_eq!(scan_f64(b"0x10zz").consumed, 4);
        // A bare `p` is not part of the number, as a bare `e` is not in the
        // decimal grammar.
        assert_eq!(scan_f64(b"0x1p").consumed, 3);
        assert_eq!(scan_f64(b"0x1p+").consumed, 3);
        assert_eq!(scan_f64(b"0x1p-x").consumed, 3);
        assert_eq!(scan_f64(b"0x1.2.3").consumed, 5);
        assert_eq!(scan_f64(b"0x1.").consumed, 4);
    }

    #[test]
    fn exact_values() {
        assert_eq!(scan_f64(b"0x1p0").value, 1.0);
        assert_eq!(scan_f64(b"0x1.8p1").value, 3.0);
        // The binary exponent is optional in `strtod`.
        assert_eq!(scan_f64(b"0x1.8").value, 1.5);
        assert_eq!(scan_f64(b"0x.8p1").value, 1.0);
        assert_eq!(scan_f64(b"0x10").value, 16.0);
        assert_eq!(scan_f64(b"0X1.FP0").value, 1.9375);
        assert_eq!(scan_f64(b"0x1p-1").value, 0.5);
        assert_eq!(scan_f64(b"0x0.08p0").value, 0.03125);
        assert_eq!(scan_f64(b"0x1p+4").value, 16.0);
        assert_eq!(scan_f64(b"0x0p1000").value, 0.0);
        assert!(!scan_f64(b"0x0p1000").range_error);
        assert_eq!(scan_f64(b"0x1.2").value, 1.125);
    }

    #[test]
    fn leading_zeros_cost_no_precision() {
        assert_eq!(scan_f64(b"0x00000000000000000001.8p1").value, 3.0);
        // Sixteen zeros before the digits that carry all 53 bits.
        assert_eq!(
            bits64(b"0x0000000000000001.fffffffffffff8p0"),
            bits64(b"0x1.fffffffffffff8p0")
        );
    }

    #[test]
    fn largest_finite_and_overflow() {
        assert_eq!(scan_f64(b"0x1.fffffffffffffp1023").value, f64::MAX);
        assert!(!scan_f64(b"0x1.fffffffffffffp1023").range_error);
        assert_eq!(bits64(b"0x1p1023"), 0x7FE0_0000_0000_0000);

        let over = scan_f64(b"0x1p1024");
        assert_eq!(over.value, f64::INFINITY);
        assert!(over.range_error);

        // Overflow by rounding up rather than by the exponent.
        let rounded = scan_f64(b"0x1.fffffffffffff8p1023");
        assert_eq!(rounded.value, f64::INFINITY);
        assert!(rounded.range_error);
        assert_eq!(scan_f64(b"0x1.fffffffffffff7p1023").value, f64::MAX);
    }

    #[test]
    fn subnormals_and_underflow() {
        assert_eq!(bits64(b"0x1p-1022"), f64::MIN_POSITIVE.to_bits());
        assert_eq!(bits64(b"0x1p-1023"), 1 << 51);
        assert_eq!(bits64(b"0x1p-1074"), 1);
        assert!(!scan_f64(b"0x1p-1074").range_error);

        // Exactly half of the smallest subnormal: a tie that rounds to even,
        // which is zero, and therefore a range error.
        let half = scan_f64(b"0x1p-1075");
        assert_eq!(half.value, 0.0);
        assert!(half.range_error);

        // A hair above that half rounds up instead of underflowing.
        let above = scan_f64(b"0x1.0000000000001p-1075");
        assert_eq!(above.value.to_bits(), 1);
        assert!(!above.range_error);

        // The one rounding of a subnormal: 0x1.8p-1074 is a tie between the
        // first two subnormals and goes to the even one.
        assert_eq!(bits64(b"0x1.8p-1074"), 2);
        assert_eq!(bits64(b"0x1.4p-1074"), 1);
        assert_eq!(bits64(b"0x1.cp-1074"), 2);

        let under = scan_f64(b"0x1p-1100");
        assert_eq!(under.value, 0.0);
        assert!(under.range_error);
    }

    #[test]
    fn ties_go_to_even() {
        // 1 + 2^-53 sits halfway between 1 and the next double.
        assert_eq!(bits64(b"0x1.00000000000008p0"), 0x3FF0_0000_0000_0000);
        assert_eq!(bits64(b"0x1.00000000000009p0"), 0x3FF0_0000_0000_0001);
        assert_eq!(bits64(b"0x1.00000000000007p0"), 0x3FF0_0000_0000_0000);
        // The same tie one ulp up, where even is the larger neighbour.
        assert_eq!(bits64(b"0x1.00000000000018p0"), 0x3FF0_0000_0000_0002);
        assert_eq!(bits64(b"0x1.00000000000011p0"), 0x3FF0_0000_0000_0001);
    }

    #[test]
    fn digits_past_the_accumulator_stay_sticky() {
        // Eighteen significand digits: the tie, then a 1 far below it that
        // only the sticky bit can carry, and which breaks the tie upward.
        assert_eq!(bits64(b"0x1.00000000000008000p0"), 0x3FF0_0000_0000_0000);
        assert_eq!(bits64(b"0x1.00000000000008001p0"), 0x3FF0_0000_0000_0001);
        assert_eq!(
            bits64(b"0x1.000000000000080000000000000000001p0"),
            0x3FF0_0000_0000_0001
        );
        // Dropped digits above the point still scale the value: 2^64 + 1 is
        // seventeen digits and rounds back down onto 2^64.
        assert_eq!(
            scan_f64(b"0x10000000000000001p0").value,
            (1u128 << 64) as f64
        );
    }

    #[test]
    fn saturating_exponent() {
        let text = b"0x1p99999999999999999999999";
        let over = scan_f64(text);
        assert_eq!(over.value, f64::INFINITY);
        assert!(over.range_error);
        assert_eq!(over.consumed, text.len());

        let under = scan_f64(b"0x1p-99999999999999999999999");
        assert_eq!(under.value, 0.0);
        assert!(under.range_error);
    }

    #[test]
    fn single_rounding_for_f32() {
        assert_eq!(scan_f32(b"0x1.8p1").value, 3.0);
        assert_eq!(bits32(b"0x1.fffffep127"), f32::MAX.to_bits());
        assert_eq!(bits32(b"0x1p-126"), f32::MIN_POSITIVE.to_bits());
        assert_eq!(bits32(b"0x1p-149"), 1);
        assert!(!scan_f32(b"0x1p-149").range_error);

        let under = scan_f32(b"0x1p-150");
        assert_eq!(under.value, 0.0);
        assert!(under.range_error);

        let over = scan_f32(b"0x1p128");
        assert_eq!(over.value, f32::INFINITY);
        assert!(over.range_error);

        // 1 + 2^-24 + 2^-53. Rounding once to 24 bits keeps the 2^-53 as a
        // sticky bit, which breaks the tie at 2^-24 upward. Rounding through
        // `f64` first loses it, leaving an exact tie that goes to even, so
        // the double-rounded answer is 1.0 and the correct one is not.
        let text = b"0x1.00000100000008p0";
        let once = scan_f32(text).value;
        let twice = scan_f64(text).value as f32;
        assert_eq!(once.to_bits(), 0x3F80_0001);
        assert_eq!(twice.to_bits(), 0x3F80_0000);
        assert_ne!(once, twice);
    }
}
