#![allow(non_camel_case_types)]

use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, Ordering};

use crate::errno::{EAGAIN, EBUSY, EDEADLK, EINVAL, EPERM};
use crate::pal::{Pal, Sys};

pub const PTHREAD_MUTEX_NORMAL: c_int = 0;
pub const PTHREAD_MUTEX_RECURSIVE: c_int = 1;
pub const PTHREAD_MUTEX_ERRORCHECK: c_int = 2;

/// 40 bytes, because that is what the target's `libc` declares
/// `pthread_mutex_t` to be. The futex word is first and the whole object is
/// valid all-zero, which is what makes `PTHREAD_MUTEX_INITIALIZER` — a zeroed
/// object — a usable, unlocked, `PTHREAD_MUTEX_NORMAL` mutex without any
/// `pthread_mutex_init` call.
#[repr(C)]
pub struct pthread_mutex_t {
    /// 0 = unlocked, 1 = locked, 2 = locked with waiters.
    pub state: AtomicI32,
    /// The owning thread's tid while locked, 0 otherwise, maintained only by
    /// the two owner-tracking kinds.
    ///
    /// Atomic because a thread reads it while another thread may be writing
    /// it. `Relaxed` is enough because the only question asked of it is "is
    /// this my own tid", and the answer cannot go stale in the direction that
    /// matters: the only thread that ever writes a given tid here, or clears
    /// it, is the thread that tid belongs to, and a thread never reads back a
    /// value it has already overwritten.
    pub owner_tid: AtomicI32,
    /// `PTHREAD_MUTEX_RECURSIVE`'s acquisition depth. Owner-only: every read
    /// and write happens with the lock held.
    pub count: i32,
    pub kind: c_int,
    pub _reserved: [u64; 3],
}

#[repr(C)]
pub struct pthread_mutexattr_t {
    pub kind: c_int,
}

