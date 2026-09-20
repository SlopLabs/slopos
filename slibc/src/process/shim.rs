//! Safe wrappers over `process::*` for use from tests.

pub fn exit(code: i32) -> ! {
    // SAFETY: `process::exit` never reads memory through its argument
    // and never returns.
    unsafe { super::exit(code) }
}

/// Terminate the process without flushing stdio or running `atexit` handlers.
pub fn _exit(code: i32) -> ! {
    // SAFETY: takes no pointers and never returns.
    unsafe { super::_exit(code) }
}

/// Create a child process. Returns the child pid to the parent, 0 to the
/// child, and -1 on error.
pub fn fork() -> i32 {
    // SAFETY: `fork` takes no arguments and returns a plain integer.
    unsafe { super::fork() }
}

/// Block until `pid` terminates and return its `$?` — the exit code, or
/// `128 + signum` for a death by signal — or -1 on error.
pub fn wait_for_child(pid: i32) -> i32 {
    let mut status = 0i32;
    // SAFETY: `status` is a live, correctly aligned `i32` the callee writes once.
    let reaped = unsafe { super::waitpid(pid, &mut status, 0) };
    if reaped <= 0 {
        return -1;
    }
    if super::WIFEXITED(status) {
        super::WEXITSTATUS(status)
    } else if super::WIFSIGNALED(status) {
        128 + super::WTERMSIG(status)
    } else {
        -1
    }
}

/// Register a function to run at normal process termination. Returns 0 on
/// success, -1 if the table is full.
pub fn atexit(func: extern "C" fn()) -> i32 {
    // SAFETY: `func` is a valid function pointer with the required signature;
    // the coercion to `unsafe extern "C" fn()` adds no obligation to the
    // caller, and `atexit` only stores it.
    unsafe { crate::cxa::atexit(func) }
}
