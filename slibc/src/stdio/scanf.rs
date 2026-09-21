use core::ffi::VaList;

use slopos_slibc_core::dtoa;

use crate::string::convert::{strtod, strtof};

use super::chars::{fgetc_unlocked, ungetc_unlocked};
use super::streams;
use super::{EOF, FILE};

#[inline(always)]
fn is_whitespace(c: u8) -> bool {
    c == b' ' || c == b'\t' || c == b'\n' || c == b'\r'
}

/// One byte longer than [`dtoa::BUFFER`], so every float slibc's own
/// `printf` can produce round-trips with its terminator. A longer subject
/// sequence is consumed in full but converts from its first
/// `SUBJECT_MAX - 1` bytes.
const SUBJECT_MAX: usize = dtoa::BUFFER + 1;

/// `long double *dst = strtold(nptr, endptr)`. `strtold` answers in `st(0)`,
/// which no Rust signature can name, so the call and the 80-bit store are
/// written out. RDI and RSI are its own arguments; RDX is `dst`, saved across
/// the call on the stack, whose push also realigns `rsp` for it.
///
/// # Safety
/// As `strtold`; `dst` is a writable `long double`.
#[unsafe(naked)]
unsafe extern "C" fn scan_long_double(_nptr: *const u8, _endptr: *mut *const u8, _dst: *mut u8) {
    core::arch::naked_asm!(
        "push rdx",
        "call strtold",
        "pop rax",
        "fstp tbyte ptr [rax]",
        "ret",
    );
}

/// Stores `text`'s float value through the next variadic pointer. The caller
/// has already found `text` to be a matching sequence.
///
/// # Safety
/// `text` is NUL-terminated; the next variadic argument is a pointer to a
/// float of that width.
unsafe fn store_float(text: *const u8, length: Length, ap: &mut VaList<'_>) {
    match length {
        Length::LongDouble => {
            let mut widened = [0u8; 16];
            scan_long_double(text, core::ptr::null_mut(), widened.as_mut_ptr());
            let dst = ap.next_arg::<*mut u8>();
            if !dst.is_null() {
                core::ptr::copy_nonoverlapping(widened.as_ptr(), dst, 10);
            }
        }
        Length::Long => {
            let value = strtod(text, core::ptr::null_mut());
            let dst = ap.next_arg::<*mut f64>();
            if !dst.is_null() {
                *dst = value;
            }
        }
        // C has no `%hf`; a width narrower than `float` is the default one.
        Length::Default | Length::Short | Length::Char => {
            let value = strtof(text, core::ptr::null_mut());
            let dst = ap.next_arg::<*mut f32>();
            if !dst.is_null() {
                *dst = value;
            }
        }
    }
}

/// The width a directive's length modifier asked for. `z`, `j` and `t` are
/// [`Length::Long`] because every one of them is 64 bits here.
#[derive(Clone, Copy, PartialEq)]
enum Length {
    Char,
    Short,
    Default,
    Long,
    LongDouble,
}

/// # Safety
/// `fp` points into a NUL-terminated format string.
/// `*`, which C17 7.21.6.2 p3 makes "convert but do not store": the input is
/// consumed and the conversion is not counted.
unsafe fn read_suppress(fp: &mut *const u8) -> bool {
    if **fp == b'*' {
        *fp = fp.add(1);
        return true;
    }
    false
}

/// The maximum field width, or `usize::MAX` for a conversion with none. A
/// width that is stated and ignored is what makes `%31s` no bound at all.
unsafe fn read_width(fp: &mut *const u8) -> usize {
    if !(**fp).is_ascii_digit() {
        return usize::MAX;
    }
    let mut width = 0usize;
    while (**fp).is_ascii_digit() {
        width = width
            .saturating_mul(10)
            .saturating_add((**fp - b'0') as usize);
        *fp = fp.add(1);
    }
    width
}

