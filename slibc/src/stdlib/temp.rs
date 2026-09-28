//! `mkstemp`, `mkostemp` and `mkdtemp`: a unique name made from a template's
//! trailing `XXXXXX`.

use core::ffi::{c_char, c_int};

use crate::errno::{EEXIST, EINVAL, Errno, errno_set};
use crate::ffi::{O_CREAT, O_EXCL, O_RDWR};
use crate::pal::{Pal, Sys};

const SUFFIX: usize = 6;
/// Names tried before giving up with `EEXIST`.
const ATTEMPTS: usize = 100;
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";

/// Fills the template's `XXXXXX` afresh until `create` stops answering
/// `EEXIST`. A template that does not end in `XXXXXX` is `EINVAL`, untouched.
///
/// # Safety
/// `template` is a writable NUL-terminated C string.
unsafe fn fill_and_create(
    template: *mut c_char,
    create: impl Fn(*const u8) -> Result<c_int, Errno>,
) -> c_int {
    if template.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let len = crate::string::u_strlen(template as *const u8);
    if len < SUFFIX {
        errno_set(EINVAL.raw());
        return -1;
    }
    let tail = core::slice::from_raw_parts_mut((template as *mut u8).add(len - SUFFIX), SUFFIX);
    if tail.iter().any(|&b| b != b'X') {
        errno_set(EINVAL.raw());
        return -1;
    }
    for _ in 0..ATTEMPTS {
        let mut random = [0u8; SUFFIX];
        if crate::conf::getentropy(random.as_mut_ptr().cast(), SUFFIX) != 0 {
            return -1;
        }
        for (slot, r) in tail.iter_mut().zip(random) {
            *slot = ALPHABET[(r & 63) as usize];
        }
        match create(template as *const u8) {
            Ok(fd) => return fd,
            Err(e) if e == EEXIST => {}
            Err(e) => {
                errno_set(e.raw());
                return -1;
            }
        }
    }
    errno_set(EEXIST.raw());
    -1
}

/// `mkostemp(3)`: [`mkstemp`] with `flags` (`O_CLOEXEC`, `O_APPEND`, ...)
/// added to the open.
///
/// # Safety
/// `template` is a writable NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkostemp(template: *mut c_char, flags: c_int) -> c_int {
    fill_and_create(template, |path| {
        Sys::open(path, O_RDWR | O_CREAT | O_EXCL | flags, 0o600)
    })
}

/// `mkstemp(3)`.
///
/// # Safety
/// `template` is a writable NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkstemp(template: *mut c_char) -> c_int {
    mkostemp(template, 0)
}

/// `mkdtemp(3)`: the directory is mode 0700. Returns `template`, or null.
///
/// # Safety
/// `template` is a writable NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mkdtemp(template: *mut c_char) -> *mut c_char {
    let rc = fill_and_create(template, |path| Sys::mkdir(path, 0o700).map(|()| 0));
    if rc < 0 {
        core::ptr::null_mut()
    } else {
        template
    }
}
