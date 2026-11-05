//! Credentials, resource limits and the process-wide odds and ends.
//!
//! The ids are the kernel's, process-wide as POSIX has them, so no call here
//! needs to reach the other threads.

use core::ffi::{c_int, c_uint, c_ulong};
use core::sync::atomic::{AtomicU8, Ordering};

use slopos_abi::syscall::{
    SYSCALL_GETGROUPS, SYSCALL_GETRESGID, SYSCALL_GETRESUID, SYSCALL_SETGID, SYSCALL_SETGROUPS,
    SYSCALL_SETREGID, SYSCALL_SETRESGID, SYSCALL_SETRESUID, SYSCALL_SETREUID, SYSCALL_SETUID,
};

use crate::errno::{EINVAL, ENOSYS, errno_set};
use crate::pal::raw::{syscall1, syscall2, syscall3};
use crate::pal::{Pal, Sys};
use crate::types::{gid_t, itimerval, pid_t, rlimit, rusage, uid_t};

/// `(uid_t)-1`: an id the set*id calls leave alone.
const KEEP: u64 = u32::MAX as u64;

fn answer(ret: u64) -> c_int {
    match crate::demux(ret) {
        Ok(value) => value as c_int,
        Err(e) => {
            errno_set(e.errno());
            -1
        }
    }
}

/// # Safety
///
/// None: the argument is a plain integer the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setuid(uid: uid_t) -> c_int {
    answer(syscall1(SYSCALL_SETUID, u64::from(uid)))
}

/// # Safety
///
/// None: the argument is a plain integer the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setgid(gid: gid_t) -> c_int {
    answer(syscall1(SYSCALL_SETGID, u64::from(gid)))
}

/// `seteuid(-1)` is `EINVAL`, as POSIX lets it be: `setresuid` would read
/// the same id as "leave it".
///
/// # Safety
///
/// None: the argument is a plain integer the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn seteuid(uid: uid_t) -> c_int {
    set_effective(SYSCALL_SETRESUID, uid)
}

/// # Safety
///
/// None: the argument is a plain integer the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setegid(gid: gid_t) -> c_int {
    set_effective(SYSCALL_SETRESGID, gid)
}

fn set_effective(call: u64, id: u32) -> c_int {
    if u64::from(id) == KEEP {
        errno_set(EINVAL.raw());
        return -1;
    }
    answer(unsafe { syscall3(call, KEEP, u64::from(id), KEEP) })
}

/// # Safety
///
/// None: the arguments are plain integers the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setreuid(ruid: uid_t, euid: uid_t) -> c_int {
    answer(syscall2(SYSCALL_SETREUID, u64::from(ruid), u64::from(euid)))
}

/// # Safety
///
/// None: the arguments are plain integers the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setregid(rgid: gid_t, egid: gid_t) -> c_int {
    answer(syscall2(SYSCALL_SETREGID, u64::from(rgid), u64::from(egid)))
}

/// # Safety
///
/// None: the arguments are plain integers the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setresuid(ruid: uid_t, euid: uid_t, suid: uid_t) -> c_int {
    answer(syscall3(
        SYSCALL_SETRESUID,
        u64::from(ruid),
        u64::from(euid),
        u64::from(suid),
    ))
}

/// # Safety
///
/// None: the arguments are plain integers the kernel validates; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setresgid(rgid: gid_t, egid: gid_t, sgid: gid_t) -> c_int {
    answer(syscall3(
        SYSCALL_SETRESGID,
        u64::from(rgid),
        u64::from(egid),
        u64::from(sgid),
    ))
}

/// # Safety
///
/// `ruid`, `euid` and `suid` must each be valid for writes of one `uid_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getresuid(ruid: *mut uid_t, euid: *mut uid_t, suid: *mut uid_t) -> c_int {
    answer(syscall3(
        SYSCALL_GETRESUID,
        ruid as u64,
        euid as u64,
        suid as u64,
    ))
}

