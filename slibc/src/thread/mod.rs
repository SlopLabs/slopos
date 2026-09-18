#![allow(non_camel_case_types)]

pub mod condvar;
pub mod create;
pub(crate) mod futex;
pub mod join;
pub mod keys;
pub mod mutex;
pub mod rwlock;
#[allow(dead_code)]
pub(crate) mod shim;
pub mod tcb;
pub mod tests;
pub mod tls;

use core::ffi::{c_char, c_int, c_void};

use crate::errno::{EINVAL, ENOSYS, ESRCH, errno_set};
use crate::pal::{Pal, Sys};

use tcb::Tcb;

pub type pthread_t = u64;

/// `pthread_attr_t` is 56 bytes because that is what the target's `libc`
/// declares. The fields inside are slibc's choice; only the size and alignment
/// are shared, and [`crate::types`] pins both.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct pthread_attr_t {
    pub detach_state: i32,
    pub sched_policy: i32,
    pub stack_size: usize,
    /// Lowest usable byte of a caller-supplied stack, or null for "allocate".
    pub stack_addr: *mut u8,
    pub guard_size: usize,
    pub sched_priority: i32,
    pub scope: i32,
    pub inherit_sched: i32,
    pub _reserved0: i32,
    pub _reserved1: u64,
}

pub const PTHREAD_CREATE_JOINABLE: i32 = 0;
pub const PTHREAD_CREATE_DETACHED: i32 = 1;
pub const DEFAULT_STACK_SIZE: usize = 2 * 1024 * 1024;
pub const PTHREAD_STACK_MIN: usize = 16384;

/// `pthread_setname_np` truncates at this, as Linux's `PR_SET_NAME` does.
pub const PTHREAD_NAME_MAX: usize = 16;

pub use condvar::{PTHREAD_COND_INITIALIZER, pthread_cond_t};
pub use create::pthread_create;
pub use join::{pthread_detach, pthread_equal, pthread_exit, pthread_join, pthread_self};
pub use keys::pthread_key_t;
pub use mutex::{
    PTHREAD_MUTEX_ERRORCHECK, PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_NORMAL,
    PTHREAD_MUTEX_RECURSIVE, pthread_mutex_t,
};
pub use rwlock::{PTHREAD_RWLOCK_INITIALIZER, pthread_rwlock_t};

