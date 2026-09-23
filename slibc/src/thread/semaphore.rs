//! POSIX unnamed semaphores over a futex word.

use core::ffi::{c_int, c_uint};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::errno::{EAGAIN, EINTR, EINVAL, ENOSYS, EOVERFLOW, ETIMEDOUT, Errno, errno_set};
use crate::pal::{Pal, Sys};
use crate::time::Timespec;

/// The value, then how many threads sleep on it, as in musl; the rest pads to
/// the 32 bytes the target's `libc` declares.
#[repr(C)]
pub struct sem_t {
    value: AtomicU32,
    waiters: AtomicU32,
    _reserved: [u32; 6],
}

const SEM_VALUE_MAX: u32 = i32::MAX as u32;

fn fail(e: Errno) -> c_int {
    errno_set(e.raw());
    -1
}

fn try_take(sem: &sem_t) -> bool {
    let mut value = sem.value.load(Ordering::Relaxed);
    while value > 0 {
        match sem.value.compare_exchange_weak(
            value,
            value - 1,
            Ordering::Acquire,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(seen) => value = seen,
        }
    }
    false
}

/// Private futexes only, so a semaphore another process maps would never be
/// woken; one that asks to be shared is refused rather than half-working.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_init(sem: *mut sem_t, pshared: c_int, value: c_uint) -> c_int {
    if sem.is_null() || value > SEM_VALUE_MAX {
        return fail(EINVAL);
    }
    if pshared != 0 {
        return fail(ENOSYS);
    }
    sem.write(sem_t {
        value: AtomicU32::new(value),
        waiters: AtomicU32::new(0),
        _reserved: [0; 6],
    });
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_destroy(sem: *mut sem_t) -> c_int {
    if sem.is_null() {
        return fail(EINVAL);
    }
    0
}

/// Async-signal-safe, as POSIX requires: one atomic and at most one wake.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_post(sem: *mut sem_t) -> c_int {
    let Some(sem) = sem.as_ref() else {
        return fail(EINVAL);
    };
    let mut value = sem.value.load(Ordering::Relaxed);
    loop {
        if value == SEM_VALUE_MAX {
            return fail(EOVERFLOW);
        }
        match sem.value.compare_exchange_weak(
            value,
            value + 1,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(seen) => value = seen,
        }
    }
    if sem.waiters.load(Ordering::SeqCst) > 0 {
        let _ = Sys::futex_wake(sem.value.as_ptr(), 1);
    }
    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_trywait(sem: *mut sem_t) -> c_int {
    match sem.as_ref() {
        None => fail(EINVAL),
        Some(sem) if try_take(sem) => 0,
        Some(_) => fail(EAGAIN),
    }
}

/// Sleeps while the value reads zero. A signal ends the wait with `EINTR`, as
/// POSIX has it, so a handler that posts can be told from a spurious wake.
unsafe fn wait(sem: *mut sem_t, deadline: Option<Timespec>) -> c_int {
    let Some(sem) = sem.as_ref() else {
        return fail(EINVAL);
    };
    loop {
        if try_take(sem) {
            return 0;
        }
        let timeout = match deadline {
            None => None,
            Some(deadline) => match remaining(deadline) {
                Some(left) => Some(left),
                None => return fail(ETIMEDOUT),
            },
        };
        sem.waiters.fetch_add(1, Ordering::SeqCst);
        let slept = Sys::futex_wait(
            sem.value.as_ptr(),
            0,
            timeout
                .as_ref()
                .map_or(core::ptr::null(), |t| t as *const _),
        );
        sem.waiters.fetch_sub(1, Ordering::SeqCst);
        match slept {
            Ok(()) => {}
            Err(e) if e == EAGAIN || e == ETIMEDOUT => {}
            Err(e) if e == EINTR => return fail(EINTR),
            Err(e) => return fail(e),
        }
    }
}

/// How long until the `CLOCK_REALTIME` deadline, or `None` once it passed.
fn remaining(deadline: Timespec) -> Option<Timespec> {
    let mut now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a live, writable `Timespec`.
    if unsafe { crate::time::clock_gettime(crate::time::CLOCK_REALTIME, &mut now) } != 0 {
        return None;
    }
    let mut sec = deadline.tv_sec - now.tv_sec;
    let mut nsec = deadline.tv_nsec - now.tv_nsec;
    if nsec < 0 {
        nsec += 1_000_000_000;
        sec -= 1;
    }
    (sec > 0 || (sec == 0 && nsec > 0)).then_some(Timespec {
        tv_sec: sec,
        tv_nsec: nsec,
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_wait(sem: *mut sem_t) -> c_int {
    wait(sem, None)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_timedwait(sem: *mut sem_t, abstime: *const Timespec) -> c_int {
    let Some(&deadline) = abstime.as_ref() else {
        return fail(EINVAL);
    };
    if deadline.tv_nsec < 0 || deadline.tv_nsec >= 1_000_000_000 {
        return fail(EINVAL);
    }
    wait(sem, Some(deadline))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sem_getvalue(sem: *mut sem_t, sval: *mut c_int) -> c_int {
    match (sem.as_ref(), sval.is_null()) {
        (Some(sem), false) => {
            sval.write(sem.value.load(Ordering::Relaxed) as c_int);
            0
        }
        _ => fail(EINVAL),
    }
}
