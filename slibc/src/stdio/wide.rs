//! Wide stdio, over the same bytes the narrow calls move.
//!
//! A stream carries no orientation here. C leaves mixing wide and byte calls
//! on one stream undefined because the two normally use different buffers;
//! these share one, encoding to and from UTF-8 at the edge, so the mixture is
//! defined and [`fwide`] has nothing to report.
//!
//! The formatting family transcodes its wide template to UTF-8 and runs the
//! narrow engine over it, because C gives the two templates the same
//! conversions — `%s` takes a multibyte string in both, `%ls` a wide one.
//! What the transcode does not carry is the count: these return wide
//! characters where the narrow engine returns bytes.

use core::ffi::{VaList, c_int, c_void};

use slopos_slibc_core::utf8::{self, MbState, Step};

use super::chars::{fgetc_unlocked, fputc_unlocked, ungetc_unlocked};
use super::file::{flockfile, funlockfile};
use super::printf::format_to_cb;
use super::{EOF, FILE, FILE_FLAG_ERR, streams};
use crate::errno::{EILSEQ, ENOMEM, errno_set};
use crate::ffi::size_t;
use crate::wchar::{WEOF, wchar_t, wint_t};

/// Marks `stream` as having hit an encoding error, which C99 7.24.3.1 asks
/// for beside the `EILSEQ`. An incomplete character at end of input is one of
/// those, not a clean end of file.
unsafe fn encoding_error(stream: *mut FILE) -> wint_t {
    if !stream.is_null() {
        (*stream).flags |= FILE_FLAG_ERR;
    }
    errno_set(EILSEQ.raw());
    WEOF
}

unsafe fn fgetwc_locked(stream: *mut FILE) -> wint_t {
    let mut state = MbState::default();
    loop {
        let byte = fgetc_unlocked(stream);
        if byte == EOF {
            if state.is_initial() {
                return WEOF;
            }
            return encoding_error(stream);
        }
        match utf8::decode_step(&mut state, byte as u8) {
            Step::Done(scalar) => return scalar,
            Step::More => {}
            Step::Invalid => return encoding_error(stream),
        }
    }
}

unsafe fn fputwc_locked(wc: wchar_t, stream: *mut FILE) -> wint_t {
    let mut bytes = [0u8; 4];
    let Some(len) = utf8::encode(wc as u32, &mut bytes) else {
        return encoding_error(stream);
    };
    for byte in &bytes[..len] {
        if fputc_unlocked(*byte as c_int, stream) == EOF {
            return WEOF;
        }
    }
    wc as wint_t
}

/// `fgetwc(3)`. An encoding error is `WEOF` with `EILSEQ` and the stream's
/// error indicator set; the bytes that caused it are consumed, which is what
/// C leaves unspecified.
///
/// # Safety
/// `stream` is an open readable stream or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgetwc(stream: *mut FILE) -> wint_t {
    flockfile(stream);
    let wc = fgetwc_locked(stream);
    funlockfile(stream);
    wc
}

/// # Safety
/// As [`fgetwc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getwc(stream: *mut FILE) -> wint_t {
    fgetwc(stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getwchar() -> wint_t {
    fgetwc(streams::stdin_file())
}

/// # Safety
/// `stream` is an open writable stream or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputwc(wc: wchar_t, stream: *mut FILE) -> wint_t {
    flockfile(stream);
    let written = fputwc_locked(wc, stream);
    funlockfile(stream);
    written
}

/// # Safety
/// As [`fputwc`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn putwc(wc: wchar_t, stream: *mut FILE) -> wint_t {
    fputwc(wc, stream)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn putwchar(wc: wchar_t) -> wint_t {
    fputwc(wc, streams::stdout_file())
}

