mod abi_pins;

use core::ffi::c_char;

pub type MainFn =
    extern "C" fn(argc: isize, argv: *const *const c_char, envp: *const *const c_char) -> i32;

/// Run the executable's `.preinit_array` and `.init_array` when nothing else
/// will.
///
/// A dynamic program's constructors are the loader's `DT_*_ARRAY` and have
/// already run by the time control reaches here; a static one has no loader,
/// and a static C++ program cannot start without them. The linker brackets
/// each section with the symbols below, so a program with no constructors gets
/// an empty range rather than an undefined reference.
///
/// The test for "static" is `AT_BASE`, which the kernel emits as 0 when it
/// loaded no interpreter. Asking the loader's object table instead would mean
/// taking its lock on every process start, for a question the auxv already
/// answers without one.
///
/// # Safety
/// Called once, before `main`, with TLS and stdio already up.
unsafe fn run_static_init_array() {
    unsafe extern "C" {
        static __preinit_array_start: [usize; 0];
        static __preinit_array_end: [usize; 0];
        static __init_array_start: [usize; 0];
        static __init_array_end: [usize; 0];
    }

    if crate::auxv::tag(slopos_abi::auxv::AT_BASE).unwrap_or(0) != 0 {
        return;
    }

    for (first, last) in [
        (
            &raw const __preinit_array_start,
            &raw const __preinit_array_end,
        ),
        (&raw const __init_array_start, &raw const __init_array_end),
    ] {
        let first = first.cast::<usize>();
        let count = (last as usize - first as usize) / size_of::<usize>();
        for i in 0..count {
            crate::ld_so::call_hook(first.add(i).read());
        }
    }
}

/// # Safety
/// `main`, `argc`, and `argv` must be valid. `envp` is derived from
/// `argv[argc+1]` per the System V ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __libc_start_main(
    main: MainFn,
    argc: isize,
    argv: *const *const c_char,
) -> ! {
    let envp_ptr = argv.add(argc as usize + 1) as *const *const c_char;
    crate::env::environ = envp_ptr as *mut *mut u8;
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();
    crate::unwind::init();
    run_static_init_array();

    let ret = main(argc, argv, envp_ptr);
    crate::process::exit(ret)
}

/// Canonical stack-based C-runtime entry, called by a naked `_start` with the
/// raw initial stack pointer. TLS is fully live before `main` runs.
///
/// # Safety
/// `stack_base` must point at the kernel-prepared entry stack (`argc` at
/// `[stack_base]`, then `argv`, `envp`, and the auxv).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __slibc_start(stack_base: *const usize) -> ! {
    // `main(argc, argv, envp)`, the Unix convention: a two-argument `main`
    // never reads the third register.
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8, envp: *const *const u8) -> isize;
    }

    let argc = *stack_base as isize;
    let argv = stack_base.add(1) as *const *const c_char;
    let envp = stack_base.add(1 + (argc as usize) + 1) as *const *const c_char;

    crate::env::environ = envp as *mut *mut u8;

    // Must precede anything that touches a thread-local; `errno` uses its
    // static fallback until this completes.
    crate::thread::tls::capture_tls_template_from_stack(stack_base);
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();
    crate::unwind::init();
    run_static_init_array();

    let ret = main(argc, argv as *const *const u8, envp as *const *const u8);
    crate::process::exit(ret as i32)
}
