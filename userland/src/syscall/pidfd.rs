//! pidfd syscall wrapper.
//!
//! The returned fd becomes `POLLIN`-ready once the target child exits —
//! pollable via [`fs::poll`](super::fs::poll) or SlopRing `OP_POLL_ADD`, then
//! reaped with `waitpid`.

use super::raw::syscall2;
use slopos_abi::syscall::SYSCALL_PIDFD_OPEN;

/// Open a process-exit fd for child task `pid`. Returns the fd (`>= 0`) or
/// a negated errno (`-ESRCH` if no such task, `-EPERM` if not a child).
#[inline(always)]
pub fn pidfd_open(pid: u32) -> i32 {
    unsafe { syscall2(SYSCALL_PIDFD_OPEN, pid as u64, 0) as i32 }
}

/// [`pidfd_open`], owned; `None` on any error.
pub fn pidfd_open_owned(pid: u32) -> Option<super::OwnedFd> {
    let fd = pidfd_open(pid);
    // SAFETY: a non-negative return is a descriptor the kernel just opened.
    (fd >= 0).then(|| unsafe { super::OwnedFd::from_raw(fd) })
}
