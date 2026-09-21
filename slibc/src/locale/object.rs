//! POSIX locale objects, and the `_l` entry points that take one.
//!
//! SlopOS has exactly one locale, so every `_l` function here is its base
//! function with the handle discarded — musl's shape, for musl's reason, and
//! not a placeholder: a second locale would be a second `lconv`, a second
//! collation order and a second `nl_langinfo` table, none of which exists.
//! [`newlocale`] still refuses a name [`super::setlocale`] refuses, so a
//! program asking for `en_US.UTF-8` is told no rather than handed the C
//! locale under that name.
//!
//! One object serves every handle. Nothing in it is mutable, so [`duplocale`]
//! answers the same pointer and [`freelocale`] has nothing to release.

use core::cell::SyncUnsafeCell;
use core::ffi::{c_char, c_int, c_long, c_longlong, c_ulong, c_ulonglong};
use core::ptr;

use super::{LC_ALL, LC_COLLATE, LC_CTYPE, LC_MESSAGES, LC_MONETARY, LC_NUMERIC, LC_TIME, lconv};
use crate::errno::{EINVAL, ENOENT, errno_set};
use crate::ffi::size_t;
use crate::string::{slice_from_cstr, u_strlen};
use crate::time::calendar::tm;
use crate::wchar::{wchar_t, wctrans_t, wctype_t, wint_t};

#[repr(C)]
pub struct __locale_struct {
    _opaque: u8,
}

pub type locale_t = *mut __locale_struct;

pub const LC_CTYPE_MASK: c_int = 1;
pub const LC_NUMERIC_MASK: c_int = 2;
pub const LC_TIME_MASK: c_int = 4;
pub const LC_COLLATE_MASK: c_int = 8;
pub const LC_MONETARY_MASK: c_int = 16;
pub const LC_MESSAGES_MASK: c_int = 32;
pub const LC_ALL_MASK: c_int = 63;

// The header generator emits a `pub const` verbatim rather than evaluating
// it, so each value is a literal and the shift it is says so here.
const _: () = assert!(LC_CTYPE_MASK == 1 << LC_CTYPE);
const _: () = assert!(LC_NUMERIC_MASK == 1 << LC_NUMERIC);
const _: () = assert!(LC_TIME_MASK == 1 << LC_TIME);
const _: () = assert!(LC_COLLATE_MASK == 1 << LC_COLLATE);
const _: () = assert!(LC_MONETARY_MASK == 1 << LC_MONETARY);
const _: () = assert!(LC_MESSAGES_MASK == 1 << LC_MESSAGES);
const _: () = assert!(LC_ALL_MASK == (1 << LC_ALL) - 1);

static C_LOCALE: SyncUnsafeCell<__locale_struct> =
    SyncUnsafeCell::new(__locale_struct { _opaque: 0 });

/// `((locale_t)-1)`, POSIX's designator for whatever `setlocale` selected.
const GLOBAL: locale_t = ptr::without_provenance_mut(usize::MAX);

pub fn c_locale() -> locale_t {
    C_LOCALE.get()
}

/// Before TLS is up there is one thread, so a static is its [`uselocale`]
/// slot. Null in either slot means the global locale.
static mut PRE_TLS_LOCALE: locale_t = ptr::null_mut();

unsafe fn thread_slot() -> *mut locale_t {
    if crate::thread::tls::tls_is_initialized() {
        &raw mut (*crate::thread::tcb::Tcb::current()).locale
    } else {
        &raw mut PRE_TLS_LOCALE
    }
}

