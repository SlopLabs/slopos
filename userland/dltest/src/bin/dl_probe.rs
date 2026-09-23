//! `/bin/dl_probe` — a dynamically linked Rust program.
//!
//! It reaches the C library exclusively through `libc.so`: no `slopos-slibc`
//! rlib, no `std`, nothing but `extern "C"` declarations and `crt0.o`. That
//! is the shape a cross-built C or C++ program has, which is what makes it
//! worth testing rather than a convenience.
//!
//! `dl_test` spawns it and grades its exit status: 0 means every check below
//! passed, and any other value is the number of the one that did not.

#![cfg_attr(target_os = "slopos", no_std)]
#![allow(unsafe_op_in_unsafe_fn)]
#![cfg_attr(target_os = "slopos", no_main)]
#![cfg_attr(target_os = "slopos", feature(thread_local))]

#[cfg(not(target_os = "slopos"))]
fn main() {}

#[cfg(target_os = "slopos")]
mod probe {
    use core::ffi::{c_char, c_int, c_void};
    use core::ptr;

    #[repr(C)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut c_void,
    }

    #[repr(C)]
    struct DlPhdrInfo {
        dlpi_addr: usize,
        dlpi_name: *const c_char,
        dlpi_phdr: *const c_void,
        dlpi_phnum: u16,
        dlpi_adds: u64,
        dlpi_subs: u64,
        dlpi_tls_modid: usize,
        dlpi_tls_data: *mut c_void,
    }

    const RTLD_NOW: c_int = 2;
    const RTLD_NOLOAD: c_int = 4;
    const RTLD_DEFAULT: *mut c_void = ptr::null_mut();

    unsafe extern "C" {
        fn dlopen(path: *const c_char, flags: c_int) -> *mut c_void;
        fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
        fn dlclose(handle: *mut c_void) -> c_int;
        fn dlerror() -> *const c_char;
        fn dladdr(addr: *const c_void, info: *mut DlInfo) -> c_int;
        fn dl_iterate_phdr(
            cb: unsafe extern "C" fn(*mut DlPhdrInfo, usize, *mut c_void) -> c_int,
            data: *mut c_void,
        ) -> c_int;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn strcmp(a: *const c_char, b: *const c_char) -> c_int;
        fn getpid() -> c_int;
    }

    const LIB: &[u8] = b"/lib/libdltest.so\0";

    /// What `libdltest.so` calls back into, which only links because the
    /// probe is built `--export-dynamic`.
    #[unsafe(no_mangle)]
    pub extern "C" fn dl_probe_callback(value: c_int) -> c_int {
        value * 3
    }

    #[thread_local]
    static mut EXE_TLS: c_int = 11;

    fn say(msg: &[u8]) {
        unsafe { write(2, msg.as_ptr().cast(), msg.len()) };
    }

    unsafe fn report_dlerror() {
        say(b"dl_probe: dlerror: ");
        let msg = dlerror();
        if msg.is_null() {
            say(b"(none)");
        } else {
            let mut len = 0usize;
            while *msg.add(len) != 0 {
                len += 1;
            }
            write(2, msg.cast(), len);
        }
        say(b"\n");
    }

    unsafe fn sym(handle: *mut c_void, name: &[u8]) -> *mut c_void {
        dlsym(handle, name.as_ptr().cast())
    }

    /// Counts the objects that report a program-header table, which is the
    /// one field an unwinder actually reads.
    unsafe extern "C" fn count_objects(
        info: *mut DlPhdrInfo,
        size: usize,
        data: *mut c_void,
    ) -> c_int {
        if size < size_of::<DlPhdrInfo>() || (*info).dlpi_phnum == 0 {
            return 0;
        }
        *(data as *mut u32) += 1;
        0
    }

    /// Every check, in order. The answer is the number of the first failure.
    unsafe fn run() -> c_int {
        // The program is running at all, which means `PT_INTERP`, the auxv,
        // the executable's relocations and the static TLS block all worked.
        EXE_TLS += 1;
        if EXE_TLS != 12 {
            return 1;
        }
        if getpid() <= 0 {
            return 2;
        }

        // A failed `dlopen` reports through `dlerror`, and reading clears it.
        if !dlopen(b"/lib/no-such-object.so\0".as_ptr().cast(), RTLD_NOW).is_null() {
            return 3;
        }
        if dlerror().is_null() {
            return 4;
        }
        if !dlerror().is_null() {
            return 5;
        }

        let handle = dlopen(LIB.as_ptr().cast(), RTLD_NOW);
        if handle.is_null() {
            report_dlerror();
            return 6;
        }

        let add = sym(handle, b"dltest_add\0");
        if add.is_null() {
            return 7;
        }
        let add: extern "C" fn(c_int, c_int) -> c_int = core::mem::transmute(add);
        if add(3, 4) != 7 {
            return 8;
        }

        let via_ptr = sym(handle, b"dltest_data_via_ptr\0");
        if via_ptr.is_null() {
            return 9;
        }
        let via_ptr: extern "C" fn() -> c_int = core::mem::transmute(via_ptr);
        if via_ptr() != 0x5105 {
            return 10;
        }

        let ctor = sym(handle, b"dltest_ctor_ran\0");
        if ctor.is_null() {
            return 11;
        }
        let ctor: extern "C" fn() -> c_int = core::mem::transmute(ctor);
        if ctor() != 1 {
            return 12;
        }

        let call_host = sym(handle, b"dltest_call_host\0");
        if call_host.is_null() {
            return 13;
        }
        let call_host: extern "C" fn(c_int) -> c_int = core::mem::transmute(call_host);
        if call_host(5) != 15 {
            return 14;
        }

        let bump = sym(handle, b"dltest_tls_bump\0");
        if bump.is_null() {
            return 15;
        }
        let bump: extern "C" fn(c_int) -> c_int = core::mem::transmute(bump);
        if bump(1) != 0x1235 || bump(2) != 0x1237 {
            return 16;
        }

        let so_strlen = sym(handle, b"dltest_strlen\0");
        if so_strlen.is_null() {
            return 17;
        }
        let so_strlen: extern "C" fn(*const c_char) -> usize = core::mem::transmute(so_strlen);
        if so_strlen(b"abcd\0".as_ptr().cast()) != 4 {
            return 18;
        }

        let mut info = DlInfo {
            dli_fname: ptr::null(),
            dli_fbase: ptr::null_mut(),
            dli_sname: ptr::null(),
            dli_saddr: ptr::null_mut(),
        };
        if dladdr(add as *const c_void, &mut info) == 0 {
            return 19;
        }
        if info.dli_fname.is_null() || strcmp(info.dli_fname, LIB.as_ptr().cast()) != 0 {
            return 20;
        }
        if info.dli_sname.is_null() || strcmp(info.dli_sname, b"dltest_add\0".as_ptr().cast()) != 0
        {
            return 21;
        }

        // The executable, `libc.so` and `libdltest.so` at least.
        let mut seen: u32 = 0;
        dl_iterate_phdr(count_objects, (&mut seen) as *mut u32 as *mut c_void);
        if seen < 3 {
            return 22;
        }

        // The global scope answers for the C library the interpreter is.
        if dlsym(RTLD_DEFAULT, b"malloc\0".as_ptr().cast()).is_null() {
            return 23;
        }

        // A second open is a second reference, so the first close keeps the
        // object mapped and only the second unloads it.
        let again = dlopen(LIB.as_ptr().cast(), RTLD_NOLOAD);
        if again != handle {
            return 24;
        }
        if dlclose(handle) != 0 {
            return 25;
        }
        if dladdr(add as *const c_void, &mut info) == 0 {
            return 26;
        }
        if dlclose(again) != 0 {
            return 27;
        }
        // The object is gone, so nothing owns its addresses any more.
        if dladdr(add as *const c_void, &mut info) != 0 {
            return 28;
        }
        0
    }

    /// Write into the loaded object's RELRO region, which a correct loader
    /// has sealed. The parent grades the resulting `SIGSEGV`.
    unsafe fn poke_relro() -> c_int {
        let handle = dlopen(LIB.as_ptr().cast(), RTLD_NOW);
        if handle.is_null() {
            return 1;
        }
        let slot = sym(handle, b"DLTEST_DATA_PTR\0");
        if slot.is_null() {
            return 2;
        }
        ptr::write_volatile(slot as *mut usize, 0);
        say(b"relro: write succeeded\n");
        3
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn main(argc: c_int, argv: *const *const c_char) -> c_int {
        if argc > 1 && strcmp(*argv.add(1), b"relro\0".as_ptr().cast()) == 0 {
            return poke_relro();
        }
        let rc = run();
        if rc != 0 {
            say(b"dl_probe: check failed\n");
        }
        rc
    }

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
        say(b"dl_probe: panic\n");
        loop {}
    }
}