/// The magnitude and sign of the integer at `cursor`, or `None` for a
/// matching failure. `%i` reads its base off the subject's own prefix, which
/// is the whole of what distinguishes it from `%d`; `%x` accepts the prefix
/// C makes optional for it, and `%o` and `%u` take none.
fn read_integer(cursor: &mut impl Cursor, spec: u8, width: usize) -> Option<(u64, bool)> {
    let mut left = width;
    let mut negative = false;

    // A sign spends field width and is not a digit, which is why `%1d` on
    // "-5" converts nothing.
    if matches!(cursor.peek(), b'-' | b'+') {
        negative = cursor.peek() == b'-';
        cursor.bump();
        left = left.saturating_sub(1);
    }

    let mut base: u64 = match spec {
        b'o' => 8,
        b'x' | b'X' => 16,
        b'i' => 0,
        _ => 10,
    };
    let mut value = 0u64;
    let mut digits = 0usize;

    // A `0x` prefix is consumed whole, and the leading `0` stands as the
    // conversion's digit if nothing hexadecimal follows it — so "0xz" reads
    // as 0 with the `z` left, rather than as a matching failure.
    if left > 0 && (base == 0 || base == 16) && cursor.peek() == b'0' {
        cursor.bump();
        left -= 1;
        digits = 1;
        if left > 0 && matches!(cursor.peek(), b'x' | b'X') {
            cursor.bump();
            left -= 1;
            base = 16;
        } else if base == 0 {
            base = 8;
        }
    }
    if base == 0 {
        base = 10;
    }

    while left > 0 {
        let Some(d) = digit_of(cursor.peek(), base) else {
            break;
        };
        value = value.wrapping_mul(base).wrapping_add(d);
        cursor.bump();
        left -= 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    Some((value, negative))
}

fn digit_of(c: u8, base: u64) -> Option<u64> {
    let d = match c {
        b'0'..=b'9' => (c - b'0') as u64,
        b'a'..=b'f' => (c - b'a' + 10) as u64,
        b'A'..=b'F' => (c - b'A' + 10) as u64,
        _ => return None,
    };
    if d < base { Some(d) } else { None }
}

unsafe fn read_length(fp: &mut *const u8) -> Length {
    match **fp {
        b'h' => {
            *fp = fp.add(1);
            if **fp == b'h' {
                *fp = fp.add(1);
                Length::Char
            } else {
                Length::Short
            }
        }
        b'l' => {
            *fp = fp.add(1);
            if **fp == b'l' {
                *fp = fp.add(1);
            }
            Length::Long
        }
        b'j' | b'z' | b't' => {
            *fp = fp.add(1);
            Length::Long
        }
        b'L' => {
            *fp = fp.add(1);
            Length::LongDouble
        }
        _ => Length::Default,
    }
}

/// # Safety
/// The next variadic argument is a pointer to a signed integer of that width.
unsafe fn store_signed(val: i64, length: Length, ap: &mut VaList<'_>) {
    match length {
        Length::Char => {
            let ptr = ap.next_arg::<*mut i8>();
            if !ptr.is_null() {
                *ptr = val as i8;
            }
        }
        Length::Short => {
            let ptr = ap.next_arg::<*mut i16>();
            if !ptr.is_null() {
                *ptr = val as i16;
            }
        }
        Length::Long | Length::LongDouble => {
            let ptr = ap.next_arg::<*mut i64>();
            if !ptr.is_null() {
                *ptr = val;
            }
        }
        Length::Default => {
            let ptr = ap.next_arg::<*mut i32>();
            if !ptr.is_null() {
                *ptr = val as i32;
            }
        }
    }
}

/// # Safety
/// The next variadic argument is a pointer to an unsigned integer of that
/// width.
unsafe fn store_unsigned(val: u64, length: Length, ap: &mut VaList<'_>) {
    match length {
        Length::Char => {
            let ptr = ap.next_arg::<*mut u8>();
            if !ptr.is_null() {
                *ptr = val as u8;
            }
        }
        Length::Short => {
            let ptr = ap.next_arg::<*mut u16>();
            if !ptr.is_null() {
                *ptr = val as u16;
            }
        }
        Length::Long | Length::LongDouble => {
            let ptr = ap.next_arg::<*mut u64>();
            if !ptr.is_null() {
                *ptr = val;
            }
        }
        Length::Default => {
            let ptr = ap.next_arg::<*mut u32>();
            if !ptr.is_null() {
                *ptr = val as u32;
            }
        }
    }
}

/// C17 7.21.6.2 p9's two lengths at a float directive: `len` is the longest
/// initial subsequence of a matching sequence, which the directive consumes
/// whatever comes of it, and `valid` is how much of that matches. `1ex`, a
/// bare `0x` and `infi` make the two differ; `strtod`'s `endptr` reports
/// only `valid`, so it cannot tell those from a whole conversion.
struct Subject {
    len: usize,
    valid: usize,
}

impl Subject {
    fn complete(&self) -> bool {
        self.len != 0 && self.valid == self.len
    }
}

/// The bytes a subject sequence is scanned from.
trait Cursor {
    /// 0 stands for the end of the input, which no subject sequence contains.
    fn peek(&self) -> u8;
    fn bump(&mut self);
}

/// A NUL-terminated string's bytes. No branch below eats a byte it peeked as
/// 0, so the cursor never leaves the string.
struct Text(*const u8);

impl Cursor for Text {
    fn peek(&self) -> u8 {
        unsafe { *self.0 }
    }

    fn bump(&mut self) {
        self.0 = unsafe { self.0.add(1) };
    }
}

/// A stream's bytes, kept in `text` as they are taken. The one byte of
/// lookahead in `pending` is all a `FILE` can be given back, and all p9 asks
/// to be: the whole subsequence is the input item even when it completes no
/// matching sequence.
struct Stream<'a> {
    stream: *mut FILE,
    pending: i32,
    text: &'a mut [u8; SUBJECT_MAX],
    n: usize,
}

