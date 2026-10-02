//! `<libgen.h>`: POSIX's `basename` and `dirname`, which may write a NUL into
//! the path they are given and may answer static storage.

use core::ffi::c_char;
use core::ptr::addr_of_mut;

use super::u_strlen;

static mut DOT: [u8; 2] = *b".\0";
static mut SLASH: [u8; 2] = *b"/\0";

/// The final component of `path`, its trailing slashes cut off in place: "."
/// for a null or empty path, "/" for one of slashes alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn basename(path: *mut c_char) -> *mut c_char {
    let path = path.cast::<u8>();
    if path.is_null() || *path == 0 {
        return addr_of_mut!(DOT).cast();
    }
    let mut end = u_strlen(path);
    while end > 1 && *path.add(end - 1) == b'/' {
        end -= 1;
    }
    if end == 1 && *path == b'/' {
        return addr_of_mut!(SLASH).cast();
    }
    *path.add(end) = 0;
    let mut start = end;
    while start > 0 && *path.add(start - 1) != b'/' {
        start -= 1;
    }
    path.add(start).cast()
}

/// Everything of `path` before its final component, cut off in place: "."
/// for a path with no slash, "/" for one directly below the root.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dirname(path: *mut c_char) -> *mut c_char {
    let path = path.cast::<u8>();
    if path.is_null() || *path == 0 {
        return addr_of_mut!(DOT).cast();
    }
    let mut end = u_strlen(path);
    while end > 1 && *path.add(end - 1) == b'/' {
        end -= 1;
    }
    while end > 0 && *path.add(end - 1) != b'/' {
        end -= 1;
    }
    if end == 0 {
        return addr_of_mut!(DOT).cast();
    }
    while end > 1 && *path.add(end - 1) == b'/' {
        end -= 1;
    }
    *path.add(end) = 0;
    path.cast()
}
