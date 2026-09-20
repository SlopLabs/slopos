//! `libc.so` — slibc as a shared object, and as the program interpreter.
//!
//! The crate has no code of its own: depending on `slopos-slibc` is what pulls
//! slibc's object code — every `#[unsafe(no_mangle)]` C export, `_dlstart`
//! and the dynamic linker among them — into the emitted shared object. All
//! this file adds is the panic runtime an rlib would have taken from the
//! binary it was linked into.
//!
//! The link line lives in `scripts/build_userland.sh`: `-Bsymbolic` and
//! `-z now` so the object's own references bind at link time and its
//! self-relocation needs only `R_X86_64_RELATIVE`, `--soname=libc.so` so a
//! `DT_NEEDED` on the C library resolves to the already-mapped interpreter,
//! and `--entry=_dlstart`.
//!
//! Off the `slopos` target the crate is a deliberately empty `std` shim so a
//! host `cargo check --workspace` stays green: a `no_std` cdylib cannot be
//! emitted for a `panic = "unwind"` target at all.

#![cfg_attr(target_os = "slopos", no_std)]

extern crate slopos_slibc;

#[cfg(target_os = "slopos")]
mod rt {
    use core::ffi::c_int;
    use core::ffi::c_void;

    /// The C library's own heap, as the Rust allocator. `unwinding` builds a
    /// `Vec` of register rules per frame, and this artifact has no `std` to
    /// take an allocator from; routing it anywhere but `malloc` would put a
    /// second heap in a process that exists to have one.
    ///
    /// Stated per artifact rather than once in `slopos-slibc`, for the reason
    /// the panic runtime below is: cargo unifies features across a workspace
    /// build, so a `#[global_allocator]` behind the `unwinder` feature would
    /// become one in every `std` binary that links the rlib.
    struct CHeap;

    // SAFETY: `memalign` returns null or a block of at least `layout.size()`
    // bytes aligned to `layout.align()`, and `dealloc` accepts exactly what it
    // returned.
    unsafe impl core::alloc::GlobalAlloc for CHeap {
        unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
            slopos_slibc::mem::malloc::memalign(layout.align(), layout.size())
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: core::alloc::Layout) {
            slopos_slibc::mem::malloc::dealloc(ptr.cast());
        }
    }

    #[global_allocator]
    static HEAP: CHeap = CHeap;

    unsafe extern "C" {
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn _exit(status: c_int) -> !;
    }

    /// The shared object's panic runtime, matching `libc.a`'s: a crash
    /// reporter rather than a recovery path, exiting with the status
    /// `abort(3)` would have produced.
    #[panic_handler]
    fn slibc_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
        const MSG: &[u8] = b"libc.so: panic in slibc\n";

        // SAFETY: `write` and `_exit` are slibc's own C exports, linked into
        // this shared object, so they resolve within the same artifact.
        unsafe {
            write(2, MSG.as_ptr().cast::<c_void>(), MSG.len());
            _exit(134);
        }
    }
}