impl Cursor for Stream<'_> {
    fn peek(&self) -> u8 {
        if self.pending < 0 {
            0
        } else {
            self.pending as u8
        }
    }

    fn bump(&mut self) {
        if self.n + 1 < self.text.len() {
            self.text[self.n] = self.peek();
            self.n += 1;
        }
        self.pending = unsafe { fgetc_unlocked(self.stream) };
    }
}

/// Walks `strtod`'s grammar, which C17 7.21.6.2 p9 borrows for `%a %e %f %g`.
fn scan_subject(cursor: &mut impl Cursor) -> Subject {
    let mut len = 0usize;
    let mut valid = 0usize;

    macro_rules! eat {
        () => {{
            cursor.bump();
            len += 1;
        }};
    }

    if matches!(cursor.peek(), b'+' | b'-') {
        eat!();
    }

    let word: &[u8] = match cursor.peek().to_ascii_lowercase() {
        b'i' => b"infinity",
        b'n' => b"nan",
        _ => b"",
    };
    if !word.is_empty() {
        for (i, want) in word.iter().enumerate() {
            if cursor.peek().to_ascii_lowercase() != *want {
                return Subject { len, valid };
            }
            eat!();
            let spelled = i + 1;
            if spelled == b"inf".len() || spelled == word.len() {
                valid = len;
            }
        }
        // C17 7.22.1.3 gives a NaN an optional `(n-char-sequence)` payload,
        // so an unclosed one is a subject sequence that matches nothing.
        if word[0] == b'n' && cursor.peek() == b'(' {
            eat!();
            while cursor.peek().is_ascii_alphanumeric() || cursor.peek() == b'_' {
                eat!();
            }
            if cursor.peek() == b')' {
                eat!();
                valid = len;
            }
        }
        return Subject { len, valid };
    }

    let mut digits = false;
    let mut hex = false;
    if cursor.peek() == b'0' {
        digits = true;
        eat!();
        valid = len;
        if matches!(cursor.peek(), b'x' | b'X') {
            hex = true;
            digits = false;
            eat!();
        }
    }

    let mut point = false;
    loop {
        let c = cursor.peek();
        if if hex {
            c.is_ascii_hexdigit()
        } else {
            c.is_ascii_digit()
        } {
            digits = true;
        } else if c == b'.' && !point {
            point = true;
        } else {
            break;
        }
        eat!();
        // A radix point needs no digits after it, but does need some before.
        if digits {
            valid = len;
        }
    }

    let marker = if hex { b'p' } else { b'e' };
    if digits && cursor.peek() | 0x20 == marker {
        eat!();
        if matches!(cursor.peek(), b'+' | b'-') {
            eat!();
        }
        while cursor.peek().is_ascii_digit() {
            eat!();
            valid = len;
        }
    }
    Subject { len, valid }
}

/// The subject sequence at `stream`, NUL-terminated in `text`.
///
/// # Safety
/// `stream` is a locked open stream.
unsafe fn read_subject(stream: *mut FILE, text: &mut [u8; SUBJECT_MAX]) -> Subject {
    let mut cursor = Stream {
        stream,
        pending: fgetc_unlocked(stream),
        text,
        n: 0,
    };
    let subject = scan_subject(&mut cursor);
    ungetc_unlocked(cursor.pending, stream);
    cursor.text[cursor.n] = 0;
    subject
}

