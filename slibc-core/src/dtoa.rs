//! C's float conversions — `%e %f %g %a` and their uppercase twins.
//!
//! `core::fmt` already renders an `f64` correctly rounded at a requested
//! precision, without allocating and without `std`: `{:.*}` generates `%f`'s
//! digits and `{:.*e}` generates `%e`'s. What it does not do is spell them the
//! way C asks, so that is what is here — a signed two-digit exponent, `%g`'s
//! style chosen from the exponent the value has *after* rounding, and `%a`,
//! which is the significand's own bits and needs no digit generation at all.
//! [`crate::hexfloat`] reads `%a` back; the two round-trip.

use core::fmt::{self, Write};

/// C17 7.21.6.1 p15 floors what one conversion may produce at 4095
/// characters. `%g` picks its style from a scientific probe it then
/// overwrites, and that probe is four bytes longer than the shortest answer
/// it can select: the `e300` it ends with, against a bare radix point.
pub const BUFFER: usize = 4099;

/// A `printf` float conversion. Width and the `-`/`0` flags are padding, which
/// the caller owns; these four decide what digits there are.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    /// One of `a A e E f F g G`.
    pub conv: u8,
    /// Negative for the conversion's default: six digits for `e`, `f` and `g`,
    /// and for `a` as many hexadecimal digits as the value needs to be exact.
    pub precision: i32,
    /// `#`: keep the radix point that nothing follows, and keep `%g`'s
    /// trailing zeros.
    pub alt: bool,
    /// What signs a non-negative value, if anything: `+` or a space.
    pub sign: Option<u8>,
}

/// What [`format`] wrote into the front of `out`.
#[derive(Clone, Copy, Debug)]
pub struct Render {
    pub len: usize,
    /// Where a `0` flag pads: past the sign and past any `0x`. `None` for an
    /// infinity or a NaN, which C says the flag does not pad.
    pub zero_at: Option<usize>,
}

/// Renders `value` under `spec` into the front of `out`. `None`, having left
/// an unspecified prefix behind, when `out` is too short.
pub fn format(out: &mut [u8], value: f64, spec: Spec) -> Option<Render> {
    let mut at = 0usize;
    let sign = if value.is_sign_negative() {
        Some(b'-')
    } else {
        spec.sign
    };
    if let Some(byte) = sign {
        *out.get_mut(0)? = byte;
        at = 1;
    }

    let conv = spec.conv | 0x20;
    let render = if value.is_finite() {
        let magnitude = f64::from_bits(value.to_bits() & !(1u64 << 63));
        let tail = out.get_mut(at..)?;
        let grown = match conv {
            b'a' => hex(tail, magnitude, spec.precision, spec.alt)?,
            b'f' => fixed(tail, magnitude, digits(spec.precision), spec.alt, false)?,
            b'e' => exponential(tail, magnitude, digits(spec.precision), spec.alt)?,
            _ => general(tail, magnitude, spec.precision, spec.alt)?,
        };
        Render {
            len: at + grown,
            zero_at: Some(at + if conv == b'a' { 2 } else { 0 }),
        }
    } else {
        let word: &[u8] = if value.is_nan() { b"nan" } else { b"inf" };
        out.get_mut(at..at + word.len())?.copy_from_slice(word);
        Render {
            len: at + word.len(),
            zero_at: None,
        }
    };

    if spec.conv.is_ascii_uppercase() {
        out[..render.len].make_ascii_uppercase();
    }
    Some(render)
}

fn digits(precision: i32) -> usize {
    if precision < 0 { 6 } else { precision as usize }
}

/// `core::fmt` carries a precision as a `u16` and panics above that, and a
/// precision wider than the output would not fit in it either.
fn bounded(precision: usize, room: usize) -> Option<usize> {
    (precision <= room && precision <= u16::MAX as usize).then_some(precision)
}

fn fixed(out: &mut [u8], value: f64, precision: usize, alt: bool, trim: bool) -> Option<usize> {
    let written = render(out, value, bounded(precision, out.len())?, false)?;
    let end = if trim {
        trimmed(&out[..written])
    } else {
        written
    };
    point(out, end, alt)
}

fn exponential(out: &mut [u8], value: f64, precision: usize, alt: bool) -> Option<usize> {
    let (mantissa, exponent) = scientific(out, value, precision)?;
    finish_scientific(out, mantissa, exponent, alt, false)
}

