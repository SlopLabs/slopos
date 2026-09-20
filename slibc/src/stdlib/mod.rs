//! `<stdlib.h>`'s integer arithmetic.
//!
//! The rest of the header lives where its subject does: allocation in
//! [`crate::mem`], conversion in [`crate::string`], termination in
//! [`crate::process`].

use core::ffi::{c_int, c_long, c_longlong};

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
