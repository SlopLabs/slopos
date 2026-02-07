#![allow(unsafe_op_in_unsafe_fn)]

use core::ffi::c_char;

pub type MainFn =
    extern "C" fn(argc: isize, argv: *const *const c_char, envp: *const *const c_char) -> i32;

static mut MAIN_FN: Option<MainFn> = None;
static mut ARGC: isize = 0;
static mut ARGV: *const *const c_char = core::ptr::null();
static mut ENVP: *const *const c_char = core::ptr::null();

pub fn set_main(main: MainFn) {
    unsafe {
        MAIN_FN = Some(main);
    }
}

pub fn argc() -> isize {
    unsafe { ARGC }
}

pub fn argv() -> *const *const c_char {
    unsafe { ARGV }
}

pub fn envp() -> *const *const c_char {
    unsafe { ENVP }
}

/// Parse argc/argv/envp from the kernel-prepared user stack and store
/// them in the CRT0 globals.
///
/// **WARNING**: This function reads RSP via inline assembly.  It must
/// only be called from a context where RSP still points at the
/// kernel-prepared stack layout (argc at [rsp], argv at [rsp+8], …).
/// Calling it from a normal Rust function is *wrong* because the
/// prologue has already adjusted RSP.  Use a `#[naked]` trampoline
/// that captures the raw stack pointer and passes it here.
///
/// Currently **not** called from the `entry!` macro — no app uses
/// `get_arg()`/`get_env()` yet.
///
/// # Safety
/// - Must be called exactly once, before any use of `argc()`/`argv()`/
///   `envp()`/`get_arg()`/`get_env()`.
/// - RSP must still point at the original kernel-prepared user stack.
pub unsafe fn init_from_stack() {
    unsafe {
        use core::arch::asm;

        let sp: u64;
        asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack));

        let stack_ptr = sp as *const u64;

        let raw_argc = *stack_ptr as isize;
        if raw_argc < 0 || raw_argc > 1024 {
            ARGC = 0;
            ARGV = core::ptr::null();
            ENVP = core::ptr::null();
            return;
        }

        ARGC = raw_argc;
        ARGV = stack_ptr.add(1) as *const *const c_char;

        let envp_offset = 1 + (raw_argc as usize) + 1;
        ENVP = stack_ptr.add(envp_offset) as *const *const c_char;
    }
}

/// # Safety
/// Same RSP requirements as [`init_from_stack`].
pub unsafe fn crt0_start() -> ! {
    use super::syscall::sys_exit;

    init_from_stack();

    if let Some(main) = MAIN_FN {
        let ret = main(ARGC, ARGV, ENVP);
        sys_exit(ret);
    } else {
        sys_exit(127);
    }
}

pub fn get_arg(index: usize) -> Option<&'static [u8]> {
    unsafe {
        if index >= ARGC as usize {
            return None;
        }
        let arg_ptr = *ARGV.add(index);
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
        if ENVP.is_null() {
            return None;
        }
        let mut i = 0;
        loop {
            let env_ptr = *ENVP.add(i);
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
