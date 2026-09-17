//! `waitpid` shim the async `Child::wait` needs.

use slopos_abi::syscall::SYSCALL_WAIT4;
use slopos_slibc::pal::raw::syscall4;

/// Reap `task_id` and report its `$?`: the exit code it passed to `exit`, or
/// `128 + signum` for a death by signal. A negated errno when it cannot be
/// reaped, which is what `Child::wait` propagates.
///
/// `wait4(2)` answers the reaped pid and writes the status word, so the
/// code has to be decoded here rather than read off the return value. The
/// `rusage` pointer is null: the kernel keeps no per-task accounting.
#[inline(always)]
pub fn waitpid(task_id: u32) -> i32 {
    let mut status = 0i32;
    let reaped = unsafe {
        syscall4(
            SYSCALL_WAIT4,
            task_id as u64,
            &mut status as *mut i32 as u64,
            0,
            0,
        ) as i64
    };
    if reaped < 0 {
        return reaped as i32;
    }
    if reaped == 0 {
        // Only `WNOHANG` answers 0, and this call never sets it.
        return -1;
    }
    let raw = status as u32;
    let signum = raw & 0x7f;
    if signum == 0 {
        ((raw >> 8) & 0xff) as i32
    } else if signum == 0x7f {
        -1
    } else {
        128 + signum as i32
    }
}
