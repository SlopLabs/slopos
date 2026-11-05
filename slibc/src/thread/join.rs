use crate::errno::{EDEADLK, EINVAL};
use crate::pal::{Pal, Sys};

use super::pthread_t;
use super::tcb::Tcb;

/// # Safety
///
/// `thread` must be 0 or a `pthread_t` from `pthread_create` whose resources
/// have not been released by a prior join or detach; `retval` must be null or
/// valid for writing a pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_join(thread: pthread_t, retval: *mut *mut u8) -> i32 {
    if thread == 0 {
        return EINVAL.raw();
    }

    let tcb = thread as *mut Tcb;

    if (*tcb).detached {
        return EINVAL.raw();
    }

    if super::tls::tls_is_initialized() {
        let self_tcb = Tcb::current();
        if tcb == self_tcb {
            return EDEADLK.raw();
        }
    }

    loop {
        let tid_val = core::ptr::read_volatile(&(*tcb).child_tid);
        if tid_val == 0 {
            break;
        }
        super::futex::futex_wait_or_abort(
            &raw const (*tcb).child_tid as *const u32,
            tid_val as u32,
            crate::pal::FutexScope::Private,
        );
    }

    if !retval.is_null() {
        *retval = (*tcb).retval;
    }

    let stack_base = (*tcb).stack_base;
    let stack_size = (*tcb).stack_size;

    if !stack_base.is_null() && stack_size > 0 {
        let _ = Sys::munmap(stack_base, stack_size);
    }

    super::tls::free_thread_tls(tcb);
    0
}

/// # Safety
///
/// `thread` must be 0 or a `pthread_t` from `pthread_create` not already joined
/// or detached; its resources may be freed, so it must not be used afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_detach(thread: pthread_t) -> i32 {
    if thread == 0 {
        return EINVAL.raw();
    }

    let tcb = thread as *mut Tcb;
    (*tcb).detached = true;

    let tid_val = core::ptr::read_volatile(&(*tcb).child_tid);
    if tid_val == 0 {
        let stack_base = (*tcb).stack_base;
        let stack_size = (*tcb).stack_size;
        if !stack_base.is_null() && stack_size > 0 {
            let _ = Sys::munmap(stack_base, stack_size);
        }
        super::tls::free_thread_tls(tcb);
    }

    0
}

/// # Safety
///
/// Runs TLS and key destructors on the calling thread and never returns, so
/// nothing on the caller's stack may rely on its destructors running.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_exit(retval: *mut u8) -> ! {
    if super::tls::tls_is_initialized() {
        let tcb = Tcb::current();
        (*tcb).retval = retval;
        crate::cxa::run_thread_destructors();
        super::keys::run_key_destructors(tcb);
        crate::mem::release_thread_heap(tcb);
    }
    Sys::exit(0)
}

/// # Safety
///
/// None: it reads only the calling thread's TCB; `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_self() -> pthread_t {
    if super::tls::tls_is_initialized() {
        Tcb::current() as pthread_t
    } else {
        0
    }
}

/// # Safety
///
/// None: the arguments are compared as plain integers; `unsafe` only as a C ABI export.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pthread_equal(t1: pthread_t, t2: pthread_t) -> i32 {
    if t1 == t2 { 1 } else { 0 }
}
