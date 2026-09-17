//! pidfd syscall handler (`SYSCALL_PIDFD_OPEN`); the work lives in the
//! `slopos-pidfd` crate.

use slopos_abi::Errno;

define_syscall!(syscall_pidfd_open
    (ctx, pid: u32, flags: u32)
    cap(NoneRelation)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    // Linux's only flag here is `PIDFD_NONBLOCK`, which needs a descriptor
    // that can report `EAGAIN`; a pidfd is never readable, so there is nothing
    // for it to change.
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let fd = slopos_pidfd::pidfd_open(process_id, task_id, pid);
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});
