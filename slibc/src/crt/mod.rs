mod abi_pins;

use core::cell::SyncUnsafeCell;
use core::ffi::c_char;

pub type MainFn =
    extern "C" fn(argc: isize, argv: *const *const c_char, envp: *const *const c_char) -> i32;

#[repr(transparent)]
struct SyncCharPtrPtr(*const *const c_char);
unsafe impl Sync for SyncCharPtrPtr {}

static ARGC: SyncUnsafeCell<isize> = SyncUnsafeCell::new(0);
static ARGV: SyncUnsafeCell<SyncCharPtrPtr> =
    SyncUnsafeCell::new(SyncCharPtrPtr(core::ptr::null()));
static ENVP: SyncUnsafeCell<SyncCharPtrPtr> =
    SyncUnsafeCell::new(SyncCharPtrPtr(core::ptr::null()));

pub fn argc() -> isize {
    unsafe { *ARGC.get() }
}

pub fn argv() -> *const *const c_char {
    unsafe { (*ARGV.get()).0 }
}

pub fn envp() -> *const *const c_char {
    unsafe { (*ENVP.get()).0 }
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
    *ARGC.get() = argc;
    (*ARGV.get()).0 = argv;

    let envp_ptr = argv.add(argc as usize + 1) as *const *const c_char;
    (*ENVP.get()).0 = envp_ptr;
    crate::env::environ = envp_ptr as *mut *mut u8;
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();

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
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8) -> isize;
    }

    let raw_argc = *stack_base as isize;
    let argc = if !(0..=1024).contains(&raw_argc) {
        0
    } else {
        raw_argc
    };
    let argv = stack_base.add(1) as *const *const c_char;
    let envp = stack_base.add(1 + (argc as usize) + 1) as *const *const c_char;

    *ARGC.get() = argc;
    (*ARGV.get()).0 = argv;
    (*ENVP.get()).0 = envp;
    crate::env::environ = envp as *mut *mut u8;

    // Must precede anything that touches a thread-local; `errno` uses its
    // static fallback until this completes.
    crate::thread::tls::capture_tls_template_from_stack(stack_base);
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();

    let ret = main(argc, argv as *const *const u8);
    crate::process::exit(ret as i32)
}

pub fn get_arg(index: usize) -> Option<&'static [u8]> {
    unsafe {
        if index >= (*ARGC.get()) as usize {
            return None;
        }
        let arg_ptr = *(*ARGV.get()).0.add(index);
        if arg_ptr.is_null() {
            return None;
        }
        let mut len = 0;
        while *arg_ptr.add(len) != 0 {
            len += 1;
        }
        Some(core::slice::from_raw_parts(arg_ptr as *const u8, len))
    }
}

pub fn get_env(name: &[u8]) -> Option<&'static [u8]> {
    unsafe {
        if (*ENVP.get()).0.is_null() {
            return None;
        }
        let mut i = 0;
        loop {
            let env_ptr = *(*ENVP.get()).0.add(i);
            if env_ptr.is_null() {
                break;
            }
            let mut len = 0;
            while *env_ptr.add(len) != 0 {
                len += 1;
            }
            let env = core::slice::from_raw_parts(env_ptr as *const u8, len);

            if env.len() > name.len() && env[name.len()] == b'=' {
                if &env[..name.len()] == name {
                    return Some(&env[name.len() + 1..]);
                }
            }
            i += 1;
        }
        None
    }
}
