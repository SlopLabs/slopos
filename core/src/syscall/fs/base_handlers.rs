//! `base_activate` and `base_monitor`: replacing the base while the system
//! runs, and hearing that it was.

use slopos_abi::Errno;
use slopos_fs::basefs::{ActivateError, StageError};
use slopos_mm::user_copy::copy_bytes_from_user;
use slopos_mm::user_ptr::UserBytes;

define_syscall!(syscall_base_activate
    (ctx, archive: u64, len: u64)
    cap(BaseUpdate)
    -> Result<u64, Errno>
{
    let budget = u64::from(slopos_mm::page_alloc::get_page_allocator_stats().total) * 4096 / 4;
    if len > budget {
        return Err(Errno::EFBIG);
    }
    let mut source = |offset: u64, buf: &mut [u8]| {
        let left = len.saturating_sub(offset).min(buf.len() as u64) as usize;
        if left == 0 {
            return Ok(0);
        }
        let at = archive.checked_add(offset).ok_or(Errno::EFAULT)?;
        let src = UserBytes::try_new(at, left).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(src, &mut buf[..left]).map_err(|_| Errno::EFAULT)
    };
    slopos_fs::basefs::activate(&mut source, budget)
        .map(u64::from)
        .map_err(activate_errno)
});

define_syscall!(syscall_base_monitor
    (ctx)
    cap(NoneSelf)
    requires(let process_id: process_id)
    -> Result<u64, Errno>
{
    let fd = slopos_fs::basefs::monitor::monitor_create(process_id);
    if fd < 0 {
        return Err(Errno::from_raw(fd).unwrap_or(Errno::EINVAL));
    }
    Ok(fd as u64)
});

fn activate_errno(e: ActivateError) -> Errno {
    match e {
        ActivateError::Stage(StageError::Read(e)) => e,
        ActivateError::Stage(StageError::NoMemory) | ActivateError::NoMemory => Errno::ENOMEM,
        ActivateError::Stage(_) | ActivateError::Missing | ActivateError::Mounts(_) => {
            Errno::EINVAL
        }
        ActivateError::Unserved(_) => Errno::ENOEXEC,
        ActivateError::Busy => Errno::EBUSY,
        ActivateError::Interrupted => Errno::EINTR,
    }
}
