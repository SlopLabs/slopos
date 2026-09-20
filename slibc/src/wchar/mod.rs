//! `<wchar.h>` — wide characters as UTF-32, multibyte sequences as UTF-8.
//!
//! Classification is `<wctype.h>`'s and lives in [`wctype`]; the wide stdio
//! `<wchar.h>` also declares lives in [`crate::stdio::wide`], because it is
//! the byte streams' code with an encoder on the end.
//!
//! Wide `scanf` is absent. Nothing links against it, and the narrow engine it
//! would transcode into consumes its template and its stream together, so it
//! is a second parser rather than a second spelling.

#![allow(non_camel_case_types)]

pub mod wctype;

pub use wctype::{
    iswalnum, iswalpha, iswblank, iswcntrl, iswctype, iswdigit, iswgraph, iswlower, iswprint,
    iswpunct, iswspace, iswupper, iswxdigit, towctrans, towlower, towupper, wctrans, wctrans_t,
    wctype, wctype_t,
};

use core::ffi::{c_char, c_int, c_long, c_longlong, c_uint, c_ulong, c_ulonglong};

use slopos_slibc_core::utf8::{self, MbState, Step};

use crate::errno::{EILSEQ, EINVAL, ENOMEM, errno_set};
use crate::ffi::size_t;
use crate::stdio::EOF;

pub type wchar_t = c_int;
pub type wint_t = c_uint;

pub const WEOF: c_uint = 0xffff_ffff;

/// C's `mbstate_t`: `unsigned int __size[2]`, all-zero in the initial state.
/// The bits inside are [`MbState`]'s; only the size, the alignment and the
/// meaning of zero are ABI.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mbstate_t {
    pub __size: [c_uint; 2],
}

const MBSTATE_INITIAL: mbstate_t = mbstate_t { __size: [0, 0] };

/// `(size_t)-1`, the encoding-error return shared by the conversion family.
const MB_ERR: size_t = size_t::MAX;
/// `(size_t)-2`: the bytes given form a valid but incomplete character.
const MB_INCOMPLETE: size_t = size_t::MAX - 1;

// C gives every restartable function handed a null `mbstate_t *` its own
// internal state object. One shared object would let an interleaved
// `mbsrtowcs` finish the character `mbrtowc` had half-read.
static mut STATE_MBRTOWC: mbstate_t = MBSTATE_INITIAL;
static mut STATE_MBRLEN: mbstate_t = MBSTATE_INITIAL;
static mut STATE_WCRTOMB: mbstate_t = MBSTATE_INITIAL;
static mut STATE_MBSRTOWCS: mbstate_t = MBSTATE_INITIAL;
static mut STATE_WCSRTOMBS: mbstate_t = MBSTATE_INITIAL;

#[inline]
unsafe fn state_load(ps: *mut mbstate_t) -> MbState {
    MbState::from_raw((*ps).__size)
}

