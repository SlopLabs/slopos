//! The parts of slibc that are arithmetic rather than ABI.
//!
//! Everything here is `no_std` and allocation-free, answering by value or
//! into a caller-owned slice, so slibc's C entry points stay thin marshalling
//! over a body that `cargo test` can run on the host in milliseconds. The
//! ABI-bound halves — the `long double` x87 shims, `setjmp`, the locale
//! globals — cannot be split this way and are proved in the guest instead.

#![no_std]
#![forbid(unsafe_code)]

pub mod calendar;
pub mod dtoa;
pub mod hexfloat;
pub mod strftime;
pub mod utf8;
