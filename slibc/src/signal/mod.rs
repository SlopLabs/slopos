//! Signal handling — taming the chaos of asynchronous fate.
//!
//! Two shapes meet here. Userland sees the 152-byte `struct sigaction` and the
//! 128-byte `sigset_t` the target's `libc` declares; the kernel takes a
//! 32-byte `UserSigaction` and a single-`u64` mask. Every function below is
//! the translation between them, and the kernel structs are not changed to
//! suit libc.
//!
//! The kernel accepts signals `1..=NSIG` — `1..=32`, bit `N-1` of its mask —
//! and has no realtime signals beyond that. So the whole of a `sigset_t` that
//! can mean anything lives in word 0, and libc refuses exactly what the
//! kernel refuses: a signal *number* outside that range is `EINVAL`, and a
//! *mask* word above it is dropped, because a set bit for a signal that
//! cannot be raised has nothing to block.

pub mod tests;

use core::ffi::{c_char, c_int, c_uint};
use core::mem;

use crate::errno::{EINTR, EINVAL, ENOSYS, errno_set};
use crate::pal::slopos::signal_restorer_addr;
use crate::pal::{Pal, Sys};
use crate::types::{KERNEL_SIGSET_MASK, sigaction as SigAction, sigset_t, stack_t};
use slopos_abi::signal::{UserSigAltStack, UserSigaction};

/// True when `handler` is a real function pointer (not `SIG_DFL`/`SIG_IGN`).
/// The kernel rejects (`EINVAL`) such a handler with a zero `sa_restorer`, so
/// libc injects its own restorer for exactly these.
#[inline]
fn is_catchable_handler(handler: u64) -> bool {
    handler != slopos_abi::signal::SIG_DFL && handler != slopos_abi::signal::SIG_IGN
}

pub const SIGHUP: i32 = slopos_abi::signal::SIGHUP as i32;
pub const SIGINT: i32 = slopos_abi::signal::SIGINT as i32;
pub const SIGQUIT: i32 = slopos_abi::signal::SIGQUIT as i32;
pub const SIGILL: i32 = slopos_abi::signal::SIGILL as i32;
pub const SIGTRAP: i32 = slopos_abi::signal::SIGTRAP as i32;
pub const SIGABRT: i32 = slopos_abi::signal::SIGABRT as i32;
pub const SIGBUS: i32 = slopos_abi::signal::SIGBUS as i32;
pub const SIGFPE: i32 = slopos_abi::signal::SIGFPE as i32;
pub const SIGKILL: i32 = slopos_abi::signal::SIGKILL as i32;
pub const SIGUSR1: i32 = slopos_abi::signal::SIGUSR1 as i32;
pub const SIGSEGV: i32 = slopos_abi::signal::SIGSEGV as i32;
pub const SIGUSR2: i32 = slopos_abi::signal::SIGUSR2 as i32;
pub const SIGPIPE: i32 = slopos_abi::signal::SIGPIPE as i32;
pub const SIGALRM: i32 = slopos_abi::signal::SIGALRM as i32;
pub const SIGTERM: i32 = slopos_abi::signal::SIGTERM as i32;
pub const SIGCHLD: i32 = slopos_abi::signal::SIGCHLD as i32;
pub const SIGCONT: i32 = slopos_abi::signal::SIGCONT as i32;
pub const SIGSTOP: i32 = slopos_abi::signal::SIGSTOP as i32;
pub const SIGTSTP: i32 = slopos_abi::signal::SIGTSTP as i32;
pub const SIGTTIN: i32 = slopos_abi::signal::SIGTTIN as i32;
pub const SIGTTOU: i32 = slopos_abi::signal::SIGTTOU as i32;
pub const SIGWINCH: i32 = slopos_abi::signal::SIGWINCH as i32;

pub const SIG_DFL: usize = slopos_abi::signal::SIG_DFL as usize;
pub const SIG_IGN: usize = slopos_abi::signal::SIG_IGN as usize;
/// `signal()`'s failure return.
pub const SIG_ERR: usize = usize::MAX;

pub const SIG_BLOCK: c_int = slopos_abi::signal::SIG_BLOCK as c_int;
pub const SIG_UNBLOCK: c_int = slopos_abi::signal::SIG_UNBLOCK as c_int;
pub const SIG_SETMASK: c_int = slopos_abi::signal::SIG_SETMASK as c_int;

