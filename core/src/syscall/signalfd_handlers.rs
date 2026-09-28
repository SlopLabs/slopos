//! signalfd4 syscall handler (`SYSCALL_SIGNALFD4`); the work lives in the
//! `slopos-signalfd` crate.

use slopos_abi::Errno;
use slopos_abi::signal::SigSet;
use slopos_abi::syscall::{SFD_CLOEXEC, SFD_NONBLOCK};
use slopos_mm::user_copy::copy_from_user;

use crate::syscall::args::UserPtr;

define_syscall!(syscall_signalfd4
    (ctx, fd: i32, mask: Option<UserPtr<SigSet>>, sizemask: u64, flags: u32)
    cap(NoneSelf)
    requires(let process_id: process_id)
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
    if flags & !(SFD_NONBLOCK | SFD_CLOEXEC) != 0 {
        return Err(Errno::EINVAL);
    }
    let mask = mask.ok_or(Errno::EFAULT)?;
    let watched = copy_from_user(mask.inner()).map_err(|_| Errno::EFAULT)?;
    let created = slopos_signalfd::signalfd_create(
        process_id,
        watched,
        flags & SFD_NONBLOCK != 0,
        flags & SFD_CLOEXEC != 0,
    );
    if created < 0 {
        return Err(Errno::from_raw(created).unwrap_or(Errno::EINVAL));
    }
    Ok(created as u64)
});