/// Turns a `{:.*e}` rendering already in `out`, whose mantissa ends at
/// `mantissa`, into C's form.
fn finish_scientific(
    out: &mut [u8],
    mantissa: usize,
    exponent: i32,
    alt: bool,
    trim: bool,
) -> Option<usize> {
    let end = if trim {
        trimmed(&out[..mantissa])
    } else {
        mantissa
    };
    let end = point(out, end, alt)?;
    exponent_at(out, end, b'e', exponent, 2)
}

fn general(out: &mut [u8], value: f64, precision: i32, alt: bool) -> Option<usize> {
    let mut significant = if precision < 0 {
        6
    } else if precision == 0 {
        1
    } else {
        precision as usize
    };
    if !alt {
        // No `double` has a nonzero digit past its 767th significant one and
        // `%g` without `#` drops the zeros, so this cannot change a byte.
        significant = significant.min(768);
    }

    // C picks the style from the exponent the value has *after* rounding to
    // `significant` digits, which only the rounded rendering knows.
    let (mantissa, exponent) = scientific(out, value, significant - 1)?;
    if exponent < -4 || exponent >= significant as i32 {
        return finish_scientific(out, mantissa, exponent, alt, !alt);
    }
    fixed(
        out,
        value,
        (significant as i32 - 1 - exponent) as usize,
        alt,
        !alt,
    )
}

/// `%a`: the significand's hexadecimal digits, exactly. The leading digit is
/// the value's own integer bit, so a subnormal keeps its `p-1022` instead of
/// renormalising and a carry lands there rather than moving the exponent.
fn hex(out: &mut [u8], value: f64, precision: i32, alt: bool) -> Option<usize> {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let bits = value.to_bits();
    let biased = (bits >> 52) as i32;
    let fraction = bits & 0x000f_ffff_ffff_ffff;

    let mut nibbles = [0u8; 14];
    nibbles[0] = u8::from(biased != 0);
    for (i, nibble) in nibbles[1..].iter_mut().enumerate() {
        *nibble = (fraction >> (48 - 4 * i)) as u8 & 0xf;
    }

    let exponent = if biased != 0 {
        biased - 1023
    } else if fraction != 0 {
        -1022
    } else {
        0
    };

    let kept = if precision < 0 {
        let mut n = 13;
        while n > 0 && nibbles[n] == 0 {
            n -= 1;
        }
        n
    } else {
        let want = precision as usize;
        if want < 13 {
            round_nibbles(&mut nibbles, want);
        }
        want
    };

    let mut at = 0usize;
    for byte in [b'0', b'x', HEX[nibbles[0] as usize]] {
        *out.get_mut(at)? = byte;
        at += 1;
    }
    if kept > 0 || alt {
        *out.get_mut(at)? = b'.';
        at += 1;
    }
    for &nibble in &nibbles[1..1 + kept.min(13)] {
        *out.get_mut(at)? = HEX[nibble as usize];
        at += 1;
    }
    for _ in 13..kept {
        *out.get_mut(at)? = b'0';
        at += 1;
    }
    exponent_at(out, at, b'p', exponent, 1)
}

/// Rounds a hexadecimal significand to `kept` fraction digits, to nearest with
/// ties to even. The carry stops at the leading digit, which C lets reach 2.
fn round_nibbles(nibbles: &mut [u8; 14], kept: usize) {
    let first = nibbles[kept + 1];
    let rest = nibbles[kept + 2..].iter().any(|&n| n != 0);
    if first < 8 || (first == 8 && !rest && nibbles[kept] % 2 == 0) {
        return;
    }
    for i in (0..=kept).rev() {
        nibbles[i] += 1;
        if nibbles[i] < 16 {
            return;
        }
        nibbles[i] = 0;
    }
}

fn render(out: &mut [u8], value: f64, precision: usize, scientific: bool) -> Option<usize> {
    let mut sink = Sink { out, written: 0 };
    let done = if scientific {
        write!(sink, "{:.*e}", precision, value)
    } else {
        write!(sink, "{:.*}", precision, value)
    };
    done.ok()?;
    Some(sink.written)
}