pub const SS_ONSTACK: c_int = slopos_abi::signal::SS_ONSTACK as c_int;
pub const SS_DISABLE: c_int = slopos_abi::signal::SS_DISABLE as c_int;

pub type SigHandler = unsafe extern "C" fn(i32);

/// The kernel's `sigsetsize` argument: it accepts 8 and nothing else.
const SIGSET_SIZE: usize = mem::size_of::<u64>();

/// Highest signal number the kernel accepts. Its `parse_signum` admits
/// `1..=NSIG` inclusive, so libc must too: refusing 32 here would reject a
/// number `kill` would deliver.
const SIGNAL_MAX: c_int = crate::types::NSIG;

#[inline]
fn signal_in_range(sig: c_int) -> bool {
    (1..=SIGNAL_MAX).contains(&sig)
}

/// Install a handler, BSD-style. Returns the previous handler, or `SIG_ERR`
/// (`usize::MAX` cast) on error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn signal(signum: c_int, handler: usize) -> usize {
    let mut act: UserSigaction = mem::zeroed();
    act.sa_handler = handler as u64;
    act.sa_flags = slopos_abi::signal::SA_RESTART;
    act.sa_mask = 0;
    act.sa_restorer = if is_catchable_handler(act.sa_handler) {
        signal_restorer_addr()
    } else {
        0
    };

    let mut old_act: UserSigaction = mem::zeroed();

    match Sys::rt_sigaction(
        signum,
        &act as *const UserSigaction as *const u8,
        &mut old_act as *mut UserSigaction as *mut u8,
        SIGSET_SIZE,
    ) {
        Ok(()) => old_act.sa_handler as usize,
        Err(e) => {
            errno_set(e.raw());
            SIG_ERR
        }
    }
}

