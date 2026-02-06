use core::ffi::c_void;

/// Access to linker-provided section symbols, isolated here so other modules
/// avoid raw `extern "C"` declarations.
mod externs {
    use core::ffi::c_void;

    unsafe extern "C" {
        pub(crate) static _kernel_start: c_void;
        pub(crate) static _kernel_end: c_void;
    }
}

#[inline]
pub fn kernel_bounds() -> (*const c_void, *const c_void) {
    unsafe { (&externs::_kernel_start, &externs::_kernel_end) }
}
