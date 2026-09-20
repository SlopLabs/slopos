//! `<locale.h>` and `<langinfo.h>`, over the one locale SlopOS has.
//!
//! [`setlocale`] accepts `""`, `"C"` and `"POSIX"` and answers NULL for
//! anything else, which C permits: answering `"en_US.UTF-8"` while behaving
//! as the C locale would be a lie the caller cannot detect.
//!
//! The `nl_item` numbering is musl's `(category << 16) | index` rather than
//! glibc's flat enum — the two schemes disagree and one had to be picked —
//! except for `CODESET`, which is 14 in both because programs hard-code it.
//!
//! No `locale_t`, `newlocale`, `uselocale` or `_l` family: libc++ is built
//! with `LIBCXX_ENABLE_LOCALIZATION` and `LIBCXX_ENABLE_WIDE_CHARACTERS`
//! off, and `build_userland.sh` puts its include directory ahead of slibc's,
//! so a C++ translation unit reaching `<locale.h>`, `<langinfo.h>` or
//! `<wchar.h>` takes libc++'s own `#error`. These headers are C-only here.

#![allow(non_camel_case_types)]

use core::cell::SyncUnsafeCell;
use core::ffi::{c_char, c_int};

use slopos_slibc_core::strftime;

use crate::string::{slice_from_cstr, u_strlen};

pub type nl_item = c_int;

// glibc's category numbering, which musl matches, and which is therefore what
// a ported program's hard-coded constant already is.

pub const LC_CTYPE: c_int = 0;
pub const LC_NUMERIC: c_int = 1;
pub const LC_TIME: c_int = 2;
pub const LC_COLLATE: c_int = 3;
pub const LC_MONETARY: c_int = 4;
pub const LC_MESSAGES: c_int = 5;
pub const LC_ALL: c_int = 6;

pub const CODESET: nl_item = 14;

pub const RADIXCHAR: nl_item = 0x10000;
pub const THOUSEP: nl_item = 0x10001;

pub const ABDAY_1: nl_item = 0x20000;
pub const ABDAY_2: nl_item = 0x20001;
pub const ABDAY_3: nl_item = 0x20002;
pub const ABDAY_4: nl_item = 0x20003;
pub const ABDAY_5: nl_item = 0x20004;
pub const ABDAY_6: nl_item = 0x20005;
pub const ABDAY_7: nl_item = 0x20006;
pub const DAY_1: nl_item = 0x20007;
pub const DAY_2: nl_item = 0x20008;
pub const DAY_3: nl_item = 0x20009;
pub const DAY_4: nl_item = 0x2000A;
pub const DAY_5: nl_item = 0x2000B;
pub const DAY_6: nl_item = 0x2000C;
pub const DAY_7: nl_item = 0x2000D;
pub const ABMON_1: nl_item = 0x2000E;
pub const ABMON_2: nl_item = 0x2000F;
pub const ABMON_3: nl_item = 0x20010;
pub const ABMON_4: nl_item = 0x20011;
pub const ABMON_5: nl_item = 0x20012;
pub const ABMON_6: nl_item = 0x20013;
pub const ABMON_7: nl_item = 0x20014;
pub const ABMON_8: nl_item = 0x20015;
pub const ABMON_9: nl_item = 0x20016;
pub const ABMON_10: nl_item = 0x20017;
pub const ABMON_11: nl_item = 0x20018;
pub const ABMON_12: nl_item = 0x20019;
pub const MON_1: nl_item = 0x2001A;
pub const MON_2: nl_item = 0x2001B;
pub const MON_3: nl_item = 0x2001C;
pub const MON_4: nl_item = 0x2001D;
pub const MON_5: nl_item = 0x2001E;
pub const MON_6: nl_item = 0x2001F;
pub const MON_7: nl_item = 0x20020;
pub const MON_8: nl_item = 0x20021;
pub const MON_9: nl_item = 0x20022;
pub const MON_10: nl_item = 0x20023;
pub const MON_11: nl_item = 0x20024;
pub const MON_12: nl_item = 0x20025;
pub const AM_STR: nl_item = 0x20026;
pub const PM_STR: nl_item = 0x20027;
pub const D_T_FMT: nl_item = 0x20028;
pub const D_FMT: nl_item = 0x20029;
pub const T_FMT: nl_item = 0x2002A;
pub const T_FMT_AMPM: nl_item = 0x2002B;
pub const ERA: nl_item = 0x2002C;
pub const ERA_D_FMT: nl_item = 0x2002E;
pub const ALT_DIGITS: nl_item = 0x2002F;
pub const ERA_D_T_FMT: nl_item = 0x20030;
pub const ERA_T_FMT: nl_item = 0x20031;

