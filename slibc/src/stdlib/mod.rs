//! `<stdlib.h>`'s integer arithmetic, with `qsort` and `bsearch` in [`sort`].
//!
//! The rest of the header lives where its subject does: allocation in
//! [`crate::mem`], conversion in [`crate::string`], termination in
//! [`crate::process`].

#![allow(non_camel_case_types)]

pub mod getopt;
pub mod sort;
pub mod temp;

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

pub type intmax_t = c_long;
pub type uintmax_t = c_ulong;

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

/// POSIX's `random`: an additive feedback generator over 31 words, each new
/// word the sum of the words 31 and 3 back, its high 31 bits the result. The
/// words start as the multiplicative congruential sequence of the seed, and
/// the first 310 results are discarded so they no longer show it.
struct Additive {
    ring: [u32; ADDITIVE_RING],
    /// Where the next word goes.
    at: usize,
}

const ADDITIVE_DEGREE: usize = 31;
const ADDITIVE_SEPARATION: usize = 3;
const ADDITIVE_RING: usize = ADDITIVE_DEGREE + ADDITIVE_SEPARATION;
const ADDITIVE_DISCARD: usize = 10 * ADDITIVE_DEGREE;
const PARK_MILLER_MULTIPLIER: i64 = 16_807;
const PARK_MILLER_MODULUS: i64 = 2_147_483_647;

impl Additive {
    const fn seeded(seed: u32) -> Additive {
        let mut ring = [0u32; ADDITIVE_RING];
        ring[0] = if seed == 0 { 1 } else { seed };
        let mut i = 1;
        while i < ADDITIVE_DEGREE {
            let word = PARK_MILLER_MULTIPLIER * (ring[i - 1] as i32 as i64) % PARK_MILLER_MODULUS;
            ring[i] = if word < 0 {
                word + PARK_MILLER_MODULUS
            } else {
                word
            } as u32;
            i += 1;
        }
        while i < ADDITIVE_RING {
            ring[i] = ring[i - ADDITIVE_DEGREE];
            i += 1;
        }
        let mut state = Additive { ring, at: 0 };
        let mut discarded = 0;
        while discarded < ADDITIVE_DISCARD {
            state.step();
            discarded += 1;
        }
        state
    }

    const fn back(&self, n: usize) -> u32 {
        self.ring[(self.at + ADDITIVE_RING - n) % ADDITIVE_RING]
    }

    const fn step(&mut self) -> u32 {
        let word = self
            .back(ADDITIVE_DEGREE)
            .wrapping_add(self.back(ADDITIVE_SEPARATION));
        self.ring[self.at] = word;
        self.at = (self.at + 1) % ADDITIVE_RING;
        word >> 1
    }
}

/// Unsynchronised, as `rand`'s state is.
static mut RANDOM_STATE: Additive = Additive::seeded(1);

#[unsafe(no_mangle)]
pub extern "C" fn srandom(seed: core::ffi::c_uint) {
    unsafe { RANDOM_STATE = Additive::seeded(seed) };
}

#[unsafe(no_mangle)]
pub extern "C" fn random() -> core::ffi::c_long {
    unsafe { core::ffi::c_long::from((*(&raw mut RANDOM_STATE)).step()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn imaxabs(n: intmax_t) -> intmax_t {
    n.wrapping_abs()
}

#[unsafe(no_mangle)]
pub extern "C" fn imaxdiv(numer: intmax_t, denom: intmax_t) -> imaxdiv_t {
    match (numer.checked_div(denom), numer.checked_rem(denom)) {
        (Some(quot), Some(rem)) => imaxdiv_t { quot, rem },
        _ => imaxdiv_t { quot: 0, rem: 0 },
    }
}