#[inline]
unsafe fn state_store(ps: *mut mbstate_t, state: MbState) {
    (*ps).__size = state.to_raw();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcslen(s: *const wchar_t) -> size_t {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsnlen(s: *const wchar_t, maxlen: size_t) -> size_t {
    if s.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while n < maxlen && *s.add(n) != 0 {
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wmemcpy(
    dst: *mut wchar_t,
    src: *const wchar_t,
    n: size_t,
) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    core::ptr::copy_nonoverlapping(src, dst, n);
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wmemmove(
    dst: *mut wchar_t,
    src: *const wchar_t,
    n: size_t,
) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    core::ptr::copy(src, dst, n);
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wmemset(dst: *mut wchar_t, c: wchar_t, n: size_t) -> *mut wchar_t {
    if dst.is_null() {
        return dst;
    }
    for i in 0..n {
        *dst.add(i) = c;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wmemcmp(a: *const wchar_t, b: *const wchar_t, n: size_t) -> c_int {
    if a.is_null() || b.is_null() {
        return 0;
    }
    for i in 0..n {
        let (av, bv) = (*a.add(i), *b.add(i));
        if av != bv {
            return if av < bv { -1 } else { 1 };
        }
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wmemchr(s: *const wchar_t, c: wchar_t, n: size_t) -> *const wchar_t {
    if s.is_null() {
        return core::ptr::null();
    }
    for i in 0..n {
        if *s.add(i) == c {
            return s.add(i);
        }
    }
    core::ptr::null()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscpy(dst: *mut wchar_t, src: *const wchar_t) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut i = 0usize;
    loop {
        let wc = *src.add(i);
        *dst.add(i) = wc;
        if wc == 0 {
            return dst;
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsncpy(
    dst: *mut wchar_t,
    src: *const wchar_t,
    n: size_t,
) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let mut i = 0usize;
    while i < n {
        let wc = *src.add(i);
        *dst.add(i) = wc;
        if wc == 0 {
            break;
        }
        i += 1;
    }
    while i < n {
        *dst.add(i) = 0;
        i += 1;
    }
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscat(dst: *mut wchar_t, src: *const wchar_t) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let end = dst.add(wcslen(dst));
    wcscpy(end, src);
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsncat(
    dst: *mut wchar_t,
    src: *const wchar_t,
    n: size_t,
) -> *mut wchar_t {
    if dst.is_null() || src.is_null() {
        return dst;
    }
    let end = dst.add(wcslen(dst));
    let mut i = 0usize;
    while i < n {
        let wc = *src.add(i);
        if wc == 0 {
            break;
        }
        *end.add(i) = wc;
        i += 1;
    }
    *end.add(i) = 0;
    dst
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscmp(a: *const wchar_t, b: *const wchar_t) -> c_int {
    let mut i = 0usize;
    loop {
        let (av, bv) = (*a.add(i), *b.add(i));
        if av != bv {
            return if av < bv { -1 } else { 1 };
        }
        if av == 0 {
            return 0;
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsncmp(a: *const wchar_t, b: *const wchar_t, n: size_t) -> c_int {
    let mut i = 0usize;
    while i < n {
        let (av, bv) = (*a.add(i), *b.add(i));
        if av != bv {
            return if av < bv { -1 } else { 1 };
        }
        if av == 0 {
            return 0;
        }
        i += 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcschr(s: *const wchar_t, c: wchar_t) -> *const wchar_t {
    if s.is_null() {
        return core::ptr::null();
    }
    let mut i = 0usize;
    loop {
        let wc = *s.add(i);
        if wc == c {
            return s.add(i);
        }
        if wc == 0 {
            return core::ptr::null();
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsrchr(s: *const wchar_t, c: wchar_t) -> *const wchar_t {
    if s.is_null() {
        return core::ptr::null();
    }
    let mut last = core::ptr::null();
    let mut i = 0usize;
    loop {
        let wc = *s.add(i);
        if wc == c {
            last = s.add(i);
        }
        if wc == 0 {
            return last;
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsstr(
    haystack: *const wchar_t,
    needle: *const wchar_t,
) -> *const wchar_t {
    if haystack.is_null() || needle.is_null() {
        return core::ptr::null();
    }
    if *needle == 0 {
        return haystack;
    }
    let mut i = 0usize;
    while *haystack.add(i) != 0 {
        let mut j = 0usize;
        loop {
            let nv = *needle.add(j);
            if nv == 0 {
                return haystack.add(i);
            }
            let hv = *haystack.add(i + j);
            if hv == 0 {
                return core::ptr::null();
            }
            if hv != nv {
                break;
            }
            j += 1;
        }
        i += 1;
    }
    core::ptr::null()
}

unsafe fn in_set(set: *const wchar_t, wc: wchar_t) -> bool {
    let mut i = 0usize;
    loop {
        let sv = *set.add(i);
        if sv == 0 {
            return false;
        }
        if sv == wc {
            return true;
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsspn(s: *const wchar_t, accept: *const wchar_t) -> size_t {
    if s.is_null() || accept.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 && in_set(accept, *s.add(n)) {
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscspn(s: *const wchar_t, reject: *const wchar_t) -> size_t {
    if s.is_null() || reject.is_null() {
        return 0;
    }
    let mut n = 0usize;
    while *s.add(n) != 0 && !in_set(reject, *s.add(n)) {
        n += 1;
    }
    n
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcspbrk(s: *const wchar_t, accept: *const wchar_t) -> *const wchar_t {
    if s.is_null() || accept.is_null() {
        return core::ptr::null();
    }
    let mut i = 0usize;
    loop {
        let wc = *s.add(i);
        if wc == 0 {
            return core::ptr::null();
        }
        if in_set(accept, wc) {
            return s.add(i);
        }
        i += 1;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstok(
    s: *mut wchar_t,
    delim: *const wchar_t,
    save: *mut *mut wchar_t,
) -> *mut wchar_t {
    if delim.is_null() || save.is_null() {
        return core::ptr::null_mut();
    }
    let mut p = if s.is_null() { *save } else { s };
    if p.is_null() {
        return core::ptr::null_mut();
    }
    while *p != 0 && in_set(delim, *p) {
        p = p.add(1);
    }
    if *p == 0 {
        *save = p;
        return core::ptr::null_mut();
    }
    let token = p;
    while *p != 0 && !in_set(delim, *p) {
        p = p.add(1);
    }
    if *p != 0 {
        *p = 0;
        p = p.add(1);
    }
    *save = p;
    token
}

/// `wcscoll(3)`. SlopOS has the C locale only, where the collation order is
/// the wide-character order, so this is `wcscmp` rather than a pretence at
/// collating.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcscoll(a: *const wchar_t, b: *const wchar_t) -> c_int {
    wcscmp(a, b)
}

/// `wcsxfrm(3)`. In the C locale the transformation that makes `wcscmp` agree
/// with `wcscoll` is the identity, so this copies and answers the source
/// length — which is what tells the caller its buffer was too small.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsxfrm(dst: *mut wchar_t, src: *const wchar_t, n: size_t) -> size_t {
    if src.is_null() {
        return 0;
    }
    let len = wcslen(src);
    if dst.is_null() {
        return len;
    }
    if len < n {
        wcscpy(dst, src);
    } else if n > 0 {
        wmemcpy(dst, src, n - 1);
        *dst.add(n - 1) = 0;
    }
    len
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsdup(s: *const wchar_t) -> *mut wchar_t {
    if s.is_null() {
        return core::ptr::null_mut();
    }
    let len = wcslen(s);
    let out = crate::mem::malloc::alloc((len + 1) * size_of::<wchar_t>()).cast::<wchar_t>();
    if out.is_null() {
        errno_set(ENOMEM.raw());
        return core::ptr::null_mut();
    }
    core::ptr::copy_nonoverlapping(s, out, len + 1);
    out
}

enum Decoded {
    Char { scalar: u32, len: size_t },
    Incomplete,
    Invalid,
}

unsafe fn decode_one(state: &mut MbState, s: *const u8, n: size_t) -> Decoded {
    let mut i = 0usize;
    while i < n {
        let byte = *s.add(i);
        i += 1;
        match utf8::decode_step(state, byte) {
            Step::Done(scalar) => return Decoded::Char { scalar, len: i },
            Step::More => {}
            Step::Invalid => return Decoded::Invalid,
        }
    }
    Decoded::Incomplete
}

/// The body of `mbsrtowcs`, `mbsnrtowcs` and `mbstowcs`. `update` receives the
/// first unconverted byte, or NULL once the terminating NUL has been
/// converted; it is only written when `dst` is non-null, which is C's rule for
/// `*src`. Answers the wide characters stored, excluding that NUL.
unsafe fn bytes_to_wide(
    dst: *mut wchar_t,
    start: *const u8,
    update: *mut *const c_char,
    nmc: size_t,
    len: size_t,
    state: &mut MbState,
) -> size_t {
    let track = !update.is_null() && !dst.is_null();
    let mut consumed = 0usize;
    let mut written = 0usize;
    let mut char_start = 0usize;

    while consumed < nmc {
        if !dst.is_null() && written == len {
            break;
        }
        let byte = *start.add(consumed);
        consumed += 1;
        match utf8::decode_step(state, byte) {
            Step::More => {}
            Step::Invalid => {
                if track {
                    *update = start.add(char_start).cast();
                }
                errno_set(EILSEQ.raw());
                return MB_ERR;
            }
            Step::Done(scalar) => {
                if !dst.is_null() {
                    *dst.add(written) = scalar as wchar_t;
                }
                if scalar == 0 {
                    if track {
                        *update = core::ptr::null();
                    }
                    return written;
                }
                written += 1;
                char_start = consumed;
            }
        }
    }

    if track {
        *update = start.add(consumed).cast();
    }
    written
}

/// The body of `wcsrtombs`, `wcsnrtombs` and `wcstombs`. A character is stored
/// whole or not at all, so `len` is never overrun by a partial sequence.
/// Answers the bytes stored, excluding the terminating NUL.
unsafe fn wide_to_bytes(
    dst: *mut u8,
    start: *const wchar_t,
    update: *mut *const wchar_t,
    nwc: size_t,
    len: size_t,
) -> size_t {
    let track = !update.is_null() && !dst.is_null();
    let mut read = 0usize;
    let mut written = 0usize;

    while read < nwc {
        let wc = *start.add(read);
        let mut buf = [0u8; 4];
        let bytes = match utf8::encode(wc as u32, &mut buf) {
            Some(bytes) => bytes,
            None => {
                if track {
                    *update = start.add(read);
                }
                errno_set(EILSEQ.raw());
                return MB_ERR;
            }
        };
        if !dst.is_null() {
            if bytes > len - written {
                break;
            }
            core::ptr::copy_nonoverlapping(buf.as_ptr(), dst.add(written), bytes);
        }
        written += bytes;
        read += 1;
        if wc == 0 {
            if track {
                *update = core::ptr::null();
            }
            return written - 1;
        }
    }

    if track {
        *update = start.add(read);
    }
    written
}

/// `mbrtowc(3)`. Answers 0 when the character converted is the NUL, otherwise
/// the bytes consumed; `(size_t)-2` when `n` bytes are a valid but incomplete
/// prefix, and `(size_t)-1` with `EILSEQ` when they cannot begin a character.
/// A null `s` means `mbrtowc(NULL, "", 1, ps)`, which resets `*ps`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbrtowc(
    pwc: *mut wchar_t,
    s: *const c_char,
    n: size_t,
    ps: *mut mbstate_t,
) -> size_t {
    let ps = if ps.is_null() {
        &raw mut STATE_MBRTOWC
    } else {
        ps
    };
    if s.is_null() {
        *ps = MBSTATE_INITIAL;
        return 0;
    }
    let mut state = state_load(ps);
    let decoded = decode_one(&mut state, s.cast(), n);
    state_store(ps, state);
    match decoded {
        Decoded::Char { scalar, len } => {
            if !pwc.is_null() {
                *pwc = scalar as wchar_t;
            }
            if scalar == 0 { 0 } else { len }
        }
        Decoded::Incomplete => MB_INCOMPLETE,
        Decoded::Invalid => {
            errno_set(EILSEQ.raw());
            MB_ERR
        }
    }
}

/// `mbrlen(3)` — `mbrtowc` without the output, and with its own state when
/// `ps` is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbrlen(s: *const c_char, n: size_t, ps: *mut mbstate_t) -> size_t {
    let ps = if ps.is_null() {
        &raw mut STATE_MBRLEN
    } else {
        ps
    };
    mbrtowc(core::ptr::null_mut(), s, n, ps)
}

/// `wcrtomb(3)`. Answers the bytes written, or `(size_t)-1` with `EILSEQ` for
/// anything that is not a Unicode scalar value. A null `s` is
/// `wcrtomb(buf, L'\0', ps)` and answers 1 without storing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcrtomb(s: *mut c_char, wc: wchar_t, ps: *mut mbstate_t) -> size_t {
    let ps = if ps.is_null() {
        &raw mut STATE_WCRTOMB
    } else {
        ps
    };
    *ps = MBSTATE_INITIAL;
    if s.is_null() {
        return 1;
    }
    let mut buf = [0u8; 4];
    match utf8::encode(wc as u32, &mut buf) {
        Some(len) => {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), s.cast::<u8>(), len);
            len
        }
        None => {
            errno_set(EILSEQ.raw());
            MB_ERR
        }
    }
}

/// `mbsnrtowcs(3)`. Converts at most `nmc` bytes, storing at most `len` wide
/// characters, and leaves `*src` at the first unconverted byte — NULL once the
/// terminating NUL has been converted. A null `dst` counts without storing and
/// without touching `*src`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbsnrtowcs(
    dst: *mut wchar_t,
    src: *mut *const c_char,
    nmc: size_t,
    len: size_t,
    ps: *mut mbstate_t,
) -> size_t {
    let ps = if ps.is_null() {
        &raw mut STATE_MBSRTOWCS
    } else {
        ps
    };
    if src.is_null() || (*src).is_null() {
        errno_set(EINVAL.raw());
        return MB_ERR;
    }
    let mut state = state_load(ps);
    let stored = bytes_to_wide(dst, (*src).cast(), src, nmc, len, &mut state);
    state_store(ps, state);
    stored
}

/// `mbsrtowcs(3)` — `mbsnrtowcs` over a NUL-terminated source.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbsrtowcs(
    dst: *mut wchar_t,
    src: *mut *const c_char,
    len: size_t,
    ps: *mut mbstate_t,
) -> size_t {
    mbsnrtowcs(dst, src, size_t::MAX, len, ps)
}

/// `wcsnrtombs(3)`. Converts at most `nwc` wide characters into at most `len`
/// bytes and leaves `*src` at the first unconverted wide character — NULL once
/// the terminating NUL has been converted. A null `dst` counts the bytes the
/// conversion would need.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsnrtombs(
    dst: *mut c_char,
    src: *mut *const wchar_t,
    nwc: size_t,
    len: size_t,
    ps: *mut mbstate_t,
) -> size_t {
    let ps = if ps.is_null() {
        &raw mut STATE_WCSRTOMBS
    } else {
        ps
    };
    if src.is_null() || (*src).is_null() {
        errno_set(EINVAL.raw());
        return MB_ERR;
    }
    *ps = MBSTATE_INITIAL;
    wide_to_bytes(dst.cast(), *src, src, nwc, len)
}

/// `wcsrtombs(3)` — `wcsnrtombs` over a NUL-terminated source.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcsrtombs(
    dst: *mut c_char,
    src: *mut *const wchar_t,
    len: size_t,
    ps: *mut mbstate_t,
) -> size_t {
    wcsnrtombs(dst, src, size_t::MAX, len, ps)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbsinit(ps: *const mbstate_t) -> c_int {
    if ps.is_null() {
        return 1;
    }
    c_int::from(MbState::from_raw((*ps).__size).is_initial())
}

/// `btowc(3)`. Only the ASCII range is a single-byte character in UTF-8;
/// everything else, `EOF` included, is `WEOF`.
#[unsafe(no_mangle)]
pub extern "C" fn btowc(c: c_int) -> wint_t {
    if c == EOF {
        return WEOF;
    }
    let byte = c as u8;
    if byte < 0x80 { byte as wint_t } else { WEOF }
}

#[unsafe(no_mangle)]
pub extern "C" fn wctob(c: wint_t) -> c_int {
    if c < 0x80 { c as c_int } else { EOF }
}

/// `mbtowc(3)`. Answers the bytes consumed, 0 for the NUL, or -1 with
/// `EILSEQ`. An incomplete sequence is an error here rather than `-2`: this
/// entry point has no way to resume one.
///
/// UTF-8 carries no shift state, so `mbtowc(NULL, ...)` has nothing to reset
/// and answers 0, meaning state-independent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbtowc(pwc: *mut wchar_t, s: *const c_char, n: size_t) -> c_int {
    if s.is_null() {
        return 0;
    }
    let mut state = MbState::default();
    match decode_one(&mut state, s.cast(), n) {
        Decoded::Char { scalar, len } => {
            if !pwc.is_null() {
                *pwc = scalar as wchar_t;
            }
            if scalar == 0 { 0 } else { len as c_int }
        }
        Decoded::Incomplete | Decoded::Invalid => {
            errno_set(EILSEQ.raw());
            -1
        }
    }
}

/// `mblen(3)` — `mbtowc` without the output.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mblen(s: *const c_char, n: size_t) -> c_int {
    mbtowc(core::ptr::null_mut(), s, n)
}

/// `wctomb(3)`. Answers the bytes written, or -1 with `EILSEQ`. A null `s`
/// answers 0: the encoding is not state-dependent.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wctomb(s: *mut c_char, wc: wchar_t) -> c_int {
    if s.is_null() {
        return 0;
    }
    let mut buf = [0u8; 4];
    match utf8::encode(wc as u32, &mut buf) {
        Some(len) => {
            core::ptr::copy_nonoverlapping(buf.as_ptr(), s.cast::<u8>(), len);
            len as c_int
        }
        None => {
            errno_set(EILSEQ.raw());
            -1
        }
    }
}

/// `mbstowcs(3)`. Begins in the initial conversion state each call, stores at
/// most `len` wide characters, and answers `(size_t)-1` with `EILSEQ` for an
/// invalid or truncated sequence. A null `dst` counts instead of storing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn mbstowcs(dst: *mut wchar_t, src: *const c_char, len: size_t) -> size_t {
    if src.is_null() {
        errno_set(EINVAL.raw());
        return MB_ERR;
    }
    let mut state = MbState::default();
    bytes_to_wide(
        dst,
        src.cast(),
        core::ptr::null_mut(),
        size_t::MAX,
        len,
        &mut state,
    )
}

/// `wcstombs(3)`. Stores at most `len` bytes and answers `(size_t)-1` with
/// `EILSEQ` for a wide character that is not a scalar value. A null `dst`
/// counts the bytes the conversion would need.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstombs(dst: *mut c_char, src: *const wchar_t, len: size_t) -> size_t {
    if src.is_null() {
        errno_set(EINVAL.raw());
        return MB_ERR;
    }
    wide_to_bytes(dst.cast(), src, core::ptr::null_mut(), size_t::MAX, len)
}

/// Bytes of subject sequence a `wcsto*` transcodes without leaving the stack.
const NUMERIC_STACK: usize = 512;

/// The leading subject sequence of a wide string, transcoded to ASCII where
/// the narrow parsers can read it.
struct Subject {
    /// The NUL-terminated bytes: the caller's array, or `owned`.
    bytes: *mut u8,
    /// Wide characters skipped as leading whitespace.
    skipped: size_t,
    /// The heap block to release, null when the array was big enough.
    owned: *mut u8,
}

fn numeric_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
}

fn wide_space(wc: wchar_t) -> bool {
    wc == 0x20 || (0x09..=0x0d).contains(&wc)
}

/// Transcodes the leading subject sequence of `s` into ASCII. `None` means
/// there is nothing a `strto*` could convert, and `endptr` has already been
/// set to `s` — C's "no conversion". Every character a C numeric subject
/// sequence can contain is ASCII, so a wide character outside it ends the run.
///
/// A run longer than `buf` takes the allocator lock, which no `strto*` does,
/// so these are not async-signal-safe; POSIX requires that of neither family.
unsafe fn numeric_subject(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    buf: &mut [u8; NUMERIC_STACK],
) -> Option<Subject> {
    if !endptr.is_null() {
        *endptr = s;
    }
    if s.is_null() {
        return None;
    }
    let mut skipped = 0usize;
    while wide_space(*s.add(skipped)) {
        skipped += 1;
    }
    let mut run = 0usize;
    loop {
        let wc = *s.add(skipped + run);
        if wc <= 0 || wc > 0x7f || !numeric_byte(wc as u8) {
            break;
        }
        run += 1;
    }
    if run == 0 {
        return None;
    }
    // The NUL needs a byte of its own, so the array serves a run shorter than
    // itself and the heap takes anything longer.
    let owned = if run < buf.len() {
        core::ptr::null_mut()
    } else {
        let block = crate::mem::malloc::alloc(run + 1).cast::<u8>();
        if block.is_null() {
            errno_set(ENOMEM.raw());
            return None;
        }
        block
    };
    let bytes = if owned.is_null() {
        buf.as_mut_ptr()
    } else {
        owned
    };
    for i in 0..run {
        *bytes.add(i) = *s.add(skipped + i) as u8;
    }
    *bytes.add(run) = 0;
    Some(Subject {
        bytes,
        skipped,
        owned,
    })
}

impl Drop for Subject {
    fn drop(&mut self) {
        if !self.owned.is_null() {
            crate::mem::malloc::dealloc(self.owned.cast());
        }
    }
}

impl Subject {
    /// Turns the narrow parser's `end` back into an offset in the wide
    /// string. `false` is C's "no conversion": the parser read nothing, so
    /// `endptr` keeps the `s` that [`numeric_subject`] wrote.
    unsafe fn finish(
        &self,
        s: *const wchar_t,
        endptr: *mut *const wchar_t,
        end: *const u8,
    ) -> bool {
        let consumed = end.offset_from(self.bytes.cast_const()) as size_t;
        if consumed == 0 {
            return false;
        }
        if !endptr.is_null() {
            *endptr = s.add(self.skipped + consumed);
        }
        true
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstol(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_long {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtol(subject.bytes, &raw mut end, base);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoul(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_ulong {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtoul(subject.bytes, &raw mut end, base);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoll(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_longlong {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtoll(subject.bytes, &raw mut end, base);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoull(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_ulonglong {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtoull(subject.bytes, &raw mut end, base);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstod(s: *const wchar_t, endptr: *mut *const wchar_t) -> f64 {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0.0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtod(subject.bytes, &raw mut end);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0.0
    }
}

/// `wcstof(3)`. Delegates to the narrow `strtof` rather than narrowing
/// `wcstod`'s `double`, so the result is rounded once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstof(s: *const wchar_t, endptr: *mut *const wchar_t) -> f32 {
    let mut buf = [0u8; NUMERIC_STACK];
    let Some(subject) = numeric_subject(s, endptr, &mut buf) else {
        return 0.0;
    };
    let mut end: *const u8 = core::ptr::null();
    let value = crate::string::convert::strtof(subject.bytes, &raw mut end);
    if subject.finish(s, endptr, end) {
        value
    } else {
        0.0
    }
}

/// `wcstold(3)`, whose `long double` return no Rust signature can name: the
/// System V ABI returns it in `st(0)`, so as in
/// [`strtold`](crate::string::convert::strtold) the declared return is `()`
/// and the widening of [`wcstod`]'s `double` is written out. The value is
/// `wcstod`'s, so this carries `double` precision rather than 64 significand
/// bits; libc++ needs the entry point (`std::stold`) to build.
///
/// Register contract: RDI = `s` and RSI = `endptr` reach `wcstod` untouched,
/// and the XMM0 it answers with is spilled and reloaded onto the x87 stack,
/// which is where a caller reads a `long double`.
///
/// # Safety
/// As `wcstod`.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstold(_s: *const wchar_t, _endptr: *mut *const wchar_t) {
    core::arch::naked_asm!(
        // 24, not 16: the ABI wants `rsp` 16-byte aligned at the `call`, and
        // this function's own return address already offsets it by 8.
        "sub rsp, 24",
        "call wcstod",
        "movsd qword ptr [rsp], xmm0",
        "fld qword ptr [rsp]",
        "add rsp, 24",
        "ret",
    );
}

/// `wcstoimax(3)`.
///
/// # Safety
/// As [`wcstoll`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoimax(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_longlong {
    wcstoll(s, endptr, base)
}

/// `wcstoumax(3)`.
///
/// # Safety
/// As [`wcstoull`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstoumax(
    s: *const wchar_t,
    endptr: *mut *const wchar_t,
    base: c_int,
) -> c_ulonglong {
    wcstoull(s, endptr, base)
}

/// `wcstold_l(3)`. As [`crate::string::convert::strtold_l`]: the locale is
/// discarded and the x87 return contract is [`wcstold`]'s.
///
/// # Safety
/// As [`wcstold`].
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wcstold_l(
    _s: *const wchar_t,
    _endptr: *mut *const wchar_t,
    _loc: *mut core::ffi::c_void,
) {
    core::arch::naked_asm!("jmp wcstold");
}