/// # Safety
///
/// `rgid`, `egid` and `sgid` must each be valid for writes of one `gid_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getresgid(rgid: *mut gid_t, egid: *mut gid_t, sgid: *mut gid_t) -> c_int {
    answer(syscall3(
        SYSCALL_GETRESGID,
        rgid as u64,
        egid as u64,
        sgid as u64,
    ))
}

/// # Safety
///
/// `ptr` must be valid for reads of `ngroups` initialized `gid_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setgroups(ngroups: usize, ptr: *const gid_t) -> c_int {
    answer(syscall2(SYSCALL_SETGROUPS, ngroups as u64, ptr as u64))
}

/// A zero `ngroups` is the sizing query, as POSIX has it.
///
/// # Safety
///
/// When `ngroups` is nonzero, `ptr` must be valid for writes of `ngroups`
/// `gid_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgroups(ngroups: c_int, ptr: *mut gid_t) -> c_int {
    answer(syscall2(SYSCALL_GETGROUPS, ngroups as u64, ptr as u64))
}

/// `getpgrp()` is `getpgid(0)`, which is all it has ever been.
///
/// # Safety
///
/// None: it takes no arguments; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getpgrp() -> pid_t {
    match Sys::getpgid(0) {
        Ok(pgid) => pgid,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `wait4(2)`. The kernel writes `usage` as `slopos_abi::syscall::Rusage`
/// says: the child's CPU time and its peak resident set.
///
/// # Safety
///
/// `status` must be null or valid for writes of one `c_int`; `usage` must be
/// null or valid for writes of one `rusage`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wait4(
    pid: pid_t,
    status: *mut c_int,
    options: c_int,
    usage: *mut rusage,
) -> pid_t {
    match crate::process::times::reap(pid, status, options, usage) {
        Ok(child) => child,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// Linux's values, for `getitimer` and `setitimer`.
pub const ITIMER_REAL: c_int = 0;
pub const ITIMER_VIRTUAL: c_int = 1;
pub const ITIMER_PROF: c_int = 2;

/// There is no per-process interval timer, so an alarm cannot be armed.
/// `alarm` has no failure return of its own — 0 means "none was pending" —
/// so the refusal is reported through `errno` alone.
///
/// # Safety
///
/// None: the argument is ignored; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alarm(_seconds: c_uint) -> c_uint {
    errno_set(ENOSYS.raw());
    0
}

/// `getitimer(2)`: no timer is ever armed, so every one reads zero.
///
/// # Safety
///
/// `value` must be null or valid for writes of one `itimerval`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getitimer(which: c_int, value: *mut itimerval) -> c_int {
    if !matches!(which, ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF) || value.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    *value = itimerval::default();
    0
}

/// `setitimer(2)`. Disarming — a zero `it_value` — succeeds, there being
/// nothing armed; arming is `ENOSYS`, as for `alarm`.
///
/// # Safety
///
/// `value` must be null or point to a valid `itimerval`; `ovalue` must be
/// null or valid for writes of one `itimerval`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setitimer(
    which: c_int,
    value: *const itimerval,
    ovalue: *mut itimerval,
) -> c_int {
    if !matches!(which, ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF) || value.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let it = (*value).it_value;
    if it.tv_sec != 0 || it.tv_usec != 0 {
        errno_set(ENOSYS.raw());
        return -1;
    }
    if !ovalue.is_null() {
        *ovalue = itimerval::default();
    }
    0
}

pub const PR_GET_DUMPABLE: c_int = 3;
pub const PR_SET_DUMPABLE: c_int = 4;
pub const PR_SET_NO_NEW_PRIVS: c_int = 38;
pub const PR_GET_NO_NEW_PRIVS: c_int = 39;

const DUMPABLE_UNSET: u8 = 2;
static DUMPABLE: AtomicU8 = AtomicU8::new(DUMPABLE_UNSET);

/// Whether the process may be dumped. Nothing here dumps or traces a
/// process, so the flag is only reported; a program started with authority
/// its caller lacks (`AT_SECURE`) starts not dumpable, as a setuid one does
/// on Linux, which is what a caller asking means to learn.
fn dumpable() -> bool {
    match DUMPABLE.load(Ordering::Relaxed) {
        DUMPABLE_UNSET => !crate::auxv::tag(slopos_abi::auxv::AT_SECURE).is_some_and(|v| v != 0),
        flag => flag != 0,
    }
}

