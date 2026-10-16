//! `base_activate` and `base_monitor`: replacing the base while the system
//! runs, and hearing that it was. `ext_activate` and `ext_deactivate`:
//! showing an extension beside it, and taking one away.

use slopos_abi::Errno;
use slopos_abi::extension::{DIGEST_LEN, NAME_MAX};
use slopos_fs::VfsError;
use slopos_fs::basefs::{ActivateError, Source, StageError};
use slopos_mm::user_copy::{copy_bytes_from_user, copy_from_user};
use slopos_mm::user_ptr::{UserBytes, UserPtr};

use crate::syscall::args::UserCStr;

fn usable_bytes() -> u64 {
    u64::from(slopos_mm::page_alloc::get_page_allocator_stats().total) * 4096
}

fn from_user(archive: u64, len: u64) -> impl FnMut(u64, &mut [u8]) -> Result<usize, Errno> {
    move |offset: u64, buf: &mut [u8]| {
        let left = len.saturating_sub(offset).min(buf.len() as u64) as usize;
        if left == 0 {
            return Ok(0);
        }
        let at = archive.checked_add(offset).ok_or(Errno::EFAULT)?;
        let src = UserBytes::try_new(at, left).map_err(|_| Errno::EFAULT)?;
        copy_bytes_from_user(src, &mut buf[..left]).map_err(|_| Errno::EFAULT)
    }
}

define_syscall!(syscall_base_activate
    (ctx, archive: u64, len: u64)
    cap(BaseUpdate)
    -> Result<u64, Errno>
{
    let budget = usable_bytes() / 4;
    if len > budget {
        return Err(Errno::EFBIG);
    }
    slopos_fs::basefs::activate(&mut from_user(archive, len), budget)
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

define_syscall!(syscall_ext_activate
    (ctx, name: UserCStr<{ NAME_MAX + 2 }>, archive: u64, len: u64, digest: u64)
    cap(ExtensionUpdate)
    -> Result<u64, Errno>
{
    if slopos_ostd::boot_flags::has_flag(slopos_abi::syscall::BOOT_FLAG_EXTENSIONS_OFF) {
        return Err(Errno::EPERM);
    }
    let budget = slopos_fs::basefs::extension::budget(usable_bytes());
    if len > budget {
        return Err(Errno::EFBIG);
    }
    let digest = UserPtr::<[u8; DIGEST_LEN]>::try_new(digest).map_err(|_| Errno::EFAULT)?;
    let digest = copy_from_user(digest).map_err(|_| Errno::EFAULT)?;
    let source: &mut Source<'_> = &mut from_user(archive, len);
    slopos_fs::basefs::extension::activate(name.as_bytes(), digest, source, budget)
        .map(u64::from)
        .map_err(ext_errno)
});

define_syscall!(syscall_ext_deactivate
    (ctx, name: UserCStr<{ NAME_MAX + 2 }>)
    cap(ExtensionUpdate)
    -> Result<(), Errno>
{
    slopos_fs::basefs::extension::deactivate(name.as_bytes()).map_err(ext_errno)
});

fn activate_errno(e: ActivateError) -> Errno {
    match e {
        ActivateError::Stage(StageError::Read(e)) => e,
        ActivateError::Stage(StageError::NoMemory) | ActivateError::NoMemory => Errno::ENOMEM,
        ActivateError::Stage(_)
        | ActivateError::Missing
        | ActivateError::Mounts(_)
        | ActivateError::Name => Errno::EINVAL,
        ActivateError::Unserved(_) => Errno::ENOEXEC,
        ActivateError::Busy => Errno::EBUSY,
        ActivateError::Interrupted => Errno::EINTR,
    }
}

fn ext_errno(e: ActivateError) -> Errno {
    match e {
        ActivateError::Mounts(VfsError::NotFound) => Errno::ENOENT,
        ActivateError::Mounts(VfsError::Busy | VfsError::NoSpace) => Errno::EBUSY,
        ActivateError::Mounts(e) => e.to_errno(),
        e => activate_errno(e),
    }
}