/// `newlocale(3)`. `base` is not consumed: there is nothing to free.
///
/// # Safety
/// `name` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn newlocale(mask: c_int, name: *const c_char, _base: locale_t) -> locale_t {
    if mask & !LC_ALL_MASK != 0 || name.is_null() {
        errno_set(EINVAL.raw());
        return ptr::null_mut();
    }
    let bytes = name as *const u8;
    match slice_from_cstr(bytes, u_strlen(bytes)) {
        b"" | b"C" | b"POSIX" => C_LOCALE.get(),
        _ => {
            errno_set(ENOENT.raw());
            ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn duplocale(base: locale_t) -> locale_t {
    if base.is_null() {
        errno_set(EINVAL.raw());
        return ptr::null_mut();
    }
    C_LOCALE.get()
}

#[unsafe(no_mangle)]
pub extern "C" fn freelocale(_loc: locale_t) {}

/// `uselocale(3)`. A null argument queries without changing anything.
///
/// # Safety
/// `loc` is a handle this library minted, `LC_GLOBAL_LOCALE`, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn uselocale(loc: locale_t) -> locale_t {
    let slot = thread_slot();
    let previous = if (*slot).is_null() { GLOBAL } else { *slot };
    if !loc.is_null() {
        *slot = if loc == GLOBAL { ptr::null_mut() } else { loc };
    }
    previous
}

#[unsafe(no_mangle)]
pub extern "C" fn isalnum_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isalnum(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isalpha_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isalpha(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isblank_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isblank(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iscntrl_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::iscntrl(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isdigit_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isdigit(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isgraph_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isgraph(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn islower_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::islower(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isprint_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isprint(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn ispunct_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::ispunct(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isspace_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isspace(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isupper_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isupper(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn isxdigit_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::isxdigit(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn tolower_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::tolower(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn toupper_l(c: c_int, _loc: locale_t) -> c_int {
    crate::ctype::toupper(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswalnum_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswalnum(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswalpha_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswalpha(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswblank_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswblank(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswcntrl_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswcntrl(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswdigit_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswdigit(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswgraph_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswgraph(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswlower_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswlower(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswprint_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswprint(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswpunct_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswpunct(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswspace_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswspace(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswupper_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswupper(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswxdigit_l(c: wint_t, _loc: locale_t) -> c_int {
    crate::wchar::iswxdigit(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn towlower_l(c: wint_t, _loc: locale_t) -> wint_t {
    crate::wchar::towlower(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn towupper_l(c: wint_t, _loc: locale_t) -> wint_t {
    crate::wchar::towupper(c)
}

#[unsafe(no_mangle)]
pub extern "C" fn iswctype_l(c: wint_t, desc: wctype_t, _loc: locale_t) -> c_int {
    crate::wchar::iswctype(c, desc)
}

#[unsafe(no_mangle)]
pub extern "C" fn towctrans_l(c: wint_t, map: wctrans_t, _loc: locale_t) -> wint_t {
    crate::wchar::towctrans(c, map)
}

#[unsafe(no_mangle)]
pub extern "C" fn localeconv_l(_loc: locale_t) -> *mut lconv {
    super::localeconv()
}

#[unsafe(no_mangle)]
pub extern "C" fn nl_langinfo_l(item: super::nl_item, _loc: locale_t) -> *mut c_char {
    super::nl_langinfo(item)
}

/// # Safety
/// `name` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wctype_l(name: *const c_char, _loc: locale_t) -> wctype_t {
    crate::wchar::wctype(name)
}

/// # Safety
/// `name` is a NUL-terminated C string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wctrans_l(name: *const c_char, _loc: locale_t) -> wctrans_t {
    crate::wchar::wctrans(name)
}

/// # Safety
/// Both arguments are NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strcoll_l(a: *const u8, b: *const u8, _loc: locale_t) -> c_int {
    crate::string::strcoll(a, b)
}

/// # Safety
/// `src` is a NUL-terminated C string; `dst` addresses `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strxfrm_l(
    dst: *mut u8,
    src: *const u8,
    n: size_t,
    _loc: locale_t,
) -> size_t {
    crate::string::strxfrm(dst, src, n)
}

/// # Safety
/// As [`crate::time::calendar::strftime`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strftime_l(
    s: *mut c_char,
    max: size_t,
    format: *const c_char,
    tmp: *const tm,
    _loc: locale_t,
) -> size_t {
    crate::time::calendar::strftime(s, max, format, tmp)
}

/// # Safety
/// Both arguments are NUL-terminated wide strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscoll_l(a: *const wchar_t, b: *const wchar_t, _loc: locale_t) -> c_int {
    crate::wchar::wcscoll(a, b)
}

/// # Safety
/// `src` is a NUL-terminated wide string; `dst` addresses `n` characters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsxfrm_l(
    dst: *mut wchar_t,
    src: *const wchar_t,
    n: size_t,
    _loc: locale_t,
) -> size_t {
    crate::wchar::wcsxfrm(dst, src, n)
}

/// # Safety
/// As [`crate::string::convert::strtod`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtod_l(s: *const u8, endptr: *mut *const u8, _loc: locale_t) -> f64 {
    crate::string::convert::strtod(s, endptr)
}

/// # Safety
/// As [`crate::string::convert::strtof`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtof_l(s: *const u8, endptr: *mut *const u8, _loc: locale_t) -> f32 {
    crate::string::convert::strtof(s, endptr)
}

/// # Safety
/// As [`crate::string::convert::strtol`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtol_l(
    s: *const u8,
    endptr: *mut *const u8,
    base: c_int,
    _loc: locale_t,
) -> c_long {
    crate::string::convert::strtol(s, endptr, base)
}

/// # Safety
/// As [`crate::string::convert::strtoul`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoul_l(
    s: *const u8,
    endptr: *mut *const u8,
    base: c_int,
    _loc: locale_t,
) -> c_ulong {
    crate::string::convert::strtoul(s, endptr, base)
}

/// # Safety
/// As [`crate::string::convert::strtoll`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoll_l(
    s: *const u8,
    endptr: *mut *const u8,
    base: c_int,
    _loc: locale_t,
) -> c_longlong {
    crate::string::convert::strtoll(s, endptr, base)
}

/// # Safety
/// As [`crate::string::convert::strtoull`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn strtoull_l(
    s: *const u8,
    endptr: *mut *const u8,
    base: c_int,
    _loc: locale_t,
) -> c_ulonglong {
    crate::string::convert::strtoull(s, endptr, base)
}

/// # Safety
/// As [`crate::wchar::wcstod`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstod_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    _loc: locale_t,
) -> f64 {
    crate::wchar::wcstod(s, endptr)
}

/// # Safety
/// As [`crate::wchar::wcstof`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstof_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    _loc: locale_t,
) -> f32 {
    crate::wchar::wcstof(s, endptr)
}

/// # Safety
/// As [`crate::wchar::wcstol`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstol_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
    _loc: locale_t,
) -> c_long {
    crate::wchar::wcstol(s, endptr, base)
}

/// # Safety
/// As [`crate::wchar::wcstoul`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoul_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
    _loc: locale_t,
) -> c_ulong {
    crate::wchar::wcstoul(s, endptr, base)
}

/// # Safety
/// As [`crate::wchar::wcstoll`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoll_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
    _loc: locale_t,
) -> c_longlong {
    crate::wchar::wcstoll(s, endptr, base)
}

/// # Safety
/// As [`crate::wchar::wcstoull`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoull_l(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
    _loc: locale_t,
) -> c_ulonglong {
    crate::wchar::wcstoull(s, endptr, base)
}