unsafe fn vsscanf_impl(input: *const u8, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut matched: i32 = 0;
    let mut ip = input;
    let mut fp = fmt;

    while *fp != 0 {
        if is_whitespace(*fp) {
            fp = fp.add(1);
            while *ip != 0 && is_whitespace(*ip) {
                ip = ip.add(1);
            }
            continue;
        }

        if *fp != b'%' {
            if *ip == 0 || *ip != *fp {
                break;
            }
            ip = ip.add(1);
            fp = fp.add(1);
            continue;
        }

        fp = fp.add(1);
        if *fp == 0 {
            break;
        }

        let suppress = read_suppress(&mut fp);
        let width = read_width(&mut fp);
        let length = read_length(&mut fp);

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut cursor = Text(ip);
                let Some((magnitude, negative)) = read_integer(&mut cursor, spec, width) else {
                    return matched;
                };
                ip = cursor.0;
                if !suppress {
                    // `%u` negates into the unsigned range, exactly as
                    // `strtoul` does, rather than refusing the sign.
                    if spec == b'd' || spec == b'i' {
                        let val = magnitude as i64;
                        store_signed(if negative { -val } else { val }, length, ap);
                    } else {
                        store_unsigned(
                            if negative {
                                magnitude.wrapping_neg()
                            } else {
                                magnitude
                            },
                            length,
                            ap,
                        );
                    }
                    matched += 1;
                }
            }

            b'a' | b'A' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let subject = scan_subject(&mut Text(ip));
                if !subject.complete() {
                    return matched;
                }
                if !suppress {
                    store_float(ip, length, ap);
                    matched += 1;
                }
                ip = ip.add(subject.len);
            }

            b's' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let dst = if suppress {
                    core::ptr::null_mut()
                } else {
                    ap.next_arg::<*mut u8>()
                };
                if !suppress && dst.is_null() {
                    return matched;
                }

                let mut i = 0usize;
                while *ip != 0 && !is_whitespace(*ip) && i < width {
                    if !dst.is_null() {
                        *dst.add(i) = *ip;
                    }
                    ip = ip.add(1);
                    i += 1;
                }
                if !dst.is_null() {
                    *dst.add(i) = 0;
                    matched += 1;
                }
            }

            b'c' => {
                // `%c` writes no terminator, and takes one character when
                // no width says otherwise.
                let want = if width == usize::MAX { 1 } else { width };
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let ptr = if suppress {
                    core::ptr::null_mut()
                } else {
                    ap.next_arg::<*mut u8>()
                };
                let mut i = 0usize;
                while i < want && *ip != 0 {
                    if !ptr.is_null() {
                        *ptr.add(i) = *ip;
                    }
                    ip = ip.add(1);
                    i += 1;
                }
                if i < want {
                    return matched;
                }
                if !suppress {
                    matched += 1;
                }
            }

            b'%' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip != b'%' {
                    break;
                }
                ip = ip.add(1);
            }

            _ => break,
        }
    }

    if matched == 0 && *ip == 0 {
        return EOF;
    }
    matched
}

unsafe fn vfscanf_impl(stream: *mut FILE, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    if stream.is_null() {
        return EOF;
    }
    // One acquisition for the whole conversion: the scan reads and pushes back
    // bytes many times per directive.
    (*stream).lock.lock();
    let matched = vfscanf_core(stream, fmt, ap);
    (*stream).lock.unlock();
    matched
}