/// `prctl(2)`, kept by libc: the kernel has no counterpart, and only the
/// dumpable flag has state to keep. No exec ever honours a set-ID bit, so
/// every process already has no new privileges to gain: asking reports 1 and
/// setting it changes nothing. Any other option — `PR_SET_NAME`,
/// `PR_SET_PDEATHSIG` — is `ENOSYS`; a thread name is kept through
/// `pthread_setname_np`.
///
/// # Safety
///
/// For `PR_SET_NO_NEW_PRIVS` and `PR_SET_DUMPABLE` the caller must pass a
/// second, `unsigned long` variadic argument; other options read none.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prctl(option: c_int, mut args: ...) -> c_int {
    match option {
        PR_GET_NO_NEW_PRIVS => 1,
        PR_SET_NO_NEW_PRIVS => match args.next_arg::<c_ulong>() {
            1 => 0,
            _ => {
                errno_set(EINVAL.raw());
                -1
            }
        },
        PR_GET_DUMPABLE => c_int::from(dumpable()),
        PR_SET_DUMPABLE => match args.next_arg::<c_ulong>() {
            value @ (0 | 1) => {
                DUMPABLE.store(value as u8, Ordering::Relaxed);
                0
            }
            _ => {
                errno_set(EINVAL.raw());
                -1
            }
        },
        _ => {
            errno_set(ENOSYS.raw());
            -1
        }
    }
}

/// `getrlimit(2)`, the C form. The Rust-typed helpers next door in
/// [`crate::process::rlimit`] are what slibc itself uses.
///
/// # Safety
///
/// `rlim` must be null or valid for writes of one `rlimit`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrlimit(resource: c_int, rlim: *mut rlimit) -> c_int {
    if rlim.is_null() || resource < 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    crate::process::rlimit::getrlimit(resource as u32, &mut *rlim)
}

/// # Safety
///
/// `rlim` must be null or point to a valid, initialized `rlimit`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setrlimit(resource: c_int, rlim: *const rlimit) -> c_int {
    if rlim.is_null() || resource < 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    crate::process::rlimit::setrlimit(resource as u32, &*rlim)
}

pub const RUSAGE_SELF: c_int = 0;
pub const RUSAGE_CHILDREN: c_int = -1;

/// A reaped child's usage is `wait4`'s to report; the kernel has no call that
/// answers for a live process, and a zeroed `struct rusage` is
/// indistinguishable from a process that has used no time at all.
///
/// # Safety
///
/// None: the arguments are never read; it is `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrusage(_who: c_int, _usage: *mut rusage) -> c_int {
    errno_set(ENOSYS.raw());
    -1
}

pub const PRIO_PROCESS: c_int = 0;
pub const PRIO_PGRP: c_int = 1;
pub const PRIO_USER: c_int = 2;
const _: () = assert!(
    PRIO_PROCESS as u64 == slopos_abi::syscall::PRIO_PROCESS
        && PRIO_PGRP as u64 == slopos_abi::syscall::PRIO_PGRP
        && PRIO_USER as u64 == slopos_abi::syscall::PRIO_USER
);

/// `getpriority(2)`: the nice value, -20 to 19. Since -1 is a valid answer, a
/// caller tells failure apart by clearing `errno` first.
#[unsafe(no_mangle)]
pub extern "C" fn getpriority(which: c_int, who: crate::types::id_t) -> c_int {
    match Sys::getpriority(which, who) {
        Ok(encoded) => 20 - encoded,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// `setpriority(2)`: `nice` is clamped to -20..=19, and lowering a task's
/// below both 0 and its current value is init's alone.
#[unsafe(no_mangle)]
pub extern "C" fn setpriority(which: c_int, who: crate::types::id_t, nice: c_int) -> c_int {
    match Sys::setpriority(which, who, nice) {
        Ok(()) => 0,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}
