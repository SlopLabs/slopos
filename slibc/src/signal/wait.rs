//! `sigwait(3)`, `sigwaitinfo(2)` and `sigtimedwait(2)`, all over
//! `rt_sigtimedwait`, which takes a pending signal of the set off the caller's
//! pending signals, its own or its process's, and waits for one when none is.

use core::ffi::c_int;
use core::ptr;

use super::SIGSET_SIZE;
use crate::errno::{EINTR, EINVAL, errno_set};
use crate::pal::{Pal, Sys};
use crate::time::Timespec;
use crate::types::sigset_t;
use slopos_abi::signal::UserSiginfo;

/// Returns the signal number, or -1 with `errno` set: `EAGAIN` once `timeout`
/// passes, `EINTR` when a caught signal outside `set` is delivered first.
///
/// # Safety
/// `set` points to a `sigset_t`; `info`, when not null, to a writable
/// `siginfo_t`; `timeout`, when not null, to a `timespec`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigtimedwait(
    set: *const sigset_t,
    info: *mut UserSiginfo,
    timeout: *const Timespec,
) -> c_int {
    if set.is_null() || (*set).has_unsupported_bits() {
        errno_set(EINVAL.raw());
        return -1;
    }
    let mask = (*set).kernel_mask();
    Sys::rt_sigtimedwait(&raw const mask, info, timeout, SIGSET_SIZE).unwrap_or(-1)
}

/// [`sigtimedwait`] with no timeout.
///
/// # Safety
/// As [`sigtimedwait`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigwaitinfo(set: *const sigset_t, info: *mut UserSiginfo) -> c_int {
    sigtimedwait(set, info, ptr::null())
}

/// Returns 0 with the signal in `*sig`, or an error number; never `EINTR`.
///
/// # Safety
/// `set` points to a `sigset_t`, `sig` to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigwait(set: *const sigset_t, sig: *mut c_int) -> c_int {
    if set.is_null() || sig.is_null() || (*set).has_unsupported_bits() {
        return EINVAL.raw();
    }
    let mask = (*set).kernel_mask();
    loop {
        match Sys::rt_sigtimedwait(&raw const mask, ptr::null_mut(), ptr::null(), SIGSET_SIZE) {
            Ok(signo) => {
                *sig = signo;
                return 0;
            }
            Err(e) if e.raw() == EINTR.raw() => continue,
            Err(e) => return e.raw(),
        }
    }
}
