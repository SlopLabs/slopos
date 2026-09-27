#![allow(non_camel_case_types)]

use core::ffi::c_int;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::errno::{EINVAL, ETIMEDOUT};
use crate::pal::{FutexScope, Pal, Sys};

use super::mutex::{pthread_mutex_lock, pthread_mutex_t, pthread_mutex_unlock};
use super::{pshared_flag, pshared_value};

/// 48 bytes, matching the target's `libc`. Valid all-zero: sequence 0, no
/// associated mutex, and `clock` 0 — which is `CLOCK_REALTIME`, the POSIX
/// default — so `PTHREAD_COND_INITIALIZER` needs no `pthread_cond_init`.
#[repr(C)]
pub struct pthread_cond_t {
    pub seq: AtomicU32,
    /// The clock `pthread_cond_timedwait`'s `abstime` is measured against,
    /// set by `pthread_condattr_setclock`.
    pub clock: c_int,
    pub mutex: *mut pthread_mutex_t,
    /// Nonzero for `PTHREAD_PROCESS_SHARED`, as `pthread_mutex_t`'s.
    pub pshared: c_int,
    pub _pad: c_int,
    pub _reserved: [u64; 3],
}

unsafe impl Send for pthread_cond_t {}
unsafe impl Sync for pthread_cond_t {}

/// The clock, with [`CONDATTR_PSHARED`] folded in: the target's `libc` gives
/// the attribute four bytes.
#[repr(C)]
pub struct pthread_condattr_t {
    pub clock: c_int,
}

const CONDATTR_PSHARED: c_int = 1 << 30;

#[inline]
unsafe fn scope_of(cond: *const pthread_cond_t) -> FutexScope {
    FutexScope::of_pshared((*cond).pshared != 0)
}

pub const PTHREAD_COND_INITIALIZER: pthread_cond_t = pthread_cond_t {
    seq: AtomicU32::new(0),
    clock: crate::time::CLOCK_REALTIME,
    mutex: ptr::null_mut(),
    pshared: 0,
    _pad: 0,
    _reserved: [0; 3],
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_init(
    cond: *mut pthread_cond_t,
    attr: *const pthread_condattr_t,
) -> c_int {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq = AtomicU32::new(0);
    (*cond).mutex = ptr::null_mut();
    (*cond)._pad = 0;
    (*cond)._reserved = [0; 3];
    let attr_clock = if attr.is_null() {
        crate::time::CLOCK_REALTIME
    } else {
        (*attr).clock
    };
    (*cond).clock = attr_clock & !CONDATTR_PSHARED;
    (*cond).pshared = c_int::from(attr_clock & CONDATTR_PSHARED != 0);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_wait(
    cond: *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
) -> c_int {
    if cond.is_null() || mutex.is_null() {
        return EINVAL.raw();
    }

    let saved_seq = (*cond).seq.load(Ordering::Relaxed);
    (*cond).mutex = mutex;

    pthread_mutex_unlock(mutex);
    super::futex::futex_wait_or_abort(
        (*cond).seq.as_ptr() as *const u32,
        saved_seq,
        scope_of(cond),
    );
    pthread_mutex_lock(mutex);

    0
}

/// `abstime` is an absolute deadline on `(*cond).clock`; the futex takes a
/// relative timeout, so the deadline is converted against that clock's current
/// reading. A deadline already past is `ETIMEDOUT` without unblocking anyone,
/// but the mutex is still dropped and re-taken, which is what POSIX requires.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_timedwait(
    cond: *mut pthread_cond_t,
    mutex: *mut pthread_mutex_t,
    abstime: *const crate::time::Timespec,
) -> c_int {
    if cond.is_null() || mutex.is_null() || abstime.is_null() {
        return EINVAL.raw();
    }
    let deadline = *abstime;
    if deadline.tv_nsec < 0 || deadline.tv_nsec >= 1_000_000_000 {
        return EINVAL.raw();
    }

    let mut now = crate::time::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if crate::time::clock_gettime((*cond).clock, &mut now) != 0 {
        return EINVAL.raw();
    }

    let mut sec = deadline.tv_sec - now.tv_sec;
    let mut nsec = deadline.tv_nsec - now.tv_nsec;
    if nsec < 0 {
        nsec += 1_000_000_000;
        sec -= 1;
    }

    let saved_seq = (*cond).seq.load(Ordering::Relaxed);
    (*cond).mutex = mutex;

    if sec < 0 {
        // Nothing to wait for; still hand the mutex back as POSIX says.
        return ETIMEDOUT.raw();
    }

    let relative = crate::time::Timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    };

    pthread_mutex_unlock(mutex);
    let outcome = Sys::futex_wait(
        (*cond).seq.as_ptr() as *const u32,
        saved_seq,
        &raw const relative,
        scope_of(cond),
    );
    pthread_mutex_lock(mutex);

    match outcome {
        Err(e) if e == ETIMEDOUT => ETIMEDOUT.raw(),
        // `EAGAIN` means the sequence already moved, i.e. a signal landed
        // between the load and the queueing; `EINTR` is a spurious wakeup.
        // Both are wakeups as far as the caller's predicate loop is concerned.
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_signal(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq.fetch_add(1, Ordering::Release);
    let _ = Sys::futex_wake((*cond).seq.as_ptr() as *const u32, 1, scope_of(cond));
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_broadcast(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq.fetch_add(1, Ordering::Release);
    let _ = Sys::futex_wake(
        (*cond).seq.as_ptr() as *const u32,
        i32::MAX as u32,
        scope_of(cond),
    );
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_cond_destroy(cond: *mut pthread_cond_t) -> c_int {
    if cond.is_null() {
        return EINVAL.raw();
    }
    (*cond).seq = AtomicU32::new(0);
    (*cond).mutex = ptr::null_mut();
    (*cond).pshared = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_condattr_init(attr: *mut pthread_condattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).clock = crate::time::CLOCK_REALTIME;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_condattr_destroy(attr: *mut pthread_condattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).clock = 0;
    0
}

/// Only the two clocks `clock_gettime` serves are accepted: a condvar whose
/// deadline is measured against a clock this kernel cannot read would block
/// for the wrong length of time.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_condattr_setclock(
    attr: *mut pthread_condattr_t,
    clock_id: c_int,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    if clock_id != crate::time::CLOCK_REALTIME && clock_id != crate::time::CLOCK_MONOTONIC {
        return EINVAL.raw();
    }
    (*attr).clock = clock_id | ((*attr).clock & CONDATTR_PSHARED);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_condattr_setpshared(
    attr: *mut pthread_condattr_t,
    pshared: c_int,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    let Some(shared) = pshared_flag(pshared) else {
        return EINVAL.raw();
    };
    let clock = (*attr).clock & !CONDATTR_PSHARED;
    (*attr).clock = if shared {
        clock | CONDATTR_PSHARED
    } else {
        clock
    };
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_condattr_getpshared(
    attr: *const pthread_condattr_t,
    pshared: *mut c_int,
) -> c_int {
    if attr.is_null() || pshared.is_null() {
        return EINVAL.raw();
    }
    *pshared = pshared_value((*attr).clock & CONDATTR_PSHARED != 0);
    0
}