/// Examine or change a signal action.
///
/// The libc-declared `sa_mask` is 128 bytes and the kernel's is 8; only bits
/// for signals `1..=31` survive the narrowing, which is every signal that
/// exists. `sa_restorer` is injected when the caller left it null and the
/// handler is catchable — without one the kernel refuses the install.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigaction(
    signum: c_int,
    act: *const SigAction,
    oldact: *mut SigAction,
) -> c_int {
    if !signal_in_range(signum) {
        errno_set(EINVAL.raw());
        return -1;
    }

    let mut kernel_act: UserSigaction = mem::zeroed();
    let act_ptr: *const u8 = if act.is_null() {
        core::ptr::null()
    } else {
        let a = &*act;
        kernel_act.sa_handler = a.sa_sigaction as u64;
        // `sa_flags` is a signed `int` in userland and a `u64` in the kernel;
        // `SA_RESETHAND` has bit 31 set, so the widening must go through u32.
        kernel_act.sa_flags = a.sa_flags as u32 as u64;
        kernel_act.sa_mask = a.sa_mask.kernel_mask();
        kernel_act.sa_restorer = match a.sa_restorer {
            Some(f) => f as *const () as u64,
            None if is_catchable_handler(kernel_act.sa_handler) => signal_restorer_addr(),
            None => 0,
        };
        &raw const kernel_act as *const u8
    };

    let mut kernel_old: UserSigaction = mem::zeroed();
    let old_ptr = if oldact.is_null() {
        core::ptr::null_mut()
    } else {
        &raw mut kernel_old as *mut u8
    };

    match Sys::rt_sigaction(signum, act_ptr, old_ptr, SIGSET_SIZE) {
        Ok(()) => {
            if !oldact.is_null() {
                *oldact = SigAction {
                    sa_sigaction: kernel_old.sa_handler as usize,
                    sa_mask: sigset_t::from_kernel_mask(kernel_old.sa_mask),
                    sa_flags: kernel_old.sa_flags as u32 as c_int,
                    sa_restorer: if kernel_old.sa_restorer == 0 {
                        None
                    } else {
                        Some(mem::transmute::<u64, extern "C" fn()>(
                            kernel_old.sa_restorer,
                        ))
                    },
                };
            }
            0
        }
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `sigaltstack(2)`. The kernel's `stack_t` already *is* Linux's, so this is a
/// straight call with the C return convention put back on.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigaltstack(ss: *const stack_t, oss: *mut stack_t) -> c_int {
    match Sys::sigaltstack(ss as *const UserSigAltStack, oss as *mut UserSigAltStack) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigemptyset(set: *mut sigset_t) -> c_int {
    if set.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    *set = sigset_t::empty();
    0
}

/// Fills the bits for the signals that exist, and no others: a set bit for a
/// realtime signal would be a claim this kernel cannot honour.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigfillset(set: *mut sigset_t) -> c_int {
    if set.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    *set = sigset_t::from_kernel_mask(KERNEL_SIGSET_MASK);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigaddset(set: *mut sigset_t, sig: c_int) -> c_int {
    if set.is_null() || !signal_in_range(sig) {
        errno_set(EINVAL.raw());
        return -1;
    }
    (*set).__val[0] |= 1u64 << (sig - 1);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigdelset(set: *mut sigset_t, sig: c_int) -> c_int {
    if set.is_null() || !signal_in_range(sig) {
        errno_set(EINVAL.raw());
        return -1;
    }
    (*set).__val[0] &= !(1u64 << (sig - 1));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigismember(set: *const sigset_t, sig: c_int) -> c_int {
    if set.is_null() || !signal_in_range(sig) {
        errno_set(EINVAL.raw());
        return -1;
    }
    if (*set).__val[0] & (1u64 << (sig - 1)) != 0 {
        1
    } else {
        0
    }
}

/// Examine or change the blocked signal mask. `how` is `SIG_BLOCK`,
/// `SIG_UNBLOCK` or `SIG_SETMASK`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigprocmask(
    how: c_int,
    set: *const sigset_t,
    oldset: *mut sigset_t,
) -> c_int {
    match mask_op(how, set, oldset) {
        0 => 0,
        err => {
            errno_set(err);
            -1
        }
    }
}

/// Every task here is a thread of one process and the kernel keeps one mask
/// per task, so this is `sigprocmask` with the errno returned rather than set
/// — which is the only difference POSIX draws between the two.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_sigmask(
    how: c_int,
    set: *const sigset_t,
    oldset: *mut sigset_t,
) -> c_int {
    mask_op(how, set, oldset)
}

/// Shared body of `sigprocmask`/`pthread_sigmask`. Answers 0 or an errno.
unsafe fn mask_op(how: c_int, set: *const sigset_t, oldset: *mut sigset_t) -> c_int {
    if !set.is_null() && how != SIG_BLOCK && how != SIG_UNBLOCK && how != SIG_SETMASK {
        return EINVAL.raw();
    }

    // Initialised once rather than zeroed and overwritten: the kernel reads
    // the word only through `new_ptr`, which is null exactly when there was
    // no set to narrow.
    let kernel_new = if set.is_null() {
        0
    } else {
        (*set).kernel_mask()
    };
    let new_ptr = if set.is_null() {
        core::ptr::null()
    } else {
        &raw const kernel_new
    };
    let mut kernel_old = 0u64;
    let old_ptr = if oldset.is_null() {
        core::ptr::null_mut()
    } else {
        &raw mut kernel_old
    };

    match Sys::rt_sigprocmask(how, new_ptr, old_ptr, SIGSET_SIZE) {
        Ok(()) => {
            if !oldset.is_null() {
                *oldset = sigset_t::from_kernel_mask(kernel_old);
            }
            0
        }
        Err(e) => e.raw(),
    }
}

/// SlopOS has no `rt_sigpending`: the pending set lives only in the task
/// struct and no syscall publishes it, so there is nothing to report.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigpending(set: *mut sigset_t) -> c_int {
    if set.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    errno_set(ENOSYS.raw());
    -1
}

/// Replace the mask and block until a signal arrives, then restore it.
///
/// There is no `pause(2)` here, but `nanosleep(2)` reports `EINTR` the moment
/// a deliverable signal exists, so a repeated long sleep is the wait: the only
/// difference from an unbounded one is that it wakes and re-sleeps every
/// [`SIGSUSPEND_SLICE_SECS`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigsuspend(set: *const sigset_t) -> c_int {
    if set.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }

    let mut saved = 0u64;
    let wanted = (*set).kernel_mask();
    if let Err(e) = Sys::rt_sigprocmask(SIG_SETMASK, &raw const wanted, &raw mut saved, SIGSET_SIZE)
    {
        errno_set(e.raw());
        return -1;
    }

    let slice = crate::time::Timespec {
        tv_sec: SIGSUSPEND_SLICE_SECS,
        tv_nsec: 0,
    };
    loop {
        match Sys::nanosleep(&raw const slice, core::ptr::null_mut()) {
            // A full slice elapsed with nothing pending: keep waiting.
            Ok(()) => continue,
            Err(_) => break,
        }
    }

    let _ = Sys::rt_sigprocmask(
        SIG_SETMASK,
        &raw const saved,
        core::ptr::null_mut(),
        SIGSET_SIZE,
    );
    // `sigsuspend` has no success return: it always answers -1/EINTR once the
    // handler has run.
    errno_set(EINTR.raw());
    -1
}