pub const CRNCYSTR: nl_item = 0x4000F;

pub const YESEXPR: nl_item = 0x50000;
pub const NOEXPR: nl_item = 0x50001;

#[repr(C)]
pub struct lconv {
    pub decimal_point: *mut c_char,
    pub thousands_sep: *mut c_char,
    pub grouping: *mut c_char,
    pub int_curr_symbol: *mut c_char,
    pub currency_symbol: *mut c_char,
    pub mon_decimal_point: *mut c_char,
    pub mon_thousands_sep: *mut c_char,
    pub mon_grouping: *mut c_char,
    pub positive_sign: *mut c_char,
    pub negative_sign: *mut c_char,
    pub int_frac_digits: c_char,
    pub frac_digits: c_char,
    pub p_cs_precedes: c_char,
    pub p_sep_by_space: c_char,
    pub n_cs_precedes: c_char,
    pub n_sep_by_space: c_char,
    pub p_sign_posn: c_char,
    pub n_sign_posn: c_char,
    pub int_p_cs_precedes: c_char,
    pub int_p_sep_by_space: c_char,
    pub int_n_cs_precedes: c_char,
    pub int_n_sep_by_space: c_char,
    pub int_p_sign_posn: c_char,
    pub int_n_sign_posn: c_char,
}

// The object is a single process-wide constant that `localeconv` hands out by
// pointer; there is nothing to race on because nothing ever writes it.
unsafe impl Sync for lconv {}

/// `CHAR_MAX` in a `char` field of `lconv` is C's "not specified in this
/// locale", which is every monetary field of the C locale.
const UNSPECIFIED: c_char = c_char::MAX;

static EMPTY: [u8; 1] = *b"\0";
static DOT: [u8; 2] = *b".\0";
static UTF8: [u8; 6] = *b"UTF-8\0";
static C_NAME: [u8; 2] = *b"C\0";
static YES_RE: [u8; 6] = *b"^[yY]\0";
static NO_RE: [u8; 6] = *b"^[nN]\0";

static C_LCONV: SyncUnsafeCell<lconv> = SyncUnsafeCell::new(lconv {
    decimal_point: DOT.as_ptr() as *mut c_char,
    thousands_sep: EMPTY.as_ptr() as *mut c_char,
    grouping: EMPTY.as_ptr() as *mut c_char,
    int_curr_symbol: EMPTY.as_ptr() as *mut c_char,
    currency_symbol: EMPTY.as_ptr() as *mut c_char,
    mon_decimal_point: EMPTY.as_ptr() as *mut c_char,
    mon_thousands_sep: EMPTY.as_ptr() as *mut c_char,
    mon_grouping: EMPTY.as_ptr() as *mut c_char,
    positive_sign: EMPTY.as_ptr() as *mut c_char,
    negative_sign: EMPTY.as_ptr() as *mut c_char,
    int_frac_digits: UNSPECIFIED,
    frac_digits: UNSPECIFIED,
    p_cs_precedes: UNSPECIFIED,
    p_sep_by_space: UNSPECIFIED,
    n_cs_precedes: UNSPECIFIED,
    n_sep_by_space: UNSPECIFIED,
    p_sign_posn: UNSPECIFIED,
    n_sign_posn: UNSPECIFIED,
    int_p_cs_precedes: UNSPECIFIED,
    int_p_sep_by_space: UNSPECIFIED,
    int_n_cs_precedes: UNSPECIFIED,
    int_n_sep_by_space: UNSPECIFIED,
    int_p_sign_posn: UNSPECIFIED,
    int_n_sign_posn: UNSPECIFIED,
});

/// Long enough for `"Wednesday"`, `"September"` and `"%a %b %e %T %Y"`; the
/// `assert!` in [`cstr`] is what fails the build if a table outgrows it.
const NL_STR_MAX: usize = 16;

type NlStr = [u8; NL_STR_MAX];

const fn cstr(s: &str) -> NlStr {
    let bytes = s.as_bytes();
    assert!(bytes.len() < NL_STR_MAX);
    let mut out = [0u8; NL_STR_MAX];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i];
        i += 1;
    }
    out
}

const fn cstr_table<const N: usize>(src: [&str; N]) -> [NlStr; N] {
    let mut out = [[0u8; NL_STR_MAX]; N];
    let mut i = 0;
    while i < N {
        out[i] = cstr(src[i]);
        i += 1;
    }
    out
}

