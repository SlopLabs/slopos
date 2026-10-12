//! The set*id and get*id calls, at their Linux numbers and with Linux's rules
//! for a caller without `SetId`.

use slopos_abi::Errno;
use slopos_mm::user_copy::copy_to_user;
use slopos_ostd::authority::Capability;
use slopos_ostd::process::Cred;
use slopos_ostd::process::cred::NGROUPS_MAX;
use slopos_ostd::util::byte_view::{pod_slice_as_bytes, pod_slice_as_bytes_mut};

use crate::syscall::args::UserPtr;
use crate::syscall::common::{syscall_bounded_from_user, syscall_copy_to_user_bounded};
use crate::syscall::context::SyscallContext;

fn change(
    ctx: &SyscallContext<'_>,
    f: impl FnOnce(&Cred, bool) -> Result<Cred, Errno>,
) -> Result<(), Errno> {
    let set_id = ctx.holds(Capability::SetId);
    ctx.task()
        .process()
        .ok_or(Errno::EPERM)?
        .change_cred(|cred| f(cred, set_id))
}

fn put_ids(ids: [u32; 3], out: [UserPtr<u32>; 3]) -> Result<(), Errno> {
    for (id, ptr) in ids.into_iter().zip(out) {
        copy_to_user(ptr.inner(), &id).map_err(|_| Errno::EFAULT)?;
    }
    Ok(())
}

define_syscall!(syscall_getuid (ctx) cap(NoneSelf) -> u32 { ctx.cred().ruid });
define_syscall!(syscall_geteuid (ctx) cap(NoneSelf) -> u32 { ctx.cred().euid });
define_syscall!(syscall_getgid (ctx) cap(NoneSelf) -> u32 { ctx.cred().rgid });
define_syscall!(syscall_getegid (ctx) cap(NoneSelf) -> u32 { ctx.cred().egid });

define_syscall!(syscall_setuid (ctx, uid: u32) cap(NoneSelf) -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_uid(uid, set_id))
});

define_syscall!(syscall_setgid (ctx, gid: u32) cap(NoneSelf) -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_gid(gid, set_id))
});

define_syscall!(syscall_setreuid (ctx, ruid: u32, euid: u32) cap(NoneSelf)
    -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_reuid(ruid, euid, set_id))
});

define_syscall!(syscall_setregid (ctx, rgid: u32, egid: u32) cap(NoneSelf)
    -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_regid(rgid, egid, set_id))
});

define_syscall!(syscall_setresuid (ctx, ruid: u32, euid: u32, suid: u32) cap(NoneSelf)
    -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_resuid([ruid, euid, suid], set_id))
});

define_syscall!(syscall_setresgid (ctx, rgid: u32, egid: u32, sgid: u32) cap(NoneSelf)
    -> Result<(), Errno> {
    change(ctx, |cred, set_id| cred.set_resgid([rgid, egid, sgid], set_id))
});

define_syscall!(syscall_getresuid
    (ctx, ruid: UserPtr<u32>, euid: UserPtr<u32>, suid: UserPtr<u32>) cap(NoneSelf)
    -> Result<(), Errno> {
    let cred = ctx.cred();
    put_ids([cred.ruid, cred.euid, cred.suid], [ruid, euid, suid])
});

define_syscall!(syscall_getresgid
    (ctx, rgid: UserPtr<u32>, egid: UserPtr<u32>, sgid: UserPtr<u32>) cap(NoneSelf)
    -> Result<(), Errno> {
    let cred = ctx.cred();
    put_ids([cred.rgid, cred.egid, cred.sgid], [rgid, egid, sgid])
});

define_syscall!(syscall_getgroups (ctx, size: i32, list: u64) cap(NoneSelf)
    -> Result<u64, Errno> {
    let cred = ctx.cred();
    let groups = cred.groups();
    let size = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if size == 0 {
        return Ok(groups.len() as u64);
    }
    if size < groups.len() {
        return Err(Errno::EINVAL);
    }
    syscall_copy_to_user_bounded(list, pod_slice_as_bytes(groups)).map_err(|_| Errno::EFAULT)?;
    Ok(groups.len() as u64)
});

define_syscall!(syscall_setgroups (ctx, size: u64, list: u64) cap(NoneSelf)
    -> Result<(), Errno> {
    let len = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if len > NGROUPS_MAX {
        return Err(Errno::EINVAL);
    }
    let mut groups = [0u32; NGROUPS_MAX];
    let bytes = pod_slice_as_bytes_mut(&mut groups[..len]);
    if !bytes.is_empty() {
        let want = bytes.len();
        let got = syscall_bounded_from_user(bytes, list, want as u64, want)
            .map_err(|_| Errno::EFAULT)?;
        if got != want {
            return Err(Errno::EFAULT);
        }
    }
    change(ctx, |cred, set_id| cred.set_groups(&groups[..len], set_id))
});
