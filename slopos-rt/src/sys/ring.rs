//! SlopRing syscall wrappers.

use slopos_abi::ring::RingParams;
use slopos_abi::syscall::{SYSCALL_RING_ENTER, SYSCALL_RING_REGISTER, SYSCALL_RING_SETUP};
use slopos_slibc::pal::raw::{syscall2, syscall4};

/// Create a ring. Returns the ring fd (`>= 0`) or a negated errno; `params` is
/// filled in with the ring geometry and the mapped user VA.
#[inline(always)]
pub fn ring_setup(entries: u32, params: &mut RingParams) -> i32 {
    unsafe { syscall2(SYSCALL_RING_SETUP, entries as u64, params as *mut _ as u64) as i32 }
}

/// Submit and/or harvest. Returns the submission count or a negated errno.
#[inline(always)]
pub fn ring_enter(ring_fd: i32, to_submit: u32, min_complete: u32, flags: u32) -> i32 {
    unsafe {
        syscall4(
            SYSCALL_RING_ENTER,
            ring_fd as u64,
            to_submit as u64,
            min_complete as u64,
            flags as u64,
        ) as i32
    }
}

/// Register or unregister fixed buffers or a provided-buffer ring with a ring,
/// as `op` says. Returns 0 on success or a negated errno.
#[inline(always)]
pub fn ring_register(ring_fd: i32, op: u32, arg: u64, nr_args: u32) -> i32 {
    unsafe {
        syscall4(
            SYSCALL_RING_REGISTER,
            ring_fd as u64,
            op as u64,
            arg,
            nr_args as u64,
        ) as i32
    }
}