/// `ungetwc(3)`. The character's whole encoding goes back, so a following
/// byte read sees it too — and it goes back whole or not at all, since a
/// half-pushed sequence would be bytes the stream never held.
///
/// # Safety
/// `stream` is an open readable stream or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ungetwc(wc: wint_t, stream: *mut FILE) -> wint_t {
    if wc == WEOF || stream.is_null() {
        return WEOF;
    }
    let mut bytes = [0u8; 4];
    let Some(len) = utf8::encode(wc, &mut bytes) else {
        return encoding_error(stream);
    };
    flockfile(stream);
    let room = (*stream).ungot.len() - (*stream).ungot_len >= len;
    if room {
        for byte in bytes[..len].iter().rev() {
            ungetc_unlocked(*byte as c_int, stream);
        }
    }
    funlockfile(stream);
    if room { wc } else { WEOF }
}

/// `fgetws(3)`. `NULL` on a read or encoding error, as C99 7.24.3.2 asks,
/// which is what distinguishes it from a short line at end of file.
///
/// # Safety
/// `s` addresses `n` wide characters; `stream` is open and readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fgetws(s: *mut wchar_t, n: c_int, stream: *mut FILE) -> *mut wchar_t {
    if s.is_null() || n <= 0 {
        return core::ptr::null_mut();
    }
    let max = (n - 1) as usize;
    let mut at = 0usize;
    let mut ended = false;
    flockfile(stream);
    // `FILE_FLAG_ERR` is sticky until `clearerr`, so an inherited flag would
    // otherwise make a good line read as a failure, consumed either way.
    let held = take_error(stream);
    while at < max {
        let wc = fgetwc_locked(stream);
        if wc == WEOF {
            ended = true;
            break;
        }
        *s.add(at) = wc as wchar_t;
        at += 1;
        if wc == '\n' as wint_t {
            break;
        }
    }
    let failed = !stream.is_null() && (*stream).flags & FILE_FLAG_ERR != 0;
    restore_error(stream, held);
    funlockfile(stream);
    if failed || (ended && at == 0) {
        return core::ptr::null_mut();
    }
    *s.add(at) = 0;
    s
}

unsafe fn take_error(stream: *mut FILE) -> bool {
    if stream.is_null() {
        return false;
    }
    let held = (*stream).flags & FILE_FLAG_ERR != 0;
    (*stream).flags &= !FILE_FLAG_ERR;
    held
}

unsafe fn restore_error(stream: *mut FILE, held: bool) {
    if held && !stream.is_null() {
        (*stream).flags |= FILE_FLAG_ERR;
    }
}

/// # Safety
/// `s` is a NUL-terminated wide string; `stream` is open and writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fputws(s: *const wchar_t, stream: *mut FILE) -> c_int {
    if s.is_null() {
        return EOF;
    }
    flockfile(stream);
    let mut p = s;
    let mut status = 0;
    while *p != 0 {
        if fputwc_locked(*p, stream) == WEOF {
            status = EOF;
            break;
        }
        p = p.add(1);
    }
    funlockfile(stream);
    status
}

/// `fwide(3)`. Always 0; see the module note.
#[unsafe(no_mangle)]
pub extern "C" fn fwide(_stream: *mut FILE, _mode: c_int) -> c_int {
    0
}

/// The wide template as UTF-8, owned. The narrow engine walks a C string, so
/// the transcode has to be contiguous; four bytes per wide character is the
/// most UTF-8 spends.
struct NarrowFormat {
    bytes: *mut u8,
}

impl NarrowFormat {
    unsafe fn new(fmt: *const wchar_t) -> Option<Self> {
        let len = crate::wchar::wcslen(fmt);
        let bytes = crate::mem::malloc::alloc(len * 4 + 1) as *mut u8;
        if bytes.is_null() {
            errno_set(ENOMEM.raw());
            return None;
        }
        let mut at = 0usize;
        for index in 0..len {
            let mut encoded = [0u8; 4];
            let Some(used) = utf8::encode(*fmt.add(index) as u32, &mut encoded) else {
                crate::mem::malloc::dealloc(bytes as *mut c_void);
                errno_set(EILSEQ.raw());
                return None;
            };
            core::ptr::copy_nonoverlapping(encoded.as_ptr(), bytes.add(at), used);
            at += used;
        }
        *bytes.add(at) = 0;
        Some(NarrowFormat { bytes })
    }
}

