//! `sigwait(3)`, over a signalfd: reading one takes a pending signal of its
//! mask off the caller's pending set, which is what `sigwait` is. The
//! signals in `set` are blocked by the caller, as POSIX requires, so none of
//! them is delivered to a handler while the read waits.

use core::ffi::c_int;

use crate::errno::{EINTR, EINVAL};
use crate::pal::{Pal, Sys};
use crate::types::sigset_t;
use slopos_abi::signal::SignalfdSiginfo;

/// Returns 0 with the signal in `*sig`, or an error number; never `EINTR`.
///
/// # Safety
/// `set` points to a `sigset_t`, `sig` to writable storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sigwait(set: *const sigset_t, sig: *mut c_int) -> c_int {
    if set.is_null() || sig.is_null() {
        return EINVAL.raw();
    }
    let mask = (*set).kernel_mask();
    if mask == 0 || (*set).has_unsupported_bits() {
        return EINVAL.raw();
    }
    let fd = match Sys::signalfd(mask, 0) {
        Ok(fd) => fd,
        Err(e) => return e.raw(),
    };
    let mut record = [0u8; SignalfdSiginfo::SERIALIZED_LEN];
    let result = loop {
        match Sys::read(fd, record.as_mut_ptr(), record.len()) {
            Ok(n) if n == record.len() => {
                let signo = u32::from_ne_bytes([record[0], record[1], record[2], record[3]]);
                *sig = signo as c_int;
                break 0;
            }
            Ok(_) => break EINVAL.raw(),
            Err(e) if e.raw() == EINTR.raw() => continue,
            Err(e) => break e.raw(),
        }
    };
    let _ = Sys::close(fd);
    result
}
