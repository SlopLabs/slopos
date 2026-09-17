//! signalfd4 syscall handler (`SYSCALL_SIGNALFD4`); the work lives in the
//! `slopos-signalfd` crate.

use slopos_abi::Errno;
use slopos_abi::signal::SigSet;
use slopos_mm::user_copy::copy_from_user;

use crate::syscall::args::UserPtr;

define_syscall!(syscall_signalfd4
    (ctx, fd: i32, mask: Option<UserPtr<SigSet>>, sizemask: u64, flags: u32)
    cap(NoneSelf)
    requires(let task_id: task_id, let process_id: process_id)
    -> Result<u64, Errno>
{
    // Re-arming an existing signalfd would mutate the mask of a descriptor
    // another task may be draining; the registry offers creation only.
    if fd != -1 {
        return Err(Errno::EINVAL);
    }
    if sizemask != core::mem::size_of::<SigSet>() as u64 {
        return Err(Errno::EINVAL);
    }
    // `SFD_CLOEXEC`/`SFD_NONBLOCK` would have to reach the descriptor install,
    // and a signalfd is installed with default fd flags and carries no
    // non-blocking state, so a caller asking for either is refused rather than
    // told yes and given neither.
    if flags != 0 {
        return Err(Errno::EINVAL);
    }
    let mask = mask.ok_or(Errno::EFAULT)?;
    let watched = copy_from_user(mask.inner()).map_err(|_| Errno::EFAULT)?;
    let created = slopos_signalfd::signalfd_create(process_id, task_id, watched);
    if created < 0 {
        return Err(Errno::from_raw(created).unwrap_or(Errno::EINVAL));
    }
    Ok(created as u64)
});
