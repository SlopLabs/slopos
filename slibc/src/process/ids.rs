//! Credentials, resource limits and the process-wide odds and ends.
//!
//! SlopOS is single-user. The only uid and gid that exist are 0, and the
//! kernel has no privilege principal to switch between, so every *setter*
//! below succeeds for 0 and answers `EPERM` for anything else: there is
//! nothing to become. That is the whole of the reasoning, stated once.

use core::ffi::{c_int, c_uint};

use crate::errno::{EINVAL, ENOSYS, EPERM, errno_set};
use crate::pal::{Pal, Sys};
use crate::types::{gid_t, pid_t, rlimit, rusage, uid_t};

/// The only principal that exists.
const ROOT: u32 = 0;

#[inline]
fn only_root(id: u32) -> c_int {
    if id == ROOT {
        0
    } else {
        errno_set(EPERM.raw());
        -1
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setuid(uid: uid_t) -> c_int {
    only_root(uid)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn seteuid(uid: uid_t) -> c_int {
    only_root(uid)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setgid(gid: gid_t) -> c_int {
    only_root(gid)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn setegid(gid: gid_t) -> c_int {
    only_root(gid)
}

/// The supplementary set is fixed at `{0}`, so a call that asks for exactly
/// that — or for nothing — succeeds and anything else is `EPERM`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn setgroups(ngroups: usize, ptr: *const gid_t) -> c_int {
    if ngroups == 0 {
        return 0;
    }
    if ptr.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    for i in 0..ngroups {
        if *ptr.add(i) != ROOT {
            errno_set(EPERM.raw());
            return -1;
        }
    }
    0
}

/// One group, gid 0. A zero `ngroups` is the sizing query, as POSIX has it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getgroups(ngroups: c_int, ptr: *mut gid_t) -> c_int {
    if ngroups < 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    if ngroups == 0 {
        return 1;
    }
    if ptr.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    *ptr = ROOT;
    1
}

/// `getpgrp()` is `getpgid(0)`, which is all it has ever been.
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

/// `wait4(2)`.
///
/// A non-null `usage` is `EINVAL`: the kernel keeps no per-task accounting,
/// and answering with a zeroed struct would be a lie a caller cannot detect.
/// `wait4(pid, status, options, NULL)` is the full-strength call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wait4(
    pid: pid_t,
    status: *mut c_int,
    options: c_int,
    usage: *mut rusage,
) -> pid_t {
    if !usage.is_null() {
        errno_set(EINVAL.raw());
        return -1;
    }
    match Sys::wait4(pid, status, options, core::ptr::null_mut()) {
        Ok(child) => child,
        Err(e) => {
            errno_set(e.raw());
            -1
        }
    }
}

/// There is no per-process interval timer and no `setitimer`, so an alarm
/// cannot be armed. `alarm` has no failure return of its own — 0 means "none
/// was pending" — so the refusal is reported through `errno` alone.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn alarm(_seconds: c_uint) -> c_uint {
    errno_set(ENOSYS.raw());
    0
}

/// `prctl(2)` has no kernel counterpart here: not one of its options —
/// `PR_SET_NAME`, `PR_SET_PDEATHSIG`, `PR_SET_DUMPABLE` — has state to set. A
/// thread name is kept by libc instead, through `pthread_setname_np`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn prctl(_option: c_int, _args: ...) -> c_int {
    errno_set(ENOSYS.raw());
    -1
}

/// `getrlimit(2)`, the C form. The Rust-typed helpers next door in
/// [`crate::process::rlimit`] are what slibc itself uses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrlimit(resource: c_int, rlim: *mut rlimit) -> c_int {
    if rlim.is_null() || resource < 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    crate::process::rlimit::getrlimit(resource as u32, &mut *rlim)
}

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

/// No per-task accounting exists to report, and a zeroed `struct rusage` is
/// indistinguishable from a process that has used no time at all.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn getrusage(_who: c_int, _usage: *mut rusage) -> c_int {
    errno_set(ENOSYS.raw());
    -1
}
