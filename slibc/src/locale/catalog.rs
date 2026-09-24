//! `<nl_types.h>`: message catalogs, of which SlopOS installs none.

use core::ffi::{c_char, c_int, c_void};

use crate::errno::{EBADF, ENOENT, errno_set};

pub const NL_SETD: c_int = 1;
pub const NL_CAT_LOCALE: c_int = 1;

/// `(nl_catd)-1`, the handle `catopen` answers on failure.
const NO_CATALOG: *mut c_void = core::ptr::without_provenance_mut(usize::MAX);

/// `catopen(3)`: there is no catalog to open.
#[unsafe(no_mangle)]
pub extern "C" fn catopen(_name: *const c_char, _flag: c_int) -> *mut c_void {
    errno_set(ENOENT.raw());
    NO_CATALOG
}

/// `catgets(3)`: with no catalog open, every message is the caller's default.
#[unsafe(no_mangle)]
pub extern "C" fn catgets(
    _catd: *mut c_void,
    _set: c_int,
    _number: c_int,
    message: *const c_char,
) -> *mut c_char {
    errno_set(EBADF.raw());
    message.cast_mut()
}

/// `catclose(3)`: no handle `catopen` answered is open.
#[unsafe(no_mangle)]
pub extern "C" fn catclose(_catd: *mut c_void) -> c_int {
    errno_set(EBADF.raw());
    -1
}
