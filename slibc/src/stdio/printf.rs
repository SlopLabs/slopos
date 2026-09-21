use core::ffi::VaList;

use slopos_slibc_core::{dtoa, utf8};

use super::FILE;
use super::chars::fputc_unlocked;
use super::streams;
use crate::wchar::wchar_t;

const FLAG_LEFT: u32 = 1;
const FLAG_ZERO: u32 = 2;
const FLAG_PLUS: u32 = 4;
const FLAG_SPACE: u32 = 8;
const FLAG_ALT: u32 = 16;

const DIGITS_LOWER: &[u8; 16] = b"0123456789abcdef";
const DIGITS_UPPER: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Clone, Copy, PartialEq)]
enum Length {
    Default,
    Long,
    LongLong,
    SizeT,
    PtrdiffT,
    LongDouble,
}

/// The x86-64 System V `va_list`, whose layout [`VaList`] is `repr(transparent)`
/// over: a `long double` is MEMORY class, so no `next_arg` can reach it and
/// the overflow area has to be stepped by hand.
#[repr(C)]
struct SysVVaList {
    gp_offset: u32,
    fp_offset: u32,
    overflow_arg_area: *mut u8,
    reg_save_area: *mut u8,
}

/// `va_arg(ap, long double)`, narrowed. A MEMORY-class argument rounds the
/// overflow area up to the type's 16-byte alignment, reads, and advances by
/// 16, which is what clang emits for the same `va_arg`. Narrowing is Tier B
/// of [`crate::math::longdouble`]: `%Lf` prints at `double` precision.
///
/// # Safety
/// The next variadic argument is a `long double`.
unsafe fn next_long_double(ap: &mut VaList<'_>) -> f64 {
    let list = (ap as *mut VaList<'_>).cast::<SysVVaList>();
    let slot = ((*list).overflow_arg_area as usize).next_multiple_of(16) as *mut u8;
    (*list).overflow_arg_area = slot.add(16);

    let mut narrowed = 0.0f64;
    core::arch::asm!(
        "fld tbyte ptr [{slot}]",
        "fstp qword ptr [{narrowed}]",
        slot = in(reg) slot,
        narrowed = in(reg) &raw mut narrowed,
        options(nostack, preserves_flags),
    );
    narrowed
}

/// Bytes the multibyte form of `wide` would take within `cap`, and whether
/// it stopped on a character that has none. The cap is tested before each
/// element is read, because C99 7.19.6.1 lets a `%.Nls` argument be an array
/// a precision bounds rather than one a NUL ends.
unsafe fn multibyte_len(wide: *const wchar_t, cap: usize) -> (usize, bool) {
    let mut total = 0usize;
    let mut at = 0isize;
    while total < cap && *wide.offset(at) != 0 {
        let mut encoded = [0u8; 4];
        let Some(used) = utf8::encode(*wide.offset(at) as u32, &mut encoded) else {
            return (total, true);
        };
        if total + used > cap {
            break;
        }
        total += used;
        at += 1;
    }
    (total, false)
}

/// What a null string argument prints, which C leaves undefined and every
/// libc answers anyway.
fn null_string(precision: i32) -> (*const u8, usize) {
    const NULL_STR: &[u8; 6] = b"(null)";
    let len = if precision >= 0 && (precision as usize) < NULL_STR.len() {
        precision as usize
    } else {
        NULL_STR.len()
    };
    (NULL_STR.as_ptr(), len)
}

unsafe fn write_unsigned(value: u64, base: u64, digits: &[u8; 16], buf: &mut [u8; 22]) -> usize {
    if value == 0 {
        buf[21] = b'0';
        return 1;
    }
    let mut pos = 22usize;
    let mut v = value;
    while v > 0 {
        pos -= 1;
        buf[pos] = digits[(v % base) as usize];
        v /= base;
    }
    22 - pos
}

/// Runs `fmt`'s conversions, emitting each byte through `out` and answering
/// how many there were. `malformed` reports a wide argument with no multibyte
/// form, which C makes the conversion fail on rather than write nothing and
/// claim success.
pub(crate) unsafe fn format_to_cb<F: FnMut(u8)>(
    out: &mut F,
    fmt: *const u8,
    ap: &mut VaList<'_>,
    malformed: &mut bool,
) -> i32 {
    let mut count: i32 = 0;
    let mut p = fmt;

    macro_rules! emit {
        ($byte:expr) => {{
            out($byte);
            count += 1;
        }};
    }

    macro_rules! emit_pad {
        ($byte:expr, $n:expr) => {{
            let mut _i = 0i32;
            while _i < $n {
                emit!($byte);
                _i += 1;
            }
        }};
    }

    while *p != 0 {
        if *p != b'%' {
            emit!(*p);
            p = p.add(1);
            continue;
        }
        p = p.add(1);

        if *p == 0 {
            break;
        }
        if *p == b'%' {
            emit!(b'%');
            p = p.add(1);
            continue;
        }

        let mut flags: u32 = 0;
        loop {
            match *p {
                b'-' => flags |= FLAG_LEFT,
                b'0' => flags |= FLAG_ZERO,
                b'+' => flags |= FLAG_PLUS,
                b' ' => flags |= FLAG_SPACE,
                b'#' => flags |= FLAG_ALT,
                _ => break,
            }
            p = p.add(1);
        }

        let mut width: i32 = 0;
        while (*p).is_ascii_digit() {
            width = width * 10 + (*p - b'0') as i32;
            p = p.add(1);
        }

        let mut precision: i32 = -1;
        if *p == b'.' {
            p = p.add(1);
            precision = 0;
            while (*p).is_ascii_digit() {
                precision = precision * 10 + (*p - b'0') as i32;
                p = p.add(1);
            }
        }

        let mut length = Length::Default;
        match *p {
            b'l' => {
                p = p.add(1);
                if *p == b'l' {
                    length = Length::LongLong;
                    p = p.add(1);
                } else {
                    length = Length::Long;
                }
            }
            b'z' => {
                length = Length::SizeT;
                p = p.add(1);
            }
            b'j' => {
                length = Length::Long;
                p = p.add(1);
            }
            b't' => {
                length = Length::PtrdiffT;
                p = p.add(1);
            }
            b'L' => {
                length = Length::LongDouble;
                p = p.add(1);
            }
            b'h' => {
                p = p.add(1);
                if *p == b'h' {
                    p = p.add(1);
                }
                // h and hh are promoted to int in varargs, so default applies.
            }
            _ => {}
        }

        let spec = *p;
        if spec == 0 {
            break;
        }
        p = p.add(1);

        match spec {
            b'd' | b'i' => {
                let val: i64 = match length {
                    Length::LongLong | Length::SizeT | Length::PtrdiffT => ap.next_arg::<i64>(),
                    Length::Long => ap.next_arg::<i64>(),
                    Length::Default | Length::LongDouble => ap.next_arg::<i32>() as i64,
                };

                let negative = val < 0;
                let abs_val = val.unsigned_abs();

                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(abs_val, 10, DIGITS_LOWER, &mut num_buf);
                let num_start = 22 - num_len;

                let sign: Option<u8> = if negative {
                    Some(b'-')
                } else if flags & FLAG_PLUS != 0 {
                    Some(b'+')
                } else if flags & FLAG_SPACE != 0 {
                    Some(b' ')
                } else {
                    None
                };

                let min_digits = if precision >= 0 {
                    precision as usize
                } else {
                    1
                };
                let zero_fill = if num_len < min_digits {
                    min_digits - num_len
                } else {
                    0
                };

                let sign_len = if sign.is_some() { 1 } else { 0 };
                let content_len = sign_len + zero_fill + num_len;
                let pad = if (width as usize) > content_len {
                    width as usize - content_len
                } else {
                    0
                };

                let use_zero_pad =
                    flags & FLAG_ZERO != 0 && flags & FLAG_LEFT == 0 && precision < 0;

                if flags & FLAG_LEFT == 0 && !use_zero_pad {
                    emit_pad!(b' ', pad as i32);
                }
                if let Some(s) = sign {
                    emit!(s);
                }
                if use_zero_pad {
                    emit_pad!(b'0', pad as i32);
                }
                emit_pad!(b'0', zero_fill as i32);
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            b'u' | b'o' | b'x' | b'X' => {
                let val: u64 = match length {
                    Length::LongLong | Length::SizeT | Length::PtrdiffT => ap.next_arg::<u64>(),
                    Length::Long => ap.next_arg::<u64>(),
                    Length::Default | Length::LongDouble => ap.next_arg::<u32>() as u64,
                };

                let (base, digits): (u64, &[u8; 16]) = match spec {
                    b'o' => (8, DIGITS_LOWER),
                    b'x' => (16, DIGITS_LOWER),
                    b'X' => (16, DIGITS_UPPER),
                    _ => (10, DIGITS_LOWER),
                };

                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(val, base, digits, &mut num_buf);
                let num_start = 22 - num_len;

                let prefix: &[u8] = if flags & FLAG_ALT != 0 && val != 0 {
                    match spec {
                        b'o' => b"0",
                        b'x' => b"0x",
                        b'X' => b"0X",
                        _ => b"",
                    }
                } else {
                    b""
                };

                let min_digits = if precision >= 0 {
                    precision as usize
                } else {
                    1
                };
                let zero_fill = if num_len < min_digits {
                    min_digits - num_len
                } else {
                    0
                };

                let content_len = prefix.len() + zero_fill + num_len;
                let pad = if (width as usize) > content_len {
                    width as usize - content_len
                } else {
                    0
                };

                let use_zero_pad =
                    flags & FLAG_ZERO != 0 && flags & FLAG_LEFT == 0 && precision < 0;

                if flags & FLAG_LEFT == 0 && !use_zero_pad {
                    emit_pad!(b' ', pad as i32);
                }
                for &b in prefix {
                    emit!(b);
                }
                if use_zero_pad {
                    emit_pad!(b'0', pad as i32);
                }
                emit_pad!(b'0', zero_fill as i32);
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            // Measured before anything is written, because the field width
            // pads to the byte count rather than the character count.
            b's' if length == Length::Long => {
                let wide: *const wchar_t = ap.next_arg::<*const wchar_t>();
                if wide.is_null() {
                    let (null_str, slen) = null_string(precision);
                    let pad = (width as usize).saturating_sub(slen) as i32;
                    if flags & FLAG_LEFT == 0 {
                        emit_pad!(b' ', pad);
                    }
                    for i in 0..slen {
                        emit!(*null_str.add(i));
                    }
                    if flags & FLAG_LEFT != 0 {
                        emit_pad!(b' ', pad);
                    }
                    continue;
                }
                let cap = if precision >= 0 {
                    precision as usize
                } else {
                    usize::MAX
                };
                let (bytes, unencodable) = multibyte_len(wide, cap);
                if unencodable {
                    *malformed = true;
                    break;
                }
                let pad = (width as usize).saturating_sub(bytes) as i32;

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad);
                }
                let mut written = 0usize;
                let mut at = 0isize;
                while written < bytes {
                    let mut encoded = [0u8; 4];
                    let used =
                        utf8::encode(*wide.offset(at) as u32, &mut encoded).unwrap_or_default();
                    for byte in &encoded[..used] {
                        emit!(*byte);
                    }
                    written += used;
                    at += 1;
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad);
                }
            }

            b'c' if length == Length::Long => {
                let mut encoded = [0u8; 4];
                let Some(used) = utf8::encode(ap.next_arg::<u32>(), &mut encoded) else {
                    *malformed = true;
                    break;
                };
                let pad = (width as usize).saturating_sub(used) as i32;

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad);
                }
                for byte in &encoded[..used] {
                    emit!(*byte);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad);
                }
            }

            b's' => {
                let s_ptr: *const u8 = ap.next_arg::<*const u8>();
                let (actual, mut slen) = if s_ptr.is_null() {
                    null_string(precision)
                } else {
                    let mut len = 0usize;
                    let mut q = s_ptr;
                    while *q != 0 {
                        len += 1;
                        q = q.add(1);
                    }
                    (s_ptr, len)
                };

                if precision >= 0 && (precision as usize) < slen {
                    slen = precision as usize;
                }

                let pad = if (width as usize) > slen {
                    width as usize - slen
                } else {
                    0
                };

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad as i32);
                }
                for i in 0..slen {
                    emit!(*actual.add(i));
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            b'c' => {
                let c = ap.next_arg::<i32>() as u8;
                let pad = if width > 1 { width - 1 } else { 0 };

                if flags & FLAG_LEFT == 0 {
                    emit_pad!(b' ', pad);
                }
                emit!(c);
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad);
                }
            }

            b'p' => {
                let ptr_val = ap.next_arg::<usize>() as u64;
                let mut num_buf = [0u8; 22];
                let num_len = write_unsigned(ptr_val, 16, DIGITS_LOWER, &mut num_buf);
                let num_start = 22 - num_len;

                emit!(b'0');
                emit!(b'x');
                for i in 0..num_len {
                    emit!(num_buf[num_start + i]);
                }
            }

            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                let value = if length == Length::LongDouble {
                    next_long_double(ap)
                } else {
                    ap.next_arg::<f64>()
                };

                let mut num_buf = [0u8; dtoa::BUFFER];
                let Some(rendered) = dtoa::format(
                    &mut num_buf,
                    value,
                    dtoa::Spec {
                        conv: spec,
                        precision,
                        alt: flags & FLAG_ALT != 0,
                        sign: if flags & FLAG_PLUS != 0 {
                            Some(b'+')
                        } else if flags & FLAG_SPACE != 0 {
                            Some(b' ')
                        } else {
                            None
                        },
                    },
                ) else {
                    // Wider than C17 7.21.6.1 p15 asks any implementation to
                    // render; 7.21.6.3 p3 is how to say no byte was emitted.
                    return -1;
                };

                let pad = if (width as usize) > rendered.len {
                    width as usize - rendered.len
                } else {
                    0
                };
                // Unlike the integer conversions, a precision does not
                // disable the `0` flag for a float.
                let zero_at = if flags & FLAG_ZERO != 0 && flags & FLAG_LEFT == 0 {
                    rendered.zero_at
                } else {
                    None
                };

                if flags & FLAG_LEFT == 0 && zero_at.is_none() {
                    emit_pad!(b' ', pad as i32);
                }
                let split = zero_at.unwrap_or(rendered.len);
                for &byte in &num_buf[..split] {
                    emit!(byte);
                }
                if zero_at.is_some() {
                    emit_pad!(b'0', pad as i32);
                }
                for &byte in &num_buf[split..rendered.len] {
                    emit!(byte);
                }
                if flags & FLAG_LEFT != 0 {
                    emit_pad!(b' ', pad as i32);
                }
            }

            _ => {
                emit!(b'%');
                emit!(spec);
            }
        }
    }

    count
}

