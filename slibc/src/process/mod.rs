//! Process lifecycle — fork, exec, wait, exit.

pub mod ids;
pub mod rlimit;
pub mod shim;
pub mod spawn;
pub mod tests;
pub mod wait;

use core::ptr;

use crate::env::environ;
use crate::errno::{self, EACCES, EINVAL, ENOENT, ENOEXEC, ENOTDIR};
use crate::pal::{Pal, Sys};
use crate::string::u_strlen;

pub use rlimit::{RLIM_INFINITY, RLIMIT_ALL, RLimit, getrlimit, prlimit, setrlimit};
pub use wait::{WEXITSTATUS, WIFCONTINUED, WIFEXITED, WIFSIGNALED, WIFSTOPPED, WSTOPSIG, WTERMSIG};

/// Returns the child PID to the parent, 0 to the child, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fork() -> i32 {
    match Sys::fork() {
        Ok(pid) => pid,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

/// Only returns on error (-1, sets errno).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execve(
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    if path.is_null() {
        errno::errno_set(EINVAL.raw());
        return -1;
    }
    match Sys::exec(path, argv, envp) {
        Ok(()) => 0,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

/// Exec with the current environment.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execv(path: *const u8, argv: *const *const u8) -> i32 {
    execve(path, argv, environ as *const *const u8)
}

/// Search `PATH` for `file`, then exec; a `file` containing `/` is used as-is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvp(file: *const u8, argv: *const *const u8) -> i32 {
    execvpe(file, argv, environ as *const *const u8)
}

/// `execvp` with an explicit environment.
///
/// POSIX: a file the system cannot execute (`ENOEXEC`) is run as a shell
/// script, `/bin/sh file args...`. A candidate that is missing or not
/// permitted moves the search on; any other failure ends it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execvpe(
    file: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    if file.is_null() || *file == 0 {
        errno::errno_set(if file.is_null() { EINVAL } else { ENOENT }.raw());
        return -1;
    }

    let file_len = u_strlen(file);
    if core::slice::from_raw_parts(file, file_len).contains(&b'/') {
        return exec_or_sh(file, argv, envp);
    }

    let path_val = crate::env::getenv(b"PATH\0".as_ptr());
    if path_val.is_null() {
        errno::errno_set(ENOENT.raw());
        return -1;
    }

    let path_len = u_strlen(path_val);
    let mut buf = [0u8; 4096];
    let mut last = ENOENT;
    let mut seg_start = 0usize;
    while seg_start <= path_len {
        let mut seg_end = seg_start;
        while seg_end < path_len && *path_val.add(seg_end) != b':' {
            seg_end += 1;
        }
        // POSIX: an empty PATH element names the current directory.
        let (dir, dir_len) = if seg_end == seg_start {
            (b".".as_ptr(), 1)
        } else {
            (path_val.add(seg_start).cast_const(), seg_end - seg_start)
        };
        let total = dir_len + 1 + file_len;
        if total < buf.len() {
            ptr::copy_nonoverlapping(dir, buf.as_mut_ptr(), dir_len);
            buf[dir_len] = b'/';
            ptr::copy_nonoverlapping(file, buf.as_mut_ptr().add(dir_len + 1), file_len);
            buf[total] = 0;
            exec_or_sh(buf.as_ptr(), argv, envp);
            let e = errno::Errno(errno::errno_get());
            if e != ENOENT && e != ENOTDIR && e != EACCES {
                return -1;
            }
            last = e;
        }
        seg_start = seg_end + 1;
    }

    errno::errno_set(last.raw());
    -1
}

/// `execlp(file, arg0, ..., NULL)`: [`execvp`] over the argument list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn execlp(file: *const u8, arg0: *const u8, mut args: ...) -> i32 {
    let mut argc = 1usize;
    if !arg0.is_null() {
        let mut probe = args.clone();
        while !probe.next_arg::<*const u8>().is_null() {
            argc += 1;
        }
    }
    let argv = crate::mem::malloc::alloc((argc + 1) * size_of::<*const u8>()) as *mut *const u8;
    if argv.is_null() {
        return -1;
    }
    *argv = arg0;
    if !arg0.is_null() {
        for i in 1..argc {
            *argv.add(i) = args.next_arg::<*const u8>();
        }
    }
    *argv.add(argc) = ptr::null();
    let rc = execvp(file, argv);
    let saved = errno::errno_get();
    crate::mem::malloc::dealloc(argv.cast());
    errno::errno_set(saved);
    rc
}

