//! slibc — SlopOS Rust-native C standard library.
//!
//! The exported surface is C's: every entry point carries its real C name,
//! the layouts in [`types`] are the ones the target's `libc` declares, and
//! failure is `-1`/`NULL`/`MAP_FAILED` with `errno` set. Where SlopOS's own
//! kernel struct differs from the C one — `sigaction`, the signal mask, the
//! `getdents64` record — slibc translates at the syscall boundary and the
//! kernel struct stays as it is.

#![no_std]
#![allow(unsafe_op_in_unsafe_fn)]
#![feature(sync_unsafe_cell)]
#![feature(thread_local)]

pub mod alloc;
pub mod auxv;
pub mod conf;
pub mod crt;
pub mod ctype;
pub mod cxa;
pub mod env;
pub mod errno;
pub mod error;
pub mod ffi;
pub mod io;
pub mod ld_so;
pub mod locale;
pub mod math;
pub mod mem;
pub mod net;
pub mod pal;
pub mod process;
pub mod setjmp;
pub mod signal;
pub mod stdio;
pub mod stdlib;
pub mod string;
pub mod test_harness;
pub mod thread;
pub mod time;
pub mod tty;
pub mod types;
pub mod unwind;
pub mod wchar;

pub use errno::{__errno_location, Errno, errno_get, errno_set};
pub use error::{SyscallError, SyscallResult, demux, mux};
pub use mem::malloc::{alloc, calloc, dealloc, memalign, realloc};
pub use string::{
    ptr_is_null, slice_from_cstr, slice_from_cstr_mut, u_memcpy, u_memset, u_strlen, u_strnlen,
};
