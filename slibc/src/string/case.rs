//! `<strings.h>`: case-insensitive comparison and `ffs`.
//!
//! The only locale is the POSIX one, whose case mapping is ASCII's, so the
//! `_l` forms take a locale and read nothing from it.

use core::ffi::c_int;

use crate::locale::object::locale_t;

/// # Safety
/// `a` and `b` are NUL-terminated, or at least `n` bytes long.
unsafe fn compare(a: *const u8, b: *const u8, n: usize) -> c_int {
    for i in 0..n {
        let x = (*a.add(i)).to_ascii_lowercase();
        let y = (*b.add(i)).to_ascii_lowercase();
        if x != y || x == 0 {
            return c_int::from(x) - c_int::from(y);
        }
    }
    0
}

/// # Safety
/// Both arguments are NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcasecmp(a: *const u8, b: *const u8) -> c_int {
    compare(a, b, usize::MAX)
}

/// # Safety
/// Both arguments are NUL-terminated or at least `n` bytes long.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncasecmp(a: *const u8, b: *const u8, n: usize) -> c_int {
    compare(a, b, n)
}

/// # Safety
/// As [`strcasecmp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcasecmp_l(a: *const u8, b: *const u8, _loc: locale_t) -> c_int {
    compare(a, b, usize::MAX)
}

/// # Safety
/// As [`strncasecmp`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strncasecmp_l(
    a: *const u8,
    b: *const u8,
    n: usize,
    _loc: locale_t,
) -> c_int {
    compare(a, b, n)
}

/// The 1-based index of the least significant set bit, 0 for 0.
#[unsafe(no_mangle)]
pub extern "C" fn ffs(i: c_int) -> c_int {
    if i == 0 {
        0
    } else {
        i.trailing_zeros() as c_int + 1
    }
}