/// The `{:.*e}` rendering of `value`, as the offset of its `e` and the decimal
/// exponent that follows it.
fn scientific(out: &mut [u8], value: f64, precision: usize) -> Option<(usize, i32)> {
    let written = render(out, value, bounded(precision, out.len())?, true)?;
    let mantissa = out[..written].iter().position(|&b| b == b'e')?;

    let mut exponent = 0i32;
    for &byte in &out[mantissa + 1..written] {
        if byte != b'-' {
            exponent = exponent * 10 + (byte - b'0') as i32;
        }
    }
    if out[mantissa + 1] == b'-' {
        exponent = -exponent;
    }
    Some((mantissa, exponent))
}

/// `#` keeps the radix point that a precision of zero leaves nothing after.
fn point(out: &mut [u8], end: usize, alt: bool) -> Option<usize> {
    if !alt || out[..end].contains(&b'.') {
        return Some(end);
    }
    *out.get_mut(end)? = b'.';
    Some(end + 1)
}

/// `%g` drops the fraction's trailing zeros, and the radix point with them
/// when they were the whole fraction.
fn trimmed(bytes: &[u8]) -> usize {
    let Some(radix) = bytes.iter().position(|&b| b == b'.') else {
        return bytes.len();
    };
    let mut end = bytes.len();
    while end > radix + 1 && bytes[end - 1] == b'0' {
        end -= 1;
    }
    if end == radix + 1 { radix } else { end }
}

/// Rust spells an exponent `e-4`; C signs it always and pads `%e`'s to two
/// digits. Whatever Rust wrote is already read by here, so this overwrites it.
fn exponent_at(
    out: &mut [u8],
    mut at: usize,
    marker: u8,
    exponent: i32,
    width: usize,
) -> Option<usize> {
    *out.get_mut(at)? = marker;
    *out.get_mut(at + 1)? = if exponent < 0 { b'-' } else { b'+' };
    at += 2;

    let mut decimal = [b'0'; 10];
    let mut n = 0usize;
    let mut rest = exponent.unsigned_abs();
    loop {
        decimal[n] = b'0' + (rest % 10) as u8;
        rest /= 10;
        n += 1;
        if rest == 0 {
            break;
        }
    }
    n = n.max(width);
    while n > 0 {
        n -= 1;
        *out.get_mut(at)? = decimal[n];
        at += 1;
    }
    Some(at)
}

struct Sink<'a> {
    out: &'a mut [u8],
    written: usize,
}