pub const PTHREAD_MUTEX_INITIALIZER: pthread_mutex_t = pthread_mutex_t {
    state: AtomicI32::new(0),
    owner_tid: AtomicI32::new(0),
    count: 0,
    kind: PTHREAD_MUTEX_NORMAL,
    _reserved: [0; 3],
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_init(
    mutex: *mut pthread_mutex_t,
    attr: *const pthread_mutexattr_t,
) -> c_int {
    if mutex.is_null() {
        return EINVAL.raw();
    }
    (*mutex).state = AtomicI32::new(0);
    (*mutex).owner_tid = AtomicI32::new(0);
    (*mutex).count = 0;
    (*mutex)._reserved = [0; 3];
    (*mutex).kind = if attr.is_null() {
        PTHREAD_MUTEX_NORMAL
    } else {
        (*attr).kind
    };
    0
}

/// The calling thread's tid. `CLONE_PARENT_SETTID` and TLS setup both fill the
/// TCB's copy, so the answer is normally a TLS read; a mutex taken before
/// `fs_base` exists has to ask the kernel for it.
#[inline]
fn self_tid() -> i32 {
    if super::tls::tls_is_initialized() {
        // SAFETY: `tls_is_initialized` reports that `fs_base` holds a live TCB.
        unsafe { (*super::tcb::Tcb::current()).tid }
    } else {
        Sys::gettid()
    }
}

/// True when the calling thread already holds `mutex`.
///
/// The `state` test is what keeps a zero `tid` from reading as ownership of an
/// unlocked mutex, and neither load can mislead: only the owner releases
/// `state`, and only the caller writes or clears its own tid.
///
/// # Safety
/// `mutex` must point to a live, initialised `pthread_mutex_t`.
#[inline]
unsafe fn held_by_caller(mutex: *const pthread_mutex_t, tid: i32) -> bool {
    (*mutex).state.load(Ordering::Relaxed) != 0 && (*mutex).owner_tid.load(Ordering::Relaxed) == tid
}

/// Futex-based lock: 0=unlocked, 1=locked, 2=locked+waiters.
#[inline]
fn lock_state(state: &AtomicI32) {
    if state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        return;
    }

    loop {
        if state.swap(2, Ordering::Acquire) == 0 {
            return;
        }
        super::futex::futex_wait_or_abort(state.as_ptr() as *const u32, 2);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_lock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    // `PTHREAD_MUTEX_NORMAL` is the only kind Rust's `std` ever creates, and
    // it needs no owner: the futex word alone is the lock.
    if (*mutex).kind == PTHREAD_MUTEX_NORMAL {
        lock_state(&(*mutex).state);
        return 0;
    }

    let tid = self_tid();
    if held_by_caller(mutex, tid) {
        if (*mutex).kind == PTHREAD_MUTEX_ERRORCHECK {
            return EDEADLK.raw();
        }
        if (*mutex).count == c_int::MAX {
            return EAGAIN.raw();
        }
        (*mutex).count += 1;
        return 0;
    }

    lock_state(&(*mutex).state);
    (*mutex).owner_tid.store(tid, Ordering::Relaxed);
    (*mutex).count = 1;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_trylock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    let tracks_owner = (*mutex).kind != PTHREAD_MUTEX_NORMAL;
    let tid = if tracks_owner { self_tid() } else { 0 };
    if tracks_owner && held_by_caller(mutex, tid) {
        // POSIX gives `trylock` no way to report a self-deadlock, so an
        // errorcheck mutex the caller already holds is simply busy.
        if (*mutex).kind == PTHREAD_MUTEX_ERRORCHECK {
            return EBUSY.raw();
        }
        if (*mutex).count == c_int::MAX {
            return EAGAIN.raw();
        }
        (*mutex).count += 1;
        return 0;
    }

    if (*mutex)
        .state
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return EBUSY.raw();
    }
    if tracks_owner {
        (*mutex).owner_tid.store(tid, Ordering::Relaxed);
        (*mutex).count = 1;
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_unlock(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() {
        return EINVAL.raw();
    }

    if (*mutex).kind != PTHREAD_MUTEX_NORMAL {
        let tid = self_tid();
        if !held_by_caller(mutex, tid) {
            return EPERM.raw();
        }
        if (*mutex).count > 1 {
            (*mutex).count -= 1;
            return 0;
        }
        (*mutex).count = 0;
        // Cleared before the release below, which is what publishes it: the
        // next owner acquires that release, so its own store to `owner_tid`
        // cannot be overwritten by this one.
        (*mutex).owner_tid.store(0, Ordering::Relaxed);
    }

    let state = &(*mutex).state;
    if state.swap(0, Ordering::Release) == 2 {
        let _ = Sys::futex_wake(state.as_ptr() as *const u32, 1);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutex_destroy(mutex: *mut pthread_mutex_t) -> c_int {
    if mutex.is_null() {
        return EINVAL.raw();
    }
    if (*mutex).state.load(Ordering::Relaxed) != 0 {
        return EBUSY.raw();
    }
    (*mutex).state = AtomicI32::new(0);
    (*mutex).owner_tid = AtomicI32::new(0);
    (*mutex).kind = 0;
    (*mutex).count = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_init(attr: *mut pthread_mutexattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).kind = PTHREAD_MUTEX_NORMAL;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_settype(
    attr: *mut pthread_mutexattr_t,
    kind: c_int,
) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    // The three kinds `lock`/`unlock` implement. A stored fourth would be a
    // mode the mutex cannot honour, and the caller would find out as a hang.
    if kind != PTHREAD_MUTEX_NORMAL
        && kind != PTHREAD_MUTEX_RECURSIVE
        && kind != PTHREAD_MUTEX_ERRORCHECK
    {
        return EINVAL.raw();
    }
    (*attr).kind = kind;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_mutexattr_destroy(attr: *mut pthread_mutexattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr).kind = 0;
    0
}
