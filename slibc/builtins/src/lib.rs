//! `libbuiltins.a` — compiler-rt's helper routines, position independent.
//!
//! x86-64 codegen calls out for 128-bit arithmetic and for the `long double`
//! conversions, and SlopOS has no libgcc to take those calls from. The
//! routines come from `compiler_builtins`, which cargo links into any
//! `staticlib`; this crate has no code of its own, and the archive's members
//! are pulled only by a link line that still has one of them undefined.
//!
//! Off the `slopos` target the crate is an empty `std` shim, for the reason
//! `slibc/staticlib` is: a `no_std` staticlib cannot be emitted for a
//! `panic = "unwind"` target at all.

#![cfg_attr(target_os = "slopos", no_std)]

#[cfg(target_os = "slopos")]
#[panic_handler]
fn builtins_panic(_info: &core::panic::PanicInfo<'_>) -> ! {
    // compiler-rt's routines do not panic, and this artifact has no stderr to
    // say so on: it is linked into shared objects, not into programs.
    loop {
        core::hint::spin_loop();
    }
}
