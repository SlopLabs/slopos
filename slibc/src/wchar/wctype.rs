//! `<wctype.h>` — wide-character classification in the C locale.
//!
//! The C locale classifies the basic character set and nothing else, so each
//! predicate here is its `<ctype.h>` counterpart over a code point that fits
//! in ASCII. A wide character outside it belongs to no class, which is what
//! glibc's C locale answers too — the Unicode tables musl carries describe a
//! locale SlopOS does not have.

use core::ffi::{c_char, c_int};

use super::{WEOF, wint_t};
use crate::string::{slice_from_cstr, u_strlen};

pub type wctype_t = core::ffi::c_ulong;
pub type wctrans_t = c_int;

const CLASSES: [&[u8]; 12] = [
    b"alnum", b"alpha", b"blank", b"cntrl", b"digit", b"graph", b"lower", b"print", b"punct",
    b"space", b"upper", b"xdigit",
];

const TRANSFORMS: [&[u8]; 2] = [b"tolower", b"toupper"];

fn narrow(c: wint_t) -> c_int {
    if c == WEOF || c > 0x7f {
        return -1;
    }
    c as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn iswalnum(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isalnum(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswalpha(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isalpha(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswblank(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isblank(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswcntrl(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::iscntrl(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswdigit(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isdigit(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswgraph(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isgraph(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswlower(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::islower(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswprint(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isprint(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswpunct(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::ispunct(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswspace(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isspace(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswupper(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isupper(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn iswxdigit(c: wint_t) -> c_int {
    match narrow(c) {
        -1 => 0,
        b => crate::ctype::isxdigit(b),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn towlower(c: wint_t) -> wint_t {
    match narrow(c) {
        -1 => c,
        b => crate::ctype::tolower(b) as wint_t,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn towupper(c: wint_t) -> wint_t {
    match narrow(c) {
        -1 => c,
        b => crate::ctype::toupper(b) as wint_t,
    }
}

/// `wctype(3)`. Answers 0 for a name that is not one of the twelve classes,
/// which C reserves as the value no character belongs to.
///
/// # Safety
/// `name` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wctype(name: *const c_char) -> wctype_t {
    lookup(name, &CLASSES) as wctype_t
}

#[unsafe(no_mangle)]
pub extern "C" fn iswctype(c: wint_t, desc: wctype_t) -> c_int {
    match desc {
        1 => iswalnum(c),
        2 => iswalpha(c),
        3 => iswblank(c),
        4 => iswcntrl(c),
        5 => iswdigit(c),
        6 => iswgraph(c),
        7 => iswlower(c),
        8 => iswprint(c),
        9 => iswpunct(c),
        10 => iswspace(c),
        11 => iswupper(c),
        12 => iswxdigit(c),
        _ => 0,
    }
}

/// # Safety
/// `name` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wctrans(name: *const c_char) -> wctrans_t {
    lookup(name, &TRANSFORMS)
}

#[unsafe(no_mangle)]
pub extern "C" fn towctrans(c: wint_t, map: wctrans_t) -> wint_t {
    match map {
        1 => towlower(c),
        2 => towupper(c),
        _ => c,
    }
}

unsafe fn lookup(name: *const c_char, table: &[&[u8]]) -> c_int {
    if name.is_null() {
        return 0;
    }
    let bytes = name as *const u8;
    let text = slice_from_cstr(bytes, u_strlen(bytes));
    match table.iter().position(|entry| *entry == text) {
        Some(index) => index as c_int + 1,
        None => 0,
    }
}
