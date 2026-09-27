//! `<iconv.h>`, over `slopos_slibc_core::iconv`.
//!
//! Every codeset is stateless, so a descriptor is its two codesets and
//! nothing else, and it is spelled in the pointer value itself: `iconv_open`
//! allocates nothing and `iconv_close` has nothing to free.

use core::ffi::{c_char, c_int, c_void};

use slopos_slibc_core::iconv::{self, Charset, Decoded, Encoded};

use crate::errno::{E2BIG, EBADF, EILSEQ, EINVAL, errno_set};
use crate::string::u_strlen;

const CHARSETS: [Charset; 7] = [
    Charset::Utf8,
    Charset::Ascii,
    Charset::Latin1,
    Charset::Utf16Le,
    Charset::Utf16Be,
    Charset::Utf32Le,
    Charset::Utf32Be,
];

const INVALID: *mut c_void = usize::MAX as *mut c_void;

fn index(charset: Charset) -> usize {
    CHARSETS.iter().position(|&c| c == charset).unwrap_or(0)
}

fn descriptor(cd: *mut c_void) -> Option<(Charset, Charset)> {
    let raw = cd as usize;
    let from = raw >> 8;
    let to = raw & 0xff;
    if raw >> 16 != 0 || from == 0 || to == 0 {
        return None;
    }
    Some((*CHARSETS.get(from - 1)?, *CHARSETS.get(to - 1)?))
}

/// `(iconv_t)-1` with `EINVAL` for a codeset this library cannot convert.
///
/// # Safety
/// Both names are NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iconv_open(tocode: *const c_char, fromcode: *const c_char) -> *mut c_void {
    let name = |s: *const c_char| core::slice::from_raw_parts(s.cast::<u8>(), u_strlen(s.cast()));
    if tocode.is_null() || fromcode.is_null() {
        errno_set(EINVAL.raw());
        return INVALID;
    }
    match (iconv::charset(name(fromcode)), iconv::charset(name(tocode))) {
        (Some(from), Some(to)) => (((index(from) + 1) << 8) | (index(to) + 1)) as *mut c_void,
        _ => {
            errno_set(EINVAL.raw());
            INVALID
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iconv_close(cd: *mut c_void) -> c_int {
    if descriptor(cd).is_none() {
        errno_set(EBADF.raw());
        return -1;
    }
    0
}

/// Converts as much of the input as fits. A character the target codeset
/// has no identical character for becomes `?` and is counted in the return
/// value, POSIX's "implementation-defined conversion".
///
/// # Safety
/// The four pointers describe live buffers as POSIX has them.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iconv(
    cd: *mut c_void,
    inbuf: *mut *mut c_char,
    inbytesleft: *mut usize,
    outbuf: *mut *mut c_char,
    outbytesleft: *mut usize,
) -> usize {
    let Some((from, to)) = descriptor(cd) else {
        errno_set(EBADF.raw());
        return usize::MAX;
    };
    // No shift state to return to the initial one.
    if inbuf.is_null() || (*inbuf).is_null() {
        return 0;
    }
    if outbuf.is_null() || (*outbuf).is_null() || inbytesleft.is_null() || outbytesleft.is_null() {
        errno_set(E2BIG.raw());
        return usize::MAX;
    }
    let mut substituted = 0usize;
    while *inbytesleft > 0 {
        let input = core::slice::from_raw_parts((*inbuf).cast::<u8>(), *inbytesleft);
        let (scalar, took) = match iconv::decode(from, input) {
            Decoded::Scalar(scalar, took) => (scalar, took),
            Decoded::Incomplete => {
                errno_set(EINVAL.raw());
                return usize::MAX;
            }
            Decoded::Invalid => {
                errno_set(EILSEQ.raw());
                return usize::MAX;
            }
        };
        let output = core::slice::from_raw_parts_mut((*outbuf).cast::<u8>(), *outbytesleft);
        let wrote = match iconv::encode(to, scalar, output) {
            Encoded::Wrote(n) => n,
            Encoded::Unrepresentable => match iconv::encode(to, u32::from(b'?'), output) {
                Encoded::Wrote(n) => {
                    substituted += 1;
                    n
                }
                _ => {
                    errno_set(E2BIG.raw());
                    return usize::MAX;
                }
            },
            Encoded::NoRoom => {
                errno_set(E2BIG.raw());
                return usize::MAX;
            }
        };
        *inbuf = (*inbuf).add(took);
        *inbytesleft -= took;
        *outbuf = (*outbuf).add(wrote);
        *outbytesleft -= wrote;
    }
    substituted
}
