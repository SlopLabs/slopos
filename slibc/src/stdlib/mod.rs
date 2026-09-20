//! `<stdlib.h>`'s integer arithmetic, with `qsort` and `bsearch` in [`sort`].
//!
//! The rest of the header lives where its subject does: allocation in
//! [`crate::mem`], conversion in [`crate::string`], termination in
//! [`crate::process`].

#![allow(non_camel_case_types)]

pub mod sort;

use core::ffi::{c_int, c_long, c_longlong, c_ulong};

/// C leaves `abs(INT_MIN)` undefined, and the x86-64 negation of it is
/// `INT_MIN` again. Wrapping is that answer, stated.
#[unsafe(no_mangle)]
pub extern "C" fn abs(n: c_int) -> c_int {
    n.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn labs(n: c_long) -> c_long {
    n.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn llabs(n: c_longlong) -> c_longlong {
    n.wrapping_abs()
}

/// C's widest signed integer, which on x86-64 is `long long`.
pub type intmax_t = c_longlong;
pub type uintmax_t = core::ffi::c_ulonglong;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct imaxdiv_t {
    pub quot: intmax_t,
    pub rem: intmax_t,
}

#[repr(C)]
pub struct div_t {
    pub quot: c_int,
    pub rem: c_int,
}

#[repr(C)]
pub struct ldiv_t {
    pub quot: c_long,
    pub rem: c_long,
}

#[repr(C)]
pub struct lldiv_t {
    pub quot: c_longlong,
    pub rem: c_longlong,
}

/// A zero divisor is undefined in C and traps on x86-64; `wrapping_div`
/// would still trap, so the division is guarded rather than wrapped.
#[unsafe(no_mangle)]
pub extern "C" fn div(numer: c_int, denom: c_int) -> div_t {
    match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(quot), Some(rem)) => div_t { quot, rem },
        _ => div_t { quot: 0, rem: 0 },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ldiv(numer: c_long, denom: c_long) -> ldiv_t {
    match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(quot), Some(rem)) => ldiv_t { quot, rem },
        _ => ldiv_t { quot: 0, rem: 0 },
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn lldiv(numer: c_longlong, denom: c_longlong) -> lldiv_t {
    match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(quot), Some(rem)) => lldiv_t { quot, rem },
        _ => lldiv_t { quot: 0, rem: 0 },
    }
}

/// C99's example generator (§7.20.2.2), which is what `RAND_MAX` of 32767
/// describes. One process-wide state, unsynchronised: C requires no more, and
/// a lock on a function whose value is arbitrary buys nothing.
static mut RAND_STATE: c_ulong = 1;

pub const RAND_MAX: c_int = 32767;

#[unsafe(no_mangle)]
pub extern "C" fn srand(seed: core::ffi::c_uint) {
    unsafe { RAND_STATE = seed as c_ulong };
}

#[unsafe(no_mangle)]
pub extern "C" fn rand() -> c_int {
    unsafe {
        RAND_STATE = RAND_STATE.wrapping_mul(1103515245).wrapping_add(12345);
        ((RAND_STATE / 65536) % 32768) as c_int
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn imaxabs(n: intmax_t) -> intmax_t {
    n.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn imaxdiv(numer: intmax_t, denom: intmax_t) -> imaxdiv_t {
    imaxdiv_t {
        quot: numer / denom,
        rem: numer % denom,
    }
}
