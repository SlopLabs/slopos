#![allow(non_camel_case_types)]

use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::errno::EINVAL;
use crate::pal::{Pal, Sys};

use super::futex::futex_wait_or_abort;

/// 4 bytes, matching the target's `libc` declaration and Linux's `int`. The
/// all-zero object is `PTHREAD_ONCE_INIT`, so a control a C++ library reaches
/// for from a static initialiser is usable without a call.
#[repr(C)]
pub struct pthread_once_t {
    state: AtomicU32,
}

const UNRUN: u32 = 0;
const RUNNING: u32 = 1;
const DONE: u32 = 2;
const RUNNING_WITH_WAITERS: u32 = 3;

/// A `routine` that does not return normally — one that throws, `longjmp`s or
/// calls `pthread_exit` — leaves the control in its running state forever and
/// every later caller blocks. glibc resets it from a cancellation cleanup
/// handler; slibc has no cancellation and no unwind landing pad here, so the
/// hazard is stated rather than handled.
///
/// # Safety
/// `control` is a writable `pthread_once_t` that every caller of this
/// `routine` shares.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_once(
    control: *mut pthread_once_t,
    routine: extern "C" fn(),
) -> c_int {
    if control.is_null() {
        return EINVAL.raw();
    }
    let state = &(*control).state;

    loop {
        // `Acquire` because a caller that sees `DONE` must also see every
        // write the routine made: this load is the only synchronisation
        // between them.
        match state.load(Ordering::Acquire) {
            DONE => return 0,
            UNRUN => {
                if state
                    .compare_exchange(UNRUN, RUNNING, Ordering::Acquire, Ordering::Relaxed)
                    .is_err()
                {
                    continue;
                }
                routine();
                if state.swap(DONE, Ordering::Release) == RUNNING_WITH_WAITERS {
                    let _ = Sys::futex_wake(state.as_ptr(), u32::MAX);
                }
                return 0;
            }
            observed => {
                if observed == RUNNING
                    && state
                        .compare_exchange(
                            RUNNING,
                            RUNNING_WITH_WAITERS,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_err()
                {
                    continue;
                }
                futex_wait_or_abort(state.as_ptr(), RUNNING_WITH_WAITERS);
            }
        }
    }
}