pub(crate) unsafe fn vfprintf_impl(stream: *mut FILE, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    if stream.is_null() {
        return -1;
    }
    // POSIX §2.5.1 requires the whole conversion to be atomic against other
    // stdio on the stream; locking here keeps it off the per-byte emit path.
    (*stream).lock.lock();
    let mut malformed = false;
    let count = format_to_cb(
        &mut |b: u8| {
            fputc_unlocked(b as i32, stream);
        },
        fmt,
        ap,
        &mut malformed,
    );
    (*stream).lock.unlock();
    if malformed {
        crate::errno::errno_set(crate::errno::EILSEQ.raw());
        return -1;
    }
    count
}

unsafe fn vsnprintf_impl(buf: *mut u8, n: usize, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    let mut pos: usize = 0;
    let limit = if n > 0 { n - 1 } else { 0 };
    let mut malformed = false;

    let total = format_to_cb(
        &mut |b: u8| {
            if pos < limit {
                *buf.add(pos) = b;
            }
            pos += 1;
        },
        fmt,
        ap,
        &mut malformed,
    );

    if n > 0 {
        let term = if pos < limit { pos } else { limit };
        *buf.add(term) = 0;
    }

    if malformed {
        crate::errno::errno_set(crate::errno::EILSEQ.raw());
        return -1;
    }
    total
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn printf(fmt: *const u8, mut args: ...) -> i32 {
    vfprintf_impl(streams::stdout_file(), fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn fprintf(stream: *mut FILE, fmt: *const u8, mut args: ...) -> i32 {
    vfprintf_impl(stream, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sprintf(buf: *mut u8, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(buf, usize::MAX, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn snprintf(buf: *mut u8, n: usize, fmt: *const u8, mut args: ...) -> i32 {
    vsnprintf_impl(buf, n, fmt, &mut args)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vprintf(fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfprintf_impl(streams::stdout_file(), fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfprintf(stream: *mut FILE, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vfprintf_impl(stream, fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsprintf(buf: *mut u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vsnprintf_impl(buf, usize::MAX, fmt, &mut ap)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn vsnprintf(
    buf: *mut u8,
    n: usize,
    fmt: *const u8,
    mut ap: VaList<'_>,
) -> i32 {
    vsnprintf_impl(buf, n, fmt, &mut ap)
}

/// `vasprintf(3)`. The measuring pass takes a `va_copy`, because a `va_list`
/// walked once cannot be rewound to format from.
///
/// # Safety
/// `fmt`'s conversions match `ap`; `strp` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vasprintf(strp: *mut *mut u8, fmt: *const u8, mut ap: VaList<'_>) -> i32 {
    vasprintf_impl(strp, fmt, &mut ap)
}

unsafe fn vasprintf_impl(strp: *mut *mut u8, fmt: *const u8, ap: &mut VaList<'_>) -> i32 {
    if strp.is_null() {
        return -1;
    }
    *strp = core::ptr::null_mut();
    let mut probe = ap.clone();
    let len = vsnprintf_impl(core::ptr::null_mut(), 0, fmt, &mut probe);
    if len < 0 {
        return -1;
    }
    let size = len as usize + 1;
    let buf = crate::mem::malloc::alloc(size) as *mut u8;
    if buf.is_null() {
        crate::errno::errno_set(crate::errno::ENOMEM.raw());
        return -1;
    }
    let written = vsnprintf_impl(buf, size, fmt, ap);
    if written < 0 {
        crate::mem::malloc::dealloc(buf as *mut core::ffi::c_void);
        return -1;
    }
    *strp = buf;
    written
}

/// # Safety
/// As [`vasprintf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asprintf(strp: *mut *mut u8, fmt: *const u8, mut args: ...) -> i32 {
    vasprintf_impl(strp, fmt, &mut args)
}
