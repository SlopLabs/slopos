#![allow(non_camel_case_types)]

use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, Ordering};

use crate::errno::{EAGAIN, EBUSY, EINVAL};
use crate::pal::{Pal, Sys};

/// Readers occupy the low 15 bits, waiting writers the next 15, and a writer
/// holding the lock sets bit 30. Every reachable value is positive, so the
/// word casts to the futex's `u32` unchanged.
const READER_ONE: i32 = 1;
const READERS_MASK: i32 = 0x7fff;
const WRITER_WAITING_ONE: i32 = 1 << 15;
const WRITERS_WAITING_MASK: i32 = 0x7fff << 15;
const WRITER_LOCKED: i32 = 1 << 30;

/// 56 bytes, matching the target's `libc`'s `[u64; 7]`, and valid all-zero: no
/// readers, no waiting writer, no writer, so `PTHREAD_RWLOCK_INITIALIZER`
/// works without `pthread_rwlock_init`.
#[repr(C)]
pub struct pthread_rwlock_t {
    /// Reader count, waiting-writer count and the writer's own bit, in one
    /// word rather than two.
    ///
    /// A reader has to observe "no writer holds it and none is waiting" and
    /// then park on the word the kernel re-compares before it sleeps. Split
    /// across two words that is a lost wakeup: a writer's entire
    /// acquire-and-release fits between the reader's two loads, the word the
    /// reader parks on is back to the value it read, the wake has already
    /// fired, and the reader sleeps with nothing left to wake it. Folded into
    /// one word, the same sequence changes the word, so the compare fails and
    /// the reader retries instead of sleeping.
    pub state: AtomicI32,
    pub _pad: u32,
    pub _reserved: [u64; 6],
}

/// 8 bytes, matching the target's `libc`'s `[u64; 1]`.
#[repr(C)]
pub struct pthread_rwlockattr_t {
    pub _opaque: u64,
}

pub const PTHREAD_RWLOCK_INITIALIZER: pthread_rwlock_t = pthread_rwlock_t {
    state: AtomicI32::new(0),
    _pad: 0,
    _reserved: [0; 6],
};

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_init(
    rwlock: *mut pthread_rwlock_t,
    _attr: *const pthread_rwlockattr_t,
) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    (*rwlock).state = AtomicI32::new(0);
    (*rwlock)._pad = 0;
    (*rwlock)._reserved = [0; 6];
    0
}

/// A reader is admitted only when no writer holds the lock and none is
/// waiting: a waiting writer closing the door on new readers is what keeps a
/// stream of readers from starving it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_rdlock(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    let state = &(*rwlock).state;
    loop {
        let s = state.load(Ordering::Acquire);
        if s & (WRITER_LOCKED | WRITERS_WAITING_MASK) != 0 {
            super::futex::futex_wait_or_abort(state.as_ptr() as *const u32, s as u32);
            continue;
        }
        if s & READERS_MASK == READERS_MASK {
            return EAGAIN.raw();
        }
        if state
            .compare_exchange_weak(s, s + READER_ONE, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return 0;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_tryrdlock(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    let state = &(*rwlock).state;
    let s = state.load(Ordering::Acquire);
    if s & (WRITER_LOCKED | WRITERS_WAITING_MASK) != 0 {
        return EBUSY.raw();
    }
    if s & READERS_MASK == READERS_MASK {
        return EAGAIN.raw();
    }
    if state
        .compare_exchange(s, s + READER_ONE, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        0
    } else {
        EBUSY.raw()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_wrlock(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    let state = &(*rwlock).state;

    loop {
        let s = state.load(Ordering::Relaxed);
        // 32767 threads already queued for one lock is not a state this
        // kernel can reach, and the alternative to refusing is overflowing
        // the count into the writer's own bit.
        if s & WRITERS_WAITING_MASK == WRITERS_WAITING_MASK {
            return EAGAIN.raw();
        }
        if state
            .compare_exchange_weak(
                s,
                s + WRITER_WAITING_ONE,
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            break;
        }
    }

    loop {
        let s = state.load(Ordering::Acquire);
        if s & (WRITER_LOCKED | READERS_MASK) != 0 {
            super::futex::futex_wait_or_abort(state.as_ptr() as *const u32, s as u32);
            continue;
        }
        // Withdraws this thread's own waiting count and takes the lock in one
        // step, from the exact word just observed, so no other writer's
        // waiting count is lost.
        if state
            .compare_exchange_weak(
                s,
                (s - WRITER_WAITING_ONE) | WRITER_LOCKED,
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
        {
            return 0;
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_trywrlock(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    // Never announces itself: a `trywrlock` that fails has to leave the lock
    // as it found it, and an announced-then-withdrawn writer would have turned
    // readers away in the meantime.
    if (*rwlock)
        .state
        .compare_exchange(0, WRITER_LOCKED, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
    {
        0
    } else {
        EBUSY.raw()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_unlock(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    let state = &(*rwlock).state;

    loop {
        let s = state.load(Ordering::Acquire);
        if s & WRITER_LOCKED != 0 {
            if state
                .compare_exchange_weak(s, s & !WRITER_LOCKED, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
        } else if s & READERS_MASK != 0 {
            if state
                .compare_exchange_weak(s, s - READER_ONE, Ordering::Release, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // A reader that was not the last one leaves the lock still held by
            // readers: nothing a wake could admit.
            if s & READERS_MASK != READER_ONE {
                return 0;
            }
        } else {
            // Nothing held, so nothing to release and nobody to wake. POSIX
            // leaves an unlock of an unheld rwlock undefined.
            return 0;
        }

        // Everyone, not one: readers and writers park on the same word, and
        // waking a single sleeper can wake a reader that has to park straight
        // back, leaving the writer the wake was meant for asleep.
        let _ = Sys::futex_wake(state.as_ptr() as *const u32, i32::MAX as u32);
        return 0;
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlock_destroy(rwlock: *mut pthread_rwlock_t) -> c_int {
    if rwlock.is_null() {
        return EINVAL.raw();
    }
    if (*rwlock).state.load(Ordering::Relaxed) != 0 {
        return EBUSY.raw();
    }
    (*rwlock).state = AtomicI32::new(0);
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlockattr_init(attr: *mut pthread_rwlockattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr)._opaque = 0;
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_rwlockattr_destroy(attr: *mut pthread_rwlockattr_t) -> c_int {
    if attr.is_null() {
        return EINVAL.raw();
    }
    (*attr)._opaque = 0;
    0
}