/// `execve`, and on `ENOEXEC` the same file as a script of `/bin/sh`, whose
/// argument vector is `/bin/sh`, `path`, then `argv[1..]`.
unsafe fn exec_or_sh(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i32 {
    execve(path, argv, envp);
    if errno::errno_get() != ENOEXEC.raw() {
        return -1;
    }
    let mut rest = 0usize;
    if !argv.is_null() && !(*argv).is_null() {
        while !(*argv.add(1 + rest)).is_null() {
            rest += 1;
        }
    }
    let sh_argv = crate::mem::malloc::alloc((rest + 3) * size_of::<*const u8>()) as *mut *const u8;
    if sh_argv.is_null() {
        return -1;
    }
    *sh_argv = SH_PATH.as_ptr();
    *sh_argv.add(1) = path;
    for i in 0..rest {
        *sh_argv.add(2 + i) = *argv.add(1 + i);
    }
    *sh_argv.add(2 + rest) = ptr::null();
    execve(SH_PATH.as_ptr(), sh_argv, envp);
    let saved = errno::errno_get();
    crate::mem::malloc::dealloc(sh_argv.cast());
    errno::errno_set(saved);
    -1
}

/// The shell POSIX has `execvp` hand a file it cannot execute.
pub(crate) const SH_PATH: &[u8] = b"/bin/sh\0";

/// Returns the child PID on success, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32 {
    match Sys::waitpid(pid, status, options) {
        Ok(ret) => ret,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

/// Wait for any child process.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wait(status: *mut i32) -> i32 {
    waitpid(-1, status, 0)
}

/// Immediately terminate the whole process without cleanup.
///
/// `SYSCALL_EXIT` ends only the calling task, so this must be `exit_group`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _exit(status: i32) -> ! {
    Sys::exit_group(status)
}

// `<sysexits.h>`'s codes, which a program picks a status out of. 4.3BSD's
// values, which every Unix since has kept.
pub const EX_OK: i32 = 0;
pub const EX__BASE: i32 = 64;
pub const EX_USAGE: i32 = 64;
pub const EX_DATAERR: i32 = 65;
pub const EX_NOINPUT: i32 = 66;
pub const EX_NOUSER: i32 = 67;
pub const EX_NOHOST: i32 = 68;
pub const EX_UNAVAILABLE: i32 = 69;
pub const EX_SOFTWARE: i32 = 70;
pub const EX_OSERR: i32 = 71;
pub const EX_OSFILE: i32 = 72;
pub const EX_CANTCREAT: i32 = 73;
pub const EX_IOERR: i32 = 74;
pub const EX_TEMPFAIL: i32 = 75;
pub const EX_PROTOCOL: i32 = 76;
pub const EX_NOPERM: i32 = 77;
pub const EX_CONFIG: i32 = 78;
pub const EX__MAX: i32 = 78;

/// `_Exit(3)`. C's spelling of [`_exit`]: no `atexit` handlers, and whether
/// open streams are flushed is implementation-defined. They are not.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _Exit(status: i32) -> ! {
    _exit(status)
}

/// Clean exit — flushes stdio, runs the `atexit` and `__cxa_atexit` list,
/// then terminates.
///
/// The flush after the handlers is what C11 §7.22.4.4 requires; the one before
/// them keeps a handler that faults or calls `_exit` from discarding what
/// `main` buffered. Flushing is idempotent, so no conforming program can tell.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn exit(status: i32) -> ! {
    crate::stdio::__stdio_exit();
    // The initial thread reaches neither the thread trampoline nor
    // `pthread_exit`, so this is the only place its `thread_local`
    // destructors can run — and C++ runs them before the static ones.
    crate::cxa::run_thread_destructors();
    crate::cxa::__cxa_finalize(core::ptr::null_mut());
    crate::stdio::__stdio_exit();
    _exit(status)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpid() -> i32 {
    Sys::getpid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getppid() -> i32 {
    Sys::getppid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getuid() -> u32 {
    Sys::getuid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgid() -> u32 {
    Sys::getgid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn geteuid() -> u32 {
    Sys::geteuid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getegid() -> u32 {
    Sys::getegid()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setpgid(pid: i32, pgid: i32) -> i32 {
    match Sys::setpgid(pid, pgid) {
        Ok(()) => 0,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpgid(pid: i32) -> i32 {
    match Sys::getpgid(pid) {
        Ok(pgid) => pgid,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn getsid(pid: i32) -> i32 {
    match Sys::getsid(pid) {
        Ok(sid) => sid,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setsid() -> i32 {
    match Sys::setsid() {
        Ok(sid) => sid,
        Err(e) => {
            errno::errno_set(e.raw());
            -1
        }
    }
}
