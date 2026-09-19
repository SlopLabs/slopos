//! `libdltest.so` — the shared object `dl_probe` loads.
//!
//! Between them, the exports below need every relocation kind a real shared
//! object uses: a call out to the executable (`JUMP_SLOT` against the global
//! scope), a pointer to its own data (`RELATIVE`), a thread-local
//! (`DTPMOD64`/`DTPOFF64` and `__tls_get_addr`), a `DT_NEEDED` on the C
//! library, and a `DT_INIT_ARRAY` constructor.

#![cfg_attr(target_os = "slopos", no_std)]
#![allow(unsafe_op_in_unsafe_fn)]
#![cfg_attr(target_os = "slopos", feature(thread_local))]

#[cfg(target_os = "slopos")]
mod probe {
    use core::ffi::{c_char, c_int};

    unsafe extern "C" {
        /// Defined by `dl_probe`, which links `--export-dynamic` for it.
        fn dl_probe_callback(value: c_int) -> c_int;
        fn strlen(s: *const c_char) -> usize;
    }

    #[unsafe(no_mangle)]
    pub static DLTEST_DATA: c_int = 0x5105;

    /// A pointer to our own data, which is a `RELATIVE` relocation in a
    /// `-Bsymbolic` object and a symbolic `R_X86_64_64` otherwise.
    #[unsafe(no_mangle)]
    pub static DLTEST_DATA_PTR: &c_int = &DLTEST_DATA;

    #[thread_local]
    static mut DLTEST_TLS: c_int = 0x1234;

    static mut CTOR_RAN: c_int = 0;

    unsafe extern "C" fn ctor() {
        unsafe { CTOR_RAN = 1 };
    }

    #[used]
    #[unsafe(link_section = ".init_array")]
    static INIT: unsafe extern "C" fn() = ctor;

    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_add(a: c_int, b: c_int) -> c_int {
        a + b
    }

    /// Calls back into the executable, so the link only works if the loader
    /// resolved this object's undefined symbols against the global scope.
    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_call_host(value: c_int) -> c_int {
        unsafe { dl_probe_callback(value) }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_data_via_ptr() -> c_int {
        *DLTEST_DATA_PTR
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_ctor_ran() -> c_int {
        unsafe { CTOR_RAN }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_tls_bump(by: c_int) -> c_int {
        unsafe {
            DLTEST_TLS += by;
            DLTEST_TLS
        }
    }

    /// Reaches the C library, which is what puts `libc.so` in this object's
    /// `DT_NEEDED` and makes `dlopen` resolve a dependency that is already
    /// mapped.
    #[unsafe(no_mangle)]
    pub extern "C" fn dltest_strlen(s: *const c_char) -> usize {
        unsafe { strlen(s) }
    }

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
        loop {}
    }
}
