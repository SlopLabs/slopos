//! The only locale is POSIX's, whose case mapping is ASCII's, so the `_l`
//! forms ignore theirs.

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

#[unsafe(no_mangle)]
pub extern "C" fn ffs(i: c_int) -> c_int {
    if i == 0 {
        0
    } else {
        i.trailing_zeros() as c_int + 1
    }
}

/// `bcopy(3)`: `memmove` with the arguments the other way round.
///
/// # Safety
/// `src` and `dst` address `n` bytes each; they may overlap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bcopy(
    src: *const core::ffi::c_void,
    dst: *mut core::ffi::c_void,
    n: usize,
) {
    core::ptr::copy(src as *const u8, dst as *mut u8, n);
}

/// `bzero(3)`.
///
/// # Safety
/// `s` addresses `n` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn bzero(s: *mut core::ffi::c_void, n: usize) {
    core::ptr::write_bytes(s as *mut u8, 0, n);
}
