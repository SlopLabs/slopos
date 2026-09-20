//! `<ctype.h>`, over ASCII.
//!
//! The classification is the C locale's and only the C locale's, which is the
//! whole of what SlopOS has: `setlocale` accepts `"C"` and `"POSIX"` and
//! nothing else, so there is no locale a table could differ for. musl ships
//! glibc's `__ctype_b_loc` table accessors beside these for binary
//! compatibility; nothing here needs them, because a program that reaches
//! these is one this tree compiled.
//!
//! An argument outside `unsigned char` and `EOF` is undefined in C. These
//! answer false for it rather than indexing anything.

use core::ffi::c_int;

fn byte(c: c_int) -> Option<u8> {
    u8::try_from(c).ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn isalnum(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_alphanumeric()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isalpha(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_alphabetic()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isblank(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b == b' ' || b == b'\t') as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn iscntrl(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_control()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isdigit(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_digit()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isgraph(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_graphic()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn islower(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_lowercase()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isprint(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_graphic() || b == b' ') as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn ispunct(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_punctuation()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isspace(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b == b' ' || (0x09..=0x0d).contains(&b)) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isupper(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_uppercase()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn isxdigit(c: c_int) -> c_int {
    byte(c).is_some_and(|b| b.is_ascii_hexdigit()) as c_int
}

#[unsafe(no_mangle)]
pub extern "C" fn tolower(c: c_int) -> c_int {
    match byte(c) {
        Some(b) => b.to_ascii_lowercase() as c_int,
        None => c,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn toupper(c: c_int) -> c_int {
    match byte(c) {
        Some(b) => b.to_ascii_uppercase() as c_int,
        None => c,
    }
}
