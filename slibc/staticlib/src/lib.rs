//! `libc.a` — slibc packaged as a C archive.
//!
//! The crate has no code of its own: depending on `slopos-slibc` is what pulls
//! slibc's object code (and therefore all of its `#[unsafe(no_mangle)]` C
//! exports, including the ones nothing in this crate references) into the
//! emitted archive. All this file adds is the one thing an archive needs that
//! an rlib gets from the binary it is linked into: a panic runtime.
//!
//! A C link line is `crt0.o`, this archive, and `slibc/include` on the include
//! path; see `slibc/include/slibc.h`.
//!
//! Off the `slopos` target the crate is a deliberately empty `std` shim so that
//! a host `cargo check --workspace` stays green: a `no_std` `staticlib` cannot
//! be emitted for a `panic = "unwind"` target at all, and `libc.a` is only
//! meaningful for SlopOS in the first place.

#![cfg_attr(target_os = "slopos", no_std)]

extern crate slopos_slibc as _;

#[cfg(target_os = "slopos")]
mod rt {
    use core::ffi::c_int;
    use core::ffi::c_void;

    unsafe extern "C" {
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn _exit(status: c_int) -> !;
    }

    /// The archive's panic runtime. Rust code inside slibc is not supposed to
    /// panic, so this is a crash reporter rather than a recovery path: it
    /// reports on `stderr` and exits with the shell's `SIGABRT` status, which
    /// is what `abort(3)` would have produced.
    #[panic_handler]
    fn slibc_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
        const MSG: &[u8] = b"libc.a: panic in slibc\n";

        // SAFETY: `write` and `_exit` are slibc's own C exports, archived into
        // this staticlib, so they resolve within the same artifact. `MSG` is a
        // `'static` slice, so the pointer/length pair is valid for the call.
        unsafe {
            write(2, MSG.as_ptr().cast::<c_void>(), MSG.len());
            _exit(134);
        }
    }
}