unsafe fn vfscanf_core(stream: *mut FILE, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut matched: i32 = 0;
    let mut fp = fmt;

    while *fp != 0 {
        if is_whitespace(*fp) {
            fp = fp.add(1);
            loop {
                let c = fgetc_unlocked(stream);
                if c == EOF {
                    break;
                }
                if !is_whitespace(c as u8) {
                    ungetc_unlocked(c, stream);
                    break;
                }
            }
            continue;
        }

        if *fp != b'%' {
            let c = fgetc_unlocked(stream);
            if c == EOF || c as u8 != *fp {
                break;
            }
            fp = fp.add(1);
            continue;
        }

        fp = fp.add(1);
        if *fp == 0 {
            break;
        }

        let suppress = read_suppress(&mut fp);
        let width = read_width(&mut fp);
        let length = read_length(&mut fp);

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                loop {
                    let c = fgetc_unlocked(stream);
                    if c == EOF {
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        ungetc_unlocked(c, stream);
                        break;
                    }
                }

                let mut text = [0u8; SUBJECT_MAX];
                let mut cursor = Stream {
                    stream,
                    pending: fgetc_unlocked(stream),
                    text: &mut text,
                    n: 0,
                };
                if cursor.pending == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }
                let read = read_integer(&mut cursor, spec, width);
                ungetc_unlocked(cursor.pending, stream);
                let Some((magnitude, negative)) = read else {
                    return matched;
                };
                if !suppress {
                    if spec == b'd' || spec == b'i' {
                        let val = magnitude as i64;
                        store_signed(if negative { -val } else { val }, length, ap);
                    } else {
                        store_unsigned(
                            if negative {
                                magnitude.wrapping_neg()
                            } else {
                                magnitude
                            },
                            length,
                            ap,
                        );
                    }
                    matched += 1;
                }
            }

            b'a' | b'A' | b'e' | b'E' | b'f' | b'F' | b'g' | b'G' => {
                let mut at_end = false;
                loop {
                    let c = fgetc_unlocked(stream);
                    if c == EOF {
                        at_end = true;
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        ungetc_unlocked(c, stream);
                        break;
                    }
                }
                if at_end {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut text = [0u8; SUBJECT_MAX];
                if !read_subject(stream, &mut text).complete() {
                    return matched;
                }
                if !suppress {
                    store_float(text.as_ptr(), length, ap);
                    matched += 1;
                }
            }

            b's' => {
                loop {
                    let c = fgetc_unlocked(stream);
                    if c == EOF {
                        break;
                    }
                    if !is_whitespace(c as u8) {
                        ungetc_unlocked(c, stream);
                        break;
                    }
                }

                let dst = if suppress {
                    core::ptr::null_mut()
                } else {
                    ap.next_arg::<*mut u8>()
                };
                if !suppress && dst.is_null() {
                    return matched;
                }

                let first = fgetc_unlocked(stream);
                if first == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut i = 0usize;
                if !dst.is_null() {
                    *dst.add(i) = first as u8;
                }
                i += 1;

                while i < width {
                    let c = fgetc_unlocked(stream);
                    if c == EOF || is_whitespace(c as u8) {
                        if c != EOF {
                            ungetc_unlocked(c, stream);
                        }
                        break;
                    }
                    if !dst.is_null() {
                        *dst.add(i) = c as u8;
                    }
                    i += 1;
                }
                if !dst.is_null() {
                    *dst.add(i) = 0;
                    matched += 1;
                }
            }

            b'c' => {
                let want = if width == usize::MAX { 1 } else { width };
                let ptr = if suppress {
                    core::ptr::null_mut()
                } else {
                    ap.next_arg::<*mut u8>()
                };
                let mut i = 0usize;
                while i < want {
                    let c = fgetc_unlocked(stream);
                    if c == EOF {
                        break;
                    }
                    if !ptr.is_null() {
                        *ptr.add(i) = c as u8;
                    }
                    i += 1;
                }
                if i < want {
                    if matched == 0 && i == 0 {
                        return EOF;
                    }
                    return matched;
                }
                if !suppress {
                    matched += 1;
                }
            }

            _ => break,
        }
    }

    if matched == 0 {
        return EOF;
    }
    matched
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sscanf(buf: *const u8, fmt: *const u8, mut args: ...) -> i32 {
    vsscanf_impl(buf, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fscanf(stream: *mut FILE, fmt: *const u8, mut args: ...) -> i32 {
    vfscanf_impl(stream, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn scanf(fmt: *const u8, mut args: ...) -> i32 {
    vfscanf_impl(streams::stdin_file(), fmt, &mut args)
}

/// # Safety
/// `fmt`'s conversions match `ap`; `buf` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsscanf(buf: *const u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vsscanf_impl(buf, fmt, &mut ap)
}

/// # Safety
/// As [`vsscanf`], over an open readable stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfscanf(stream: *mut FILE, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfscanf_impl(stream, fmt, &mut ap)
}

/// # Safety
/// As [`vfscanf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vscanf(fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfscanf_impl(streams::stdin_file(), fmt, &mut ap)
}
