use crate::errno::{ERANGE, errno_set};

/// C's `isspace`, which is what every `strto*` skips. The earlier `b <= 0x20`
/// also accepted every control character, so `strtol("\x01" "5", &end)`
/// converted 5 where C requires no conversion at all.
#[inline(always)]
fn is_space(b: u8) -> bool {
    b == b' ' || (0x09..=0x0d).contains(&b)
}

#[inline(always)]
fn digit_value(b: u8) -> i32 {
    if b.is_ascii_digit() {
        (b - b'0') as i32
    } else if b.is_ascii_lowercase() {
        (b - b'a' + 10) as i32
    } else if b.is_ascii_uppercase() {
        (b - b'A' + 10) as i32
    } else {
        -1
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn atoi(s: *const u8) -> i32 {
    strtol(s, core::ptr::null_mut(), 10) as i32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn atol(s: *const u8) -> i64 {
    strtol(s, core::ptr::null_mut(), 10)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtol(s: *const u8, endptr: *mut *const u8, base: i32) -> i64 {
    if s.is_null() {
        if !endptr.is_null() {
            *endptr = core::ptr::null();
        }
        return 0;
    }

    let original = s;
    let mut p = s;

    while *p != 0 && is_space(*p) {
        p = p.add(1);
    }

    let mut neg = false;
    if *p == b'+' {
        p = p.add(1);
    } else if *p == b'-' {
        neg = true;
        p = p.add(1);
    }

    // Where the `x` of a consumed `0x` sits. C's subject sequence for a `0x`
    // no hex digit follows is the `0` alone, so the caller resumes there.
    let mut after_zero: *const u8 = core::ptr::null();
    let mut b = base;
    if b == 0 {
        if *p == b'0' {
            if *p.add(1) == b'x' || *p.add(1) == b'X' {
                b = 16;
                after_zero = p.add(1);
                p = p.add(2);
            } else {
                b = 8;
            }
        } else {
            b = 10;
        }
    } else if b == 16 {
        if *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            after_zero = p.add(1);
            p = p.add(2);
        }
    }

    if !(2..=36).contains(&b) {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    let digits_start = p;
    let mut value: u64 = 0;
    let mut overflowed = false;

    loop {
        let ch = *p;
        if ch == 0 {
            break;
        }
        let d = digit_value(ch);
        if d < 0 || d >= b {
            break;
        }
        value = match value
            .checked_mul(b as u64)
            .and_then(|v| v.checked_add(d as u64))
        {
            Some(v) => v,
            None => {
                overflowed = true;
                value
            }
        };
        p = p.add(1);
    }

    if p == digits_start {
        if !endptr.is_null() {
            *endptr = if after_zero.is_null() {
                original
            } else {
                after_zero
            };
        }
        return 0;
    }

    if !endptr.is_null() {
        *endptr = p;
    }

    // C requires the saturated value *and* `ERANGE`, which is the `errno`
    // `std::stol` reads to decide whether to throw `out_of_range`.
    let limit = if neg {
        i64::MIN as u64
    } else {
        i64::MAX as u64
    };
    if overflowed || value > limit {
        errno_set(ERANGE.raw());
        return if neg { i64::MIN } else { i64::MAX };
    }

    if neg {
        (value as i64).wrapping_neg()
    } else {
        value as i64
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoul(s: *const u8, endptr: *mut *const u8, base: i32) -> u64 {
    if s.is_null() {
        if !endptr.is_null() {
            *endptr = core::ptr::null();
        }
        return 0;
    }

    let original = s;
    let mut p = s;

    while *p != 0 && is_space(*p) {
        p = p.add(1);
    }

    let mut neg = false;
    if *p == b'+' {
        p = p.add(1);
    } else if *p == b'-' {
        neg = true;
        p = p.add(1);
    }

    // Where the `x` of a consumed `0x` sits. C's subject sequence for a `0x`
    // no hex digit follows is the `0` alone, so the caller resumes there.
    let mut after_zero: *const u8 = core::ptr::null();
    let mut b = base;
    if b == 0 {
        if *p == b'0' {
            if *p.add(1) == b'x' || *p.add(1) == b'X' {
                b = 16;
                after_zero = p.add(1);
                p = p.add(2);
            } else {
                b = 8;
            }
        } else {
            b = 10;
        }
    } else if b == 16 {
        if *p == b'0' && (*p.add(1) == b'x' || *p.add(1) == b'X') {
            after_zero = p.add(1);
            p = p.add(2);
        }
    }

    if !(2..=36).contains(&b) {
        if !endptr.is_null() {
            *endptr = original;
        }
        return 0;
    }

    let digits_start = p;
    let mut value: u64 = 0;
    let mut overflowed = false;

    loop {
        let ch = *p;
        if ch == 0 {
            break;
        }
        let d = digit_value(ch);
        if d < 0 || d >= b {
            break;
        }
        value = match value
            .checked_mul(b as u64)
            .and_then(|v| v.checked_add(d as u64))
        {
            Some(v) => v,
            None => {
                overflowed = true;
                value
            }
        };
        p = p.add(1);
    }

    if p == digits_start {
        if !endptr.is_null() {
            *endptr = if after_zero.is_null() {
                original
            } else {
                after_zero
            };
        }
        return 0;
    }

    if !endptr.is_null() {
        *endptr = p;
    }

    if overflowed {
        errno_set(ERANGE.raw());
        return u64::MAX;
    }

    // A negated unsigned conversion is C's own rule and is not an overflow.
    if neg { value.wrapping_neg() } else { value }
}

pub fn itoa_buf(n: i64, buf: *mut u8, base: u32) -> *mut u8 {
    if buf.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        if !(2..=36).contains(&base) {
            *buf = 0;
            return buf;
        }

        let mut p = buf.add(64);
        *p = 0;

        let negative = base == 10 && n < 0;
        let mut value = if negative { n.unsigned_abs() } else { n as u64 };

        loop {
            let d = (value % base as u64) as u8;
            p = p.sub(1);
            *p = if d < 10 { b'0' + d } else { b'a' + (d - 10) };
            value /= base as u64;
            if value == 0 {
                break;
            }
        }

        if negative {
            p = p.sub(1);
            *p = b'-';
        }

        p
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoll(s: *const u8, endptr: *mut *const u8, base: i32) -> i64 {
    strtol(s, endptr, base)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoull(s: *const u8, endptr: *mut *const u8, base: i32) -> u64 {
    strtoul(s, endptr, base)
}

/// The longest prefix of `p` that C's `strtod` grammar accepts, as a `str`
/// `core`'s own correctly-rounded parser can take. `None` means no conversion
/// was performed, which is what sets `endptr` back to the original pointer.
///
/// Hexadecimal significands are deliberately absent: the scan stops at the
/// `x`, so `strtod("0x10", &end)` answers 0 with `end` at the `x` — C89's
/// reading of it, and a refusal a caller can see rather than a wrong value.
unsafe fn scan_float(p: *const u8) -> Option<(&'static str, *const u8)> {
    let start = p;
    let mut at = p;

    if *at == b'+' || *at == b'-' {
        at = at.add(1);
    }

    let word = |at: *const u8, word: &[u8]| -> bool {
        word.iter()
            .enumerate()
            .all(|(i, w)| (*at.add(i)).to_ascii_lowercase() == *w)
    };

    let end = if word(at, b"infinity") {
        at.add(8)
    } else if word(at, b"inf") {
        at.add(3)
    } else if word(at, b"nan") {
        at.add(3)
    } else {
        let mut digits = 0usize;
        while (*at).is_ascii_digit() {
            at = at.add(1);
            digits += 1;
        }
        if *at == b'.' {
            at = at.add(1);
            while (*at).is_ascii_digit() {
                at = at.add(1);
                digits += 1;
            }
        }
        if digits == 0 {
            return None;
        }
        // An exponent is only part of the number when it has digits; a bare
        // `1e` converts as `1` with `endptr` at the `e`.
        if *at == b'e' || *at == b'E' {
            let mut exponent = at.add(1);
            if *exponent == b'+' || *exponent == b'-' {
                exponent = exponent.add(1);
            }
            if (*exponent).is_ascii_digit() {
                at = exponent;
                while (*at).is_ascii_digit() {
                    at = at.add(1);
                }
            }
        }
        at
    };

    let len = end.offset_from(start) as usize;
    let text = core::str::from_utf8(core::slice::from_raw_parts(start, len)).ok()?;
    Some((text, end))
}

/// # Safety
/// `s` is a NUL-terminated C string or null; `endptr` is writable or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtod(s: *const u8, endptr: *mut *const u8) -> f64 {
    let (text, end) = match parse_float_prefix(s, endptr) {
        Some(parsed) => parsed,
        None => return 0.0,
    };
    if !endptr.is_null() {
        *endptr = end;
    }
    let value: f64 = text.parse().unwrap_or(0.0);
    if out_of_range(text, value == 0.0, value.is_infinite()) {
        errno_set(ERANGE.raw());
    }
    value
}

/// # Safety
/// `s` is a NUL-terminated C string or null; `endptr` is writable or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtof(s: *const u8, endptr: *mut *const u8) -> f32 {
    let (text, end) = match parse_float_prefix(s, endptr) {
        Some(parsed) => parsed,
        None => return 0.0,
    };
    if !endptr.is_null() {
        *endptr = end;
    }
    let value: f32 = text.parse().unwrap_or(0.0);
    if out_of_range(text, value == 0.0, value.is_infinite()) {
        errno_set(ERANGE.raw());
    }
    value
}

/// Whether a conversion that produced zero or an infinity did so by running
/// out of range rather than by being asked for one. `core`'s parser saturates
/// silently, and `std::stod` decides whether to throw `out_of_range` on the
/// `errno` that saturation is supposed to leave behind.
fn out_of_range(text: &str, is_zero: bool, is_infinite: bool) -> bool {
    let digits = text.trim_start_matches(['+', '-']);
    let spelled = |word: &str| {
        digits.len() >= word.len()
            && digits.as_bytes()[..word.len()]
                .iter()
                .zip(word.as_bytes())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
    };
    if spelled("inf") || spelled("nan") {
        return false;
    }
    if is_infinite {
        return true;
    }
    // The significand alone, because `0e1` is exactly zero and a range error
    // there is one `std::stod` turns into a throw.
    let significand = digits.split(['e', 'E']).next().unwrap_or(digits);
    is_zero && significand.bytes().any(|b| (b'1'..=b'9').contains(&b))
}

/// # Safety
/// `s` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn atof(s: *const u8) -> f64 {
    strtod(s, core::ptr::null_mut())
}

/// Skips leading whitespace and hands back the scanned prefix, writing the
/// no-conversion `endptr` itself so both callers state it once.
unsafe fn parse_float_prefix(
    s: *const u8,
    endptr: *mut *const u8,
) -> Option<(&'static str, *const u8)> {
    if s.is_null() {
        if !endptr.is_null() {
            *endptr = core::ptr::null();
        }
        return None;
    }
    let mut p = s;
    while *p != 0 && is_space(*p) {
        p = p.add(1);
    }
    match scan_float(p) {
        Some(parsed) => Some(parsed),
        None => {
            if !endptr.is_null() {
                *endptr = s;
            }
            None
        }
    }
}

/// `strtold(3)`, whose `long double` return Rust has no type for: it is x87
/// 80-bit here, and the System V ABI returns it in `st(0)`, which no Rust
/// signature can name. So the declared return is `()` and the two
/// instructions that widen `strtod`'s `double` are written out.
///
/// The value is `strtod`'s, so this carries `double` precision rather than
/// the 64 significand bits an 80-bit parse would give. libc++ requires the
/// entry point to build (`std::stold`); an 80-bit decimal parser is not here.
///
/// Register contract: RDI = `nptr` and RSI = `endptr` are passed to `strtod`
/// untouched, and the XMM0 it answers with is spilled and reloaded onto the
/// x87 stack, which is where a caller reads a `long double`.
///
/// # Safety
/// As `strtod`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtold(_nptr: *const u8, _endptr: *mut *const u8) {
    core::arch::naked_asm!(
        // 24, not 16: the ABI wants `rsp` 16-byte aligned at the `call`, and
        // this function's own return address already offsets it by 8.
        "sub rsp, 24",
        "call strtod",
        "movsd qword ptr [rsp], xmm0",
        "fld qword ptr [rsp]",
        "add rsp, 24",
        "ret",
    );
}