/// How long one `sigsuspend` sleep lasts before it is re-armed.
const SIGSUSPEND_SLICE_SECS: i64 = 3600;

/// Send a signal to a process. Returns 0, or -1 with errno set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kill(pid: i32, sig: c_int) -> c_int {
    match Sys::kill(pid, sig) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `kill(-pgrp)`, which is how the process-group fan-out is spelled at the
/// syscall. A zero `pgrp` means the caller's own group, which `kill(0)` is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn killpg(pgrp: i32, sig: c_int) -> c_int {
    if pgrp < 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    kill(if pgrp == 0 { 0 } else { -pgrp }, sig)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn raise(sig: c_int) -> c_int {
    kill(Sys::getpid(), sig)
}

/// Abort the process — sends SIGABRT, then force-exits if the handler returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn abort() -> ! {
    let _ = raise(SIGABRT);
    crate::process::_exit(134)
}

/// What `<assert.h>`'s `assert` expands to when it fails. Writes straight to
/// fd 2 rather than through `stderr`, because a failed assertion is as likely
/// to be about the stdio lock as about anything else.
///
/// # Safety
/// Every argument is a NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __slibc_assert_fail(
    expr: *const c_char,
    file: *const c_char,
    line: c_uint,
    func: *const c_char,
) -> ! {
    let say = |text: *const c_char| {
        if !text.is_null() {
            let bytes = text as *const u8;
            let _ = Sys::write(2, bytes, crate::string::u_strlen(bytes));
        }
    };
    say(file.cast());
    say(c":".as_ptr());
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    let mut value = line;
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 || at == 0 {
            break;
        }
    }
    let _ = Sys::write(2, digits[at..].as_ptr(), digits.len() - at);
    say(c": ".as_ptr());
    say(func);
    say(c": assertion failed: ".as_ptr());
    say(expr);
    say(c"\n".as_ptr());
    abort()
}

/// `strsignal(3)`. A static description, never NULL. glibc renders an unknown
/// number into a per-thread buffer; there is nothing to render it from here
/// that the caller does not already hold.
#[unsafe(no_mangle)]
pub extern "C" fn strsignal(sig: i32) -> *mut core::ffi::c_char {
    let text: &[u8] = match sig {
        SIGHUP => b"Hangup\0",
        SIGINT => b"Interrupt\0",
        SIGQUIT => b"Quit\0",
        SIGILL => b"Illegal instruction\0",
        SIGTRAP => b"Trace/breakpoint trap\0",
        SIGABRT => b"Aborted\0",
        SIGBUS => b"Bus error\0",
        SIGFPE => b"Floating point exception\0",
        SIGKILL => b"Killed\0",
        SIGUSR1 => b"User defined signal 1\0",
        SIGSEGV => b"Segmentation fault\0",
        SIGUSR2 => b"User defined signal 2\0",
        SIGPIPE => b"Broken pipe\0",
        SIGALRM => b"Alarm clock\0",
        SIGTERM => b"Terminated\0",
        SIGCHLD => b"Child exited\0",
        SIGCONT => b"Continued\0",
        SIGSTOP => b"Stopped (signal)\0",
        SIGTSTP => b"Stopped\0",
        SIGTTIN => b"Stopped (tty input)\0",
        SIGTTOU => b"Stopped (tty output)\0",
        SIGWINCH => b"Window changed\0",
        _ => b"Unknown signal\0",
    };
    text.as_ptr() as *mut core::ffi::c_char
}
