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
        Length::Default => {
            let value = strtof(text, core::ptr::null_mut());
            let dst = ap.next_arg::<*mut f32>();
            if !dst.is_null() {
                *dst = value;
            }
        }
    }
}

/// What a directive's `l` or `L` asked for.
#[derive(Clone, Copy, PartialEq)]
enum Length {
    Default,
    Long,
    LongDouble,
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

        let length = match *fp {
            b'l' => {
                fp = fp.add(1);
                Length::Long
            }
            b'L' => {
                fp = fp.add(1);
                Length::LongDouble
            }
            _ => Length::Default,
        };

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut neg = false;
                if *ip == b'-' {
                    neg = true;
                    ip = ip.add(1);
                } else if *ip == b'+' {
                    ip = ip.add(1);
                }

                if !(*ip).is_ascii_digit() {
                    return matched;
                }

                let mut val: i64 = 0;
                while (*ip).is_ascii_digit() {
                    val = val.wrapping_mul(10).wrapping_add((*ip - b'0') as i64);
                    ip = ip.add(1);
                }
                if neg {
                    val = -val;
                }

                if length == Length::Long {
                    let ptr = ap.next_arg::<*mut i64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut i32>();
                    if !ptr.is_null() {
                        *ptr = val as i32;
                    }
                }
                matched += 1;
            }

            b'u' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                if !(*ip).is_ascii_digit() {
                    return matched;
                }

                let mut val: u64 = 0;
                while (*ip).is_ascii_digit() {
                    val = val.wrapping_mul(10).wrapping_add((*ip - b'0') as u64);
                    ip = ip.add(1);
                }

                if length == Length::Long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
            }

            b'x' | b'X' => {
                while *ip != 0 && is_whitespace(*ip) {
                    ip = ip.add(1);
                }
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                if *ip == b'0' && (*ip.add(1) == b'x' || *ip.add(1) == b'X') {
                    ip = ip.add(2);
                }

                let start = ip;
                let mut val: u64 = 0;
                loop {
                    let c = *ip;
                    let d = if c.is_ascii_digit() {
                        (c - b'0') as u64
                    } else if (b'a'..=b'f').contains(&c) {
                        (c - b'a' + 10) as u64
                    } else if (b'A'..=b'F').contains(&c) {
                        (c - b'A' + 10) as u64
                    } else {
                        break;
                    };
                    val = val.wrapping_mul(16).wrapping_add(d);
                    ip = ip.add(1);
                }

                if ip == start {
                    return matched;
                }

                if length == Length::Long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
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
                store_float(ip, length, ap);
                ip = ip.add(subject.len);
                matched += 1;
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

                let dst = ap.next_arg::<*mut u8>();
                if dst.is_null() {
                    return matched;
                }

                let mut i = 0usize;
                while *ip != 0 && !is_whitespace(*ip) {
                    *dst.add(i) = *ip;
                    ip = ip.add(1);
                    i += 1;
                }
                *dst.add(i) = 0;
                matched += 1;
            }

            b'c' => {
                if *ip == 0 {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let ptr = ap.next_arg::<*mut u8>();
                if !ptr.is_null() {
                    *ptr = *ip;
                }
                ip = ip.add(1);
                matched += 1;
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

        let length = match *fp {
            b'l' => {
                fp = fp.add(1);
                Length::Long
            }
            b'L' => {
                fp = fp.add(1);
                Length::LongDouble
            }
            _ => Length::Default,
        };

        let spec = *fp;
        if spec == 0 {
            break;
        }
        fp = fp.add(1);

        match spec {
            b'd' | b'i' => {
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

                let mut neg = false;
                let c = fgetc_unlocked(stream);
                if c == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }
                if c as u8 == b'-' {
                    neg = true;
                } else if c as u8 == b'+' {
                } else if (c as u8).is_ascii_digit() {
                    ungetc_unlocked(c, stream);
                } else {
                    ungetc_unlocked(c, stream);
                    return matched;
                }

                let first = fgetc_unlocked(stream);
                if first == EOF || !(first as u8).is_ascii_digit() {
                    if first != EOF {
                        ungetc_unlocked(first, stream);
                    }
                    return matched;
                }

                let mut val: i64 = (first as u8 - b'0') as i64;
                loop {
                    let d = fgetc_unlocked(stream);
                    if d == EOF || !(d as u8).is_ascii_digit() {
                        if d != EOF {
                            ungetc_unlocked(d, stream);
                        }
                        break;
                    }
                    val = val.wrapping_mul(10).wrapping_add((d as u8 - b'0') as i64);
                }
                if neg {
                    val = -val;
                }

                if length == Length::Long {
                    let ptr = ap.next_arg::<*mut i64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut i32>();
                    if !ptr.is_null() {
                        *ptr = val as i32;
                    }
                }
                matched += 1;
            }

            b'u' => {
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

                let first = fgetc_unlocked(stream);
                if first == EOF || !(first as u8).is_ascii_digit() {
                    if first != EOF {
                        ungetc_unlocked(first, stream);
                    }
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }

                let mut val: u64 = (first as u8 - b'0') as u64;
                loop {
                    let d = fgetc_unlocked(stream);
                    if d == EOF || !(d as u8).is_ascii_digit() {
                        if d != EOF {
                            ungetc_unlocked(d, stream);
                        }
                        break;
                    }
                    val = val.wrapping_mul(10).wrapping_add((d as u8 - b'0') as u64);
                }

                if length == Length::Long {
                    let ptr = ap.next_arg::<*mut u64>();
                    if !ptr.is_null() {
                        *ptr = val;
                    }
                } else {
                    let ptr = ap.next_arg::<*mut u32>();
                    if !ptr.is_null() {
                        *ptr = val as u32;
                    }
                }
                matched += 1;
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
                store_float(text.as_ptr(), length, ap);
                matched += 1;
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

                let dst = ap.next_arg::<*mut u8>();
                if dst.is_null() {
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
                *dst.add(i) = first as u8;
                i += 1;

                loop {
                    let c = fgetc_unlocked(stream);
                    if c == EOF || is_whitespace(c as u8) {
                        if c != EOF {
                            ungetc_unlocked(c, stream);
                        }
                        break;
                    }
                    *dst.add(i) = c as u8;
                    i += 1;
                }
                *dst.add(i) = 0;
                matched += 1;
            }

            b'c' => {
                let c = fgetc_unlocked(stream);
                if c == EOF {
                    if matched == 0 {
                        return EOF;
                    }
                    return matched;
                }
                let ptr = ap.next_arg::<*mut u8>();
                if !ptr.is_null() {
                    *ptr = c as u8;
                }
                matched += 1;
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

/// `vsscanf(3)`.
///
/// # Safety
/// `fmt`'s conversions match `ap`; `buf` is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsscanf(buf: *const u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vsscanf_impl(buf, fmt, &mut ap)
}

/// `vfscanf(3)`.
///
/// # Safety
/// As [`vsscanf`], over an open readable stream.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfscanf(stream: *mut FILE, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfscanf_impl(stream, fmt, &mut ap)
}

/// `vscanf(3)`.
///
/// # Safety
/// As [`vfscanf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vscanf(fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfscanf_impl(streams::stdin_file(), fmt, &mut ap)
}