impl pthread_attr_t {
    pub const fn zeroed() -> Self {
        Self {
            detach_state: PTHREAD_CREATE_JOINABLE,
            sched_policy: 0,
            stack_size: 0,
            stack_addr: core::ptr::null_mut(),
            guard_size: 0,
            sched_priority: 0,
            scope: 0,
            inherit_sched: 0,
            _reserved0: 0,
            _reserved1: 0,
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_init(attr: *mut pthread_attr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    *attr = pthread_attr_t::zeroed();
    (*attr).stack_size = DEFAULT_STACK_SIZE;
    (*attr).guard_size = create::THREAD_STACK_GUARD_SIZE;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_destroy(attr: *mut pthread_attr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    *attr = pthread_attr_t::zeroed();
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setstacksize(
    attr: *mut pthread_attr_t,
    stacksize: usize,
) -> c_int {
    if attr.is_null() || stacksize < PTHREAD_STACK_MIN {
        return EINVAL.raw();
    }
    (*attr).stack_size = stacksize;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getstacksize(
    attr: *const pthread_attr_t,
    stacksize: *mut usize,
) -> c_int {
    if attr.is_null() || stacksize.is_null() {
        return EINVAL.raw();
    }
    *stacksize = (*attr).stack_size;
    0
}

/// Reports the *usable* stack: the guard page sits immediately below
/// `stackaddr`, which is the convention `pthread_attr_getguardsize` documents.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getstack(
    attr: *const pthread_attr_t,
    stackaddr: *mut *mut c_void,
    stacksize: *mut usize,
) -> c_int {
    if attr.is_null() || stackaddr.is_null() || stacksize.is_null() {
        return EINVAL.raw();
    }
    *stackaddr = (*attr).stack_addr as *mut c_void;
    *stacksize = (*attr).stack_size;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getguardsize(
    attr: *const pthread_attr_t,
    guardsize: *mut usize,
) -> c_int {
    if attr.is_null() || guardsize.is_null() {
        return EINVAL.raw();
    }
    *guardsize = (*attr).guard_size;
    0
}

/// The guard is one unmapped page or none; a request in between is rounded up
/// to a page, as POSIX permits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setguardsize(
    attr: *mut pthread_attr_t,
    guardsize: usize,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).guard_size = if guardsize == 0 {
        0
    } else {
        create::THREAD_STACK_GUARD_SIZE
    };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_setdetachstate(
    attr: *mut pthread_attr_t,
    detachstate: c_int,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    if detachstate != PTHREAD_CREATE_JOINABLE && detachstate != PTHREAD_CREATE_DETACHED {
        return EINVAL.raw();
    }
    (*attr).detach_state = detachstate;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_attr_getdetachstate(
    attr: *const pthread_attr_t,
    detachstate: *mut c_int,
) -> c_int {
    if attr.is_null() || detachstate.is_null() {
        return EINVAL.raw();
    }
    *detachstate = (*attr).detach_state;
    0
}

/// Describe a live thread's stack.
///
/// Fails with `ENOENT` for a thread whose stack this library did not allocate
/// — the initial thread runs on the kernel-provided stack, whose extent and
/// guard are not knowable from here. Failing is the correct answer rather than
/// a guess: a caller that believes a wrong guard range mis-attributes the next
/// fault in it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_getattr_np(native: pthread_t, attr: *mut pthread_attr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    if native == 0 {
        return ESRCH.raw();
    }
    let tcb = native as *mut Tcb;
    let base = (*tcb).stack_base;
    let mapped = (*tcb).stack_size;
    let guard = (*tcb).guard_size;
    if base.is_null() || mapped <= guard {
        return crate::errno::ENOENT.raw();
    }

    *attr = pthread_attr_t::zeroed();
    (*attr).detach_state = if (*tcb).detached {
        PTHREAD_CREATE_DETACHED
    } else {
        PTHREAD_CREATE_JOINABLE
    };
    (*attr).stack_addr = base.add(guard);
    (*attr).stack_size = mapped - guard;
    (*attr).guard_size = guard;
    0
}

/// Names are kept in the TCB: there is no `PR_SET_NAME` here, so a name is
/// libc-visible only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_setname_np(thread: pthread_t, name: *const c_char) -> c_int {
    if thread == 0 || name.is_null() {
        return EINVAL.raw();
    }
    let len = crate::string::u_strlen(name as *const u8);
    if len >= PTHREAD_NAME_MAX {
        return crate::errno::ERANGE.raw();
    }
    let tcb = thread as *mut Tcb;
    (*tcb).name = [0; PTHREAD_NAME_MAX];
    core::ptr::copy_nonoverlapping(name as *const u8, (*tcb).name.as_mut_ptr(), len);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_getname_np(
    thread: pthread_t,
    name: *mut c_char,
    len: usize,
) -> c_int {
    if thread == 0 || name.is_null() {
        return EINVAL.raw();
    }
    let tcb = thread as *mut Tcb;
    let stored = &(*tcb).name;
    let n = crate::string::u_strnlen(stored.as_ptr(), PTHREAD_NAME_MAX);
    if len <= n {
        return crate::errno::ERANGE.raw();
    }
    core::ptr::copy_nonoverlapping(stored.as_ptr(), name as *mut u8, n);
    *(name as *mut u8).add(n) = 0;
    0
}

/// Directed thread signals need `tgkill`, which SlopOS has not got: `kill`
/// fans a signal out to the whole thread group, so `pthread_kill` can only
/// answer the `sig == 0` liveness probe and must refuse the rest.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_kill(thread: pthread_t, sig: c_int) -> c_int {
    if thread == 0 {
        return ESRCH.raw();
    }
    let tcb = thread as *mut Tcb;
    if core::ptr::read_volatile(&(*tcb).child_tid) == 0 && (*tcb).tid == 0 {
        return ESRCH.raw();
    }
    if sig == 0 {
        return 0;
    }
    ENOSYS.raw()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_yield() -> c_int {
    Sys::yield_now();
    0
}

/// Writes the CPUs the task may run on. The kernel's mask is one `unsigned
/// long`, so a larger `cpusetsize` is zero-filled above it rather than
/// refused.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sched_getaffinity(
    pid: c_int,
    cpusetsize: usize,
    cpuset: *mut c_void,
) -> c_int {
    if cpuset.is_null() || cpusetsize == 0 {
        errno_set(EINVAL.raw());
        return -1;
    }
    let mut mask = [0u8; 8];
    let written = match Sys::sched_getaffinity(pid, mask.len(), mask.as_mut_ptr()) {
        Ok(n) => n.min(mask.len()),
        Err(e) => {
            errno_set(e.raw());
            return -1;
        }
    };
    let out = cpuset as *mut u8;
    core::ptr::write_bytes(out, 0, cpusetsize);
    core::ptr::copy_nonoverlapping(mask.as_ptr(), out, written.min(cpusetsize));
    0
}