static ABDAY_STR: [NlStr; 7] = cstr_table(strftime::ABDAY);
static DAY_STR: [NlStr; 7] = cstr_table(strftime::DAY);
static ABMON_STR: [NlStr; 12] = cstr_table(strftime::ABMON);
static MON_STR: [NlStr; 12] = cstr_table(strftime::MON);
static AM_PM_STR: [NlStr; 2] = cstr_table(strftime::AM_PM);
static D_T_FMT_STR: NlStr = cstr(strftime::D_T_FMT);
static D_FMT_STR: NlStr = cstr(strftime::D_FMT);
static T_FMT_STR: NlStr = cstr(strftime::T_FMT);
static T_FMT_AMPM_STR: NlStr = cstr(strftime::T_FMT_AMPM);

/// `setlocale(3)`. A NULL `locale` queries and always answers `"C"`; `""`,
/// `"C"` and `"POSIX"` select it; anything else fails with no state change.
/// The answer is a static, so it outlives every later call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setlocale(category: c_int, locale: *const c_char) -> *mut c_char {
    if !(LC_CTYPE..=LC_ALL).contains(&category) {
        return core::ptr::null_mut();
    }
    if locale.is_null() {
        return C_NAME.as_ptr() as *mut c_char;
    }
    let name = locale as *const u8;
    let requested = slice_from_cstr(name, u_strlen(name));
    match requested {
        b"" | b"C" | b"POSIX" => C_NAME.as_ptr() as *mut c_char,
        _ => core::ptr::null_mut(),
    }
}

/// `localeconv(3)`. One static object; C's return type is non-const but POSIX
/// forbids the caller modifying it.
#[unsafe(no_mangle)]
pub extern "C" fn localeconv() -> *mut lconv {
    C_LCONV.get()
}

/// `nl_langinfo(3)`. An unrecognised item answers `""`, never NULL.
#[unsafe(no_mangle)]
pub extern "C" fn nl_langinfo(item: nl_item) -> *mut c_char {
    langinfo(item) as *mut c_char
}

fn langinfo(item: nl_item) -> *const u8 {
    if item < 0 {
        return EMPTY.as_ptr();
    }
    let category = item >> 16;
    let index = (item & 0xffff) as usize;
    match category {
        LC_CTYPE if index == CODESET as usize => UTF8.as_ptr(),
        LC_NUMERIC if index == 0 => DOT.as_ptr(),
        LC_TIME => time_item(index),
        LC_MESSAGES if index == 0 => YES_RE.as_ptr(),
        LC_MESSAGES if index == 1 => NO_RE.as_ptr(),
        _ => EMPTY.as_ptr(),
    }
}

/// `ERA*` and `ALT_DIGITS` are empty in the C locale — which is what makes
/// `%E` and `%O` fall back — so they share the arm for an item that has none.
fn time_item(index: usize) -> *const u8 {
    match index {
        0..=6 => ABDAY_STR[index].as_ptr(),
        7..=13 => DAY_STR[index - 7].as_ptr(),
        14..=25 => ABMON_STR[index - 14].as_ptr(),
        26..=37 => MON_STR[index - 26].as_ptr(),
        38 | 39 => AM_PM_STR[index - 38].as_ptr(),
        40 => D_T_FMT_STR.as_ptr(),
        41 => D_FMT_STR.as_ptr(),
        42 => T_FMT_STR.as_ptr(),
        43 => T_FMT_AMPM_STR.as_ptr(),
        _ => EMPTY.as_ptr(),
    }
}

// `lconv` is declared twice — here and in the target's `libc` — and a
// disagreement is a miscompile rather than a link error.

const _: () = assert!(size_of::<lconv>() == 96);
const _: () = assert!(align_of::<lconv>() == 8);
const _: () = assert!(core::mem::offset_of!(lconv, decimal_point) == 0);
const _: () = assert!(core::mem::offset_of!(lconv, negative_sign) == 72);
const _: () = assert!(core::mem::offset_of!(lconv, int_frac_digits) == 80);
const _: () = assert!(core::mem::offset_of!(lconv, int_n_sign_posn) == 93);
const _: () = assert!(UNSPECIFIED == 127);

// The item numbering has to decode back to the categories it was built from.
const _: () = assert!(ABDAY_1 >> 16 == LC_TIME);
const _: () = assert!(RADIXCHAR >> 16 == LC_NUMERIC);
const _: () = assert!(CRNCYSTR >> 16 == LC_MONETARY);
const _: () = assert!(YESEXPR >> 16 == LC_MESSAGES);
const _: () = assert!(CODESET >> 16 == LC_CTYPE);