impl Drop for NarrowFormat {
    fn drop(&mut self) {
        crate::mem::malloc::dealloc(self.bytes as *mut c_void);
    }
}

/// # Safety
/// `fmt` is a NUL-terminated wide string whose conversions match `ap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vfwprintf(
    stream: *mut FILE,
    fmt: *const wchar_t,
    mut ap: VaList<'_>,
) -> c_int {
    vfwprintf_impl(stream, fmt, &mut ap)
}

unsafe fn vfwprintf_impl(stream: *mut FILE, fmt: *const wchar_t, ap: &mut VaList<'_>) -> c_int {
    if stream.is_null() {
        return -1;
    }
    let Some(narrow) = NarrowFormat::new(fmt) else {
        return -1;
    };

    // C99 7.24.2.5 counts wide characters, not the bytes the narrow engine
    // emits. The stream takes the bytes; the count is of the sequences they
    // are, which is every byte that is not a continuation.
    let mut written = 0usize;
    let mut unencodable = false;
    flockfile(stream);
    format_to_cb(
        &mut |byte: u8| {
            if byte & 0xc0 != 0x80 {
                written += 1;
            }
            fputc_unlocked(byte as c_int, stream);
        },
        narrow.bytes,
        ap,
        &mut unencodable,
    );
    funlockfile(stream);

    if unencodable {
        errno_set(EILSEQ.raw());
        return -1;
    }
    written as c_int
}

/// # Safety
/// As [`vfwprintf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fwprintf(stream: *mut FILE, fmt: *const wchar_t, mut args: ...) -> c_int {
    vfwprintf_impl(stream, fmt, &mut args)
}

/// # Safety
/// As [`vfwprintf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vwprintf(fmt: *const wchar_t, mut ap: VaList<'_>) -> c_int {
    vfwprintf_impl(streams::stdout_file(), fmt, &mut ap)
}

/// # Safety
/// As [`vfwprintf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wprintf(fmt: *const wchar_t, mut args: ...) -> c_int {
    vfwprintf_impl(streams::stdout_file(), fmt, &mut args)
}

/// `vswprintf(3)`. `n` counts wide characters, and unlike `snprintf` C makes
/// a result that does not fit an error rather than a length.
///
/// # Safety
/// `s` addresses `n` wide characters; `fmt`'s conversions match `ap`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vswprintf(
    s: *mut wchar_t,
    n: size_t,
    fmt: *const wchar_t,
    mut ap: VaList<'_>,
) -> c_int {
    vswprintf_impl(s, n, fmt, &mut ap)
}

unsafe fn vswprintf_impl(
    s: *mut wchar_t,
    n: size_t,
    fmt: *const wchar_t,
    ap: &mut VaList<'_>,
) -> c_int {
    if s.is_null() || n == 0 {
        return -1;
    }
    let Some(narrow) = NarrowFormat::new(fmt) else {
        return -1;
    };

    let mut state = MbState::default();
    let mut written = 0usize;
    let mut overflowed = false;
    let mut malformed = false;
    let mut unencodable = false;
    format_to_cb(
        &mut |byte: u8| match utf8::decode_step(&mut state, byte) {
            Step::Done(scalar) => {
                if written + 1 >= n {
                    overflowed = true;
                } else {
                    *s.add(written) = scalar as wchar_t;
                    written += 1;
                }
            }
            Step::More => {}
            Step::Invalid => malformed = true,
        },
        narrow.bytes,
        ap,
        &mut unencodable,
    );

    *s.add(written) = 0;
    if malformed || unencodable {
        errno_set(EILSEQ.raw());
        return -1;
    }
    if overflowed {
        return -1;
    }
    written as c_int
}

/// # Safety
/// As [`vswprintf`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn swprintf(
    s: *mut wchar_t,
    n: size_t,
    fmt: *const wchar_t,
    mut args: ...
) -> c_int {
    vswprintf_impl(s, n, fmt, &mut args)
}