impl Write for Sink<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.written.checked_add(text.len()).ok_or(fmt::Error)?;
        let room = self.out.get_mut(self.written..end).ok_or(fmt::Error)?;
        room.copy_from_slice(text.as_bytes());
        self.written = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hexfloat;

    fn spelled(value: f64, conv: u8, precision: i32, alt: bool) -> ([u8; BUFFER], usize) {
        let mut out = [0u8; BUFFER];
        let render = format(
            &mut out,
            value,
            Spec {
                conv,
                precision,
                alt,
                sign: None,
            },
        )
        .expect("the buffer is the one C requires");
        (out, render.len)
    }

    /// Every expectation below is glibc's own answer for the same conversion.
    macro_rules! assert_spells {
        ($want:expr, $value:expr, $conv:expr, $precision:expr, $alt:expr) => {{
            let (out, len) = spelled($value, $conv, $precision, $alt);
            assert_eq!(
                core::str::from_utf8(&out[..len]).unwrap(),
                $want,
                "%{}{} of {:?}",
                if $alt { "#" } else { "" },
                $conv as char,
                $value
            );
        }};
    }

    #[test]
    fn zero_keeps_its_sign() {
        assert_spells!("0.000000", 0.0, b'f', -1, false);
        assert_spells!("-0.000000", -0.0, b'f', -1, false);
        assert_spells!("-0", -0.0, b'f', 0, false);
        assert_spells!("-0.", -0.0, b'f', 0, true);
        assert_spells!("0.00e+00", 0.0, b'e', 2, false);
        assert_spells!("0e+00", 0.0, b'e', 0, false);
        assert_spells!("0.e+00", 0.0, b'e', 0, true);
        assert_spells!("0", 0.0, b'g', -1, false);
        assert_spells!("0", 0.0, b'g', 1, false);
        assert_spells!("0x0p+0", 0.0, b'a', -1, false);
        assert_spells!("-0x0p+0", -0.0, b'a', -1, false);
        assert_spells!("0x0.p+0", 0.0, b'a', 0, true);
    }

    #[test]
    fn subnormals_are_exact() {
        let least = f64::from_bits(1);
        assert_spells!("0x0.0000000000001p-1022", least, b'a', -1, false);
        assert_spells!("4.940656e-324", least, b'e', -1, false);
        assert_spells!("0.0", least, b'f', 1, false);
        // The largest subnormal, which shares the smallest normal's exponent
        // but not its leading digit.
        let largest = f64::from_bits(0x000f_ffff_ffff_ffff);
        assert_spells!("0x0.fffffffffffffp-1022", largest, b'a', -1, false);
        assert_spells!(
            "0x1p-1022",
            f64::from_bits(0x0010_0000_0000_0000),
            b'a',
            -1,
            false
        );
    }

    #[test]
    fn largest_finite() {
        assert_spells!("0x1.fffffffffffffp+1023", f64::MAX, b'a', -1, false);
        assert_spells!("1.80e+308", f64::MAX, b'e', 2, false);
        assert_spells!("1.797693e+308", f64::MAX, b'e', -1, false);
        // The carry lands in the leading digit and leaves the exponent alone.
        assert_spells!("0x2.00p+1023", f64::MAX, b'a', 2, false);
    }

    #[test]
    fn rounding_is_to_nearest_with_ties_to_even() {
        assert_spells!("0", 0.5, b'f', 0, false);
        assert_spells!("2", 1.5, b'f', 0, false);
        assert_spells!("2", 2.5, b'f', 0, false);
        assert_spells!("4", 3.5, b'f', 0, false);
        assert_spells!("0.2", 0.25, b'f', 1, false);
        assert_spells!("0.8", 0.75, b'f', 1, false);
        // 2.675 is below its own decimal spelling, so a correct rounding goes
        // down where a decimal one would go up.
        assert_spells!("2.67", 2.675, b'f', 2, false);
        assert_spells!("1.00", 1.005, b'f', 2, false);
        // A hexadecimal tie resolves on the last kept digit's parity.
        assert_spells!("0x1.2p+0", 1.09375, b'a', 1, false);
        assert_spells!("0x1.2p+0", 1.15625, b'a', 1, false);
        assert_spells!("0x1.4p+0", 1.21875, b'a', 1, false);
        assert_spells!("0x1.99ap-4", 0.1, b'a', 3, false);
        assert_spells!("0x2p-4", 0.1, b'a', 0, false);
    }

    #[test]
    fn general_switches_style_at_the_exponent_boundary() {
        // Below the low end it is style e, at it style f.
        assert_spells!("1.23e-05", 0.00001234, b'g', 3, false);
        assert_spells!("0.000123", 0.0001234, b'g', 3, false);
        // Below the precision it is style f, at it style e — and the exponent
        // that decides is the one rounding produces, not the one the value has.
        assert_spells!("999", 999.4, b'g', 3, false);
        assert_spells!("1e+03", 999.6, b'g', 3, false);
        assert_spells!("999999", 999999.0, b'g', -1, false);
        assert_spells!("1e+06", 1000000.0, b'g', -1, false);
        assert_spells!("1e-05", 1e-5, b'g', -1, false);
        assert_spells!("0.0001", 1e-4, b'g', -1, false);
    }

    #[test]
    fn general_drops_trailing_zeros_unless_told_not_to() {
        assert_spells!("100", 100.0, b'g', -1, false);
        assert_spells!("100.000", 100.0, b'g', -1, true);
        assert_spells!("0.5", 0.5, b'g', 10, false);
        assert_spells!("0.5000000000", 0.5, b'g', 10, true);
        assert_spells!("1", 1.0, b'g', 0, false);
        assert_spells!("1.", 1.0, b'g', 0, true);
        assert_spells!("1.00", 1.0, b'g', 3, true);
        assert_spells!("1e+06", 1e6, b'g', 1, false);
        assert_spells!("1.e+06", 1e6, b'g', 1, true);
    }

    #[test]
    fn hexadecimal_round_trips_through_the_reader() {
        for bits in [
            1u64,
            0x000f_ffff_ffff_ffff,
            0x0010_0000_0000_0000,
            0x3ff0_0000_0000_0000,
            0x3fb9_999a_0000_0000,
            0x7fef_ffff_ffff_ffff,
            0x4008_0000_0000_0000,
            0,
        ] {
            let value = f64::from_bits(bits);
            let (out, len) = spelled(value, b'a', -1, false);
            let read = hexfloat::scan_f64(&out[..len]);
            assert_eq!(read.consumed, len, "{:?}", &out[..len]);
            assert_eq!(read.value.to_bits(), bits);
        }
    }

    #[test]
    fn infinity_and_nan_ignore_the_zero_flag() {
        for (conv, want) in [
            (b'f', "inf"),
            (b'e', "inf"),
            (b'g', "inf"),
            (b'a', "inf"),
            (b'F', "INF"),
            (b'E', "INF"),
            (b'G', "INF"),
            (b'A', "INF"),
        ] {
            assert_spells!(want, f64::INFINITY, conv, 3, false);
        }
        for (conv, want) in [
            (b'f', "-nan"),
            (b'e', "-nan"),
            (b'g', "-nan"),
            (b'a', "-nan"),
            (b'F', "-NAN"),
            (b'E', "-NAN"),
            (b'G', "-NAN"),
            (b'A', "-NAN"),
        ] {
            assert_spells!(want, -f64::NAN, conv, 3, false);
        }

        let mut out = [0u8; BUFFER];
        let render = format(
            &mut out,
            f64::INFINITY,
            Spec {
                conv: b'f',
                precision: -1,
                alt: false,
                sign: Some(b'+'),
            },
        )
        .unwrap();
        assert_eq!(&out[..render.len], b"+inf");
        assert!(render.zero_at.is_none());
    }

    #[test]
    fn uppercase_reaches_the_exponent_and_the_prefix() {
        assert_spells!("1.500000E+00", 1.5, b'E', -1, false);
        assert_spells!("0X1.8P+1", 3.0, b'A', -1, false);
        assert_spells!("1E+06", 1e6, b'G', -1, false);
        assert_spells!("1.500000", 1.5, b'F', -1, false);
    }

    fn rendered(
        out: &mut [u8; BUFFER],
        value: f64,
        conv: u8,
        precision: i32,
        sign: Option<u8>,
    ) -> Render {
        format(
            out,
            value,
            Spec {
                conv,
                precision,
                alt: false,
                sign,
            },
        )
        .expect("the buffer is the one C requires")
    }

    #[test]
    fn a_zero_flag_pads_past_the_prefix() {
        let mut out = [0u8; BUFFER];

        let render = rendered(&mut out, -1.0, b'a', 4, None);
        assert_eq!(&out[..render.len], b"-0x1.0000p+0");
        assert_eq!(render.zero_at, Some(3));

        let render = rendered(&mut out, -1.5, b'f', 1, None);
        assert_eq!(&out[..render.len], b"-1.5");
        assert_eq!(render.zero_at, Some(1));

        let render = rendered(&mut out, 1.5, b'e', 1, Some(b'+'));
        assert_eq!(&out[..render.len], b"+1.5e+00");
        assert_eq!(render.zero_at, Some(1));

        let render = rendered(&mut out, 1.5, b'g', -1, None);
        assert_eq!(&out[..render.len], b"1.5");
        assert_eq!(render.zero_at, Some(0));
    }

    /// Both are 4095 characters, the most C17 7.21.6.1 p15 lets a conforming
    /// program ask for, and both are `%g`, whose probe is four bytes longer.
    #[test]
    fn the_longest_conforming_conversion_is_rendered() {
        let (out, len) = spelled(1e300, b'g', 4094, true);
        assert_eq!(len, 4095);
        assert_eq!(out[301], b'.');
        let text = core::str::from_utf8(&out[..len]).unwrap();
        assert_eq!(text.parse::<f64>().unwrap(), 1e300);

        let mut out = [0u8; BUFFER];
        let render = format(
            &mut out,
            -1e300,
            Spec {
                conv: b'g',
                precision: 4093,
                alt: true,
                sign: None,
            },
        )
        .expect("the buffer is the one C requires");
        assert_eq!(render.len, 4095);
        assert_eq!(render.zero_at, Some(1));
    }

    #[test]
    fn a_short_buffer_is_refused() {
        let mut out = [0u8; 4];
        assert!(
            format(
                &mut out,
                1.5,
                Spec {
                    conv: b'f',
                    precision: -1,
                    alt: false,
                    sign: None,
                },
            )
            .is_none()
        );
    }
}
