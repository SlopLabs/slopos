//! The `*at(2)` family, plus the two plain forms defined in terms of it.

use slopos_abi::Errno;
use slopos_abi::fs::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, UTIME_NOW,
    UTIME_OMIT, UserFsStat,
};
use slopos_abi::syscall::types::Timespec;

use slopos_fs::fileio::{
    FdTable, file_access_at, file_chmod_at, file_fstat_fd, file_link_at, file_open_at,
    file_rmdir_at, file_set_times_fd, file_stat_at, file_symlink_at, file_unlink_at,
    file_utimens_at,
};

use slopos_mm::user_copy::{copy_from_user, copy_to_user};

use crate::syscall::args::{UserBytes, UserPath, UserPtr};
use crate::syscall::common::{USER_PATH_MAX, errno_from_neg};
use crate::syscall::fs::dirfd::{
    open_resolve_flags, reject_non_directory, resolve_flags_from, with_cwd_base, with_dir_base,
};
use crate::syscall::fs::path_handlers::readlink_at_into_user;

/// Unknown flags are refused, not ignored: a caller that asked for
/// `AT_SYMLINK_NOFOLLOW` on a call that followed would act on the wrong file.
fn reject_unknown(flags: u32, allowed: u32) -> Result<(), Errno> {
    if flags & !allowed != 0 {
        Err(Errno::EINVAL)
    } else {
        Ok(())
    }
}

define_syscall!(syscall_openat
    (ctx, dirfd: i32, path: UserPath, flags: u32, mode: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let create_mode = (mode & 0o7777) as u16;
    let resolve = open_resolve_flags(flags, path.as_bytes());
    let fd = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        file_open_at(pid, path.as_bytes(), cwd, flags, resolve, Some(create_mode))
    })?;
    if fd < 0 {
        Err(errno_from_neg(fd))
    } else {
        Ok(fd as u64)
    }
});

define_syscall!(syscall_mkdirat
    (ctx, dirfd: i32, path: UserPath, mode: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    // The mode goes on the inode `create` returned: re-resolving the name to
    // chmod it can land on a replacement, or on a symlink's target.
    with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        slopos_fs::vfs::vfs_mkdir_at(path.as_bytes(), cwd, Some((mode & 0o7777) as u16))
            .map_err(|e| e.to_errno())
    })??;
    Ok(())
});

define_syscall!(syscall_unlinkat
    (ctx, dirfd: i32, path: UserPath, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_REMOVEDIR)?;
    let remove_dir = flags & AT_REMOVEDIR != 0;
    let rc = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        if remove_dir {
            file_rmdir_at(path.as_bytes(), cwd)
        } else {
            if let Err(e) = reject_non_directory(path.as_bytes(), cwd) {
                return e.raw();
            }
            file_unlink_at(path.as_bytes(), cwd)
        }
    })?;
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_renameat
    (ctx, olddirfd: i32, old_path: UserPath, newdirfd: i32, new_path: UserPath)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    // `with_fd_dir_path` drops the descriptor-table lock before it calls in,
    // so the nested base lookup takes no lock the outer one holds.
    with_dir_base(ctx, pid, olddirfd, old_path.as_bytes(), |old_cwd| {
        with_dir_base(ctx, pid, newdirfd, new_path.as_bytes(), |new_cwd| {
            slopos_fs::vfs::vfs_rename_at(old_path.as_bytes(), old_cwd, new_path.as_bytes(), new_cwd)
                .map_err(|e| e.to_errno())
        })
    })??
});

define_syscall!(syscall_fstatat
    (ctx, dirfd: i32, path: UserPath, out: UserPtr<UserFsStat>, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)?;
    let mut stat = UserFsStat::default();
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(Errno::ENOENT);
        }
        let rc = file_fstat_fd(pid, dirfd, &mut stat);
        if rc != 0 {
            return Err(errno_from_neg(rc));
        }
    } else {
        let resolve = resolve_flags_from(flags, path.as_bytes());
        let rc = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
            file_stat_at(path.as_bytes(), cwd, resolve, &mut stat)
        })?;
        if rc != 0 {
            return Err(errno_from_neg(rc));
        }
    }
    copy_to_user(out.inner(), &stat).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_readlinkat
    (ctx, dirfd: i32, path: UserPath, buf: UserBytes)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let len = buf.len().min(USER_PATH_MAX);
    let base = buf.base_u64();
    let n = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        readlink_at_into_user(path.as_bytes(), cwd, base, len)
    })??;
    Ok(n as u64)
});

define_syscall!(syscall_symlinkat
    (ctx, target: UserPath, newdirfd: i32, link_path: UserPath)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = with_dir_base(ctx, pid, newdirfd, link_path.as_bytes(), |cwd| {
        file_symlink_at(target.as_bytes(), link_path.as_bytes(), cwd)
    })?;
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fchmodat
    (ctx, dirfd: i32, path: UserPath, mode: u32, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_SYMLINK_NOFOLLOW)?;
    let resolve = resolve_flags_from(flags, path.as_bytes());
    let rc = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        file_chmod_at(path.as_bytes(), cwd, (mode & 0o7777) as u16, resolve)
    })?;
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

// `AT_EACCESS` is accepted and ignored: the machine is single-user uid 0, so
// the effective and real identities are the same one.
define_syscall!(syscall_faccessat
    (ctx, dirfd: i32, path: UserPath, mode: u32, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_SYMLINK_NOFOLLOW | slopos_abi::fs::AT_EACCESS)?;
    let resolve = resolve_flags_from(flags, path.as_bytes());
    let rc = with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
        file_access_at(path.as_bytes(), cwd, mode, resolve)
    })?;
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_access
    (ctx, path: UserPath, mode: u32)
    cap(NoneFd)
    -> Result<(), Errno>
{
    let resolve = resolve_flags_from(0, path.as_bytes());
    let rc = with_cwd_base(ctx, |cwd| file_access_at(path.as_bytes(), cwd, mode, resolve));
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_linkat
    (ctx, olddirfd: i32, old_path: UserPath, newdirfd: i32, new_path: UserPath, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_SYMLINK_FOLLOW)?;
    let follow = flags & AT_SYMLINK_FOLLOW != 0;
    let rc = with_dir_base(ctx, pid, olddirfd, old_path.as_bytes(), |old_cwd| {
        with_dir_base(ctx, pid, newdirfd, new_path.as_bytes(), |new_cwd| {
            file_link_at(old_path.as_bytes(), old_cwd, new_path.as_bytes(), new_cwd, follow)
        })
    })??;
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_link
    (ctx, old_path: UserPath, new_path: UserPath)
    cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| {
        file_link_at(old_path.as_bytes(), cwd, new_path.as_bytes(), cwd, false)
    });
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

/// One `utimensat` timestamp in whole seconds; `None` is `UTIME_OMIT`.
///
/// `UTIME_NOW` without a set wall clock is `EINVAL`: reporting success for a
/// stamp that never landed breaks every mtime-based build system.
fn timestamp_of(ts: &Timespec, now: Option<u64>) -> Result<Option<u64>, Errno> {
    match ts.tv_nsec {
        UTIME_OMIT => Ok(None),
        UTIME_NOW => now.map(Some).ok_or(Errno::EINVAL),
        nsec if (0..1_000_000_000).contains(&nsec) => {
            if ts.tv_sec < 0 {
                return Err(Errno::EINVAL);
            }
            Ok(Some(ts.tv_sec as u64))
        }
        _ => Err(Errno::EINVAL),
    }
}

/// Prove the descriptor exists without touching it. Own frame:
/// [`UserFsStat`] is 144 bytes.
#[inline(never)]
fn validate_fd(table: FdTable, dirfd: i32) -> i32 {
    let mut stat = UserFsStat::default();
    file_fstat_fd(table, dirfd, &mut stat)
}

/// Prove the name resolves without touching it. Its own frame, as above.
#[inline(never)]
fn validate_path(path: &[u8], cwd: &[u8], resolve: u32) -> i32 {
    let mut stat = UserFsStat::default();
    file_stat_at(path, cwd, resolve, &mut stat)
}

// A NULL `path` names the descriptor itself, as Linux's `futimens` does;
// `AT_FDCWD` with a NULL path names nothing and is `EFAULT`.
define_syscall!(syscall_utimensat
    (ctx, dirfd: i32, path: Option<UserPath>, times: Option<UserPtr<[Timespec; 2]>>, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    reject_unknown(flags, AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH)?;
    let now = slopos_kernel_services::clock::realtime_unix_secs().map(u64::from);
    let (atime, mtime) = match times {
        None => {
            let now = now.ok_or(Errno::EINVAL)?;
            (Some(now), Some(now))
        }
        Some(ptr) => {
            let pair: [Timespec; 2] = copy_from_user(ptr.inner()).map_err(|_| Errno::EFAULT)?;
            (timestamp_of(&pair[0], now)?, timestamp_of(&pair[1], now)?)
        }
    };
    // Both omitted is a no-op only once the name has resolved: Linux reports
    // `EBADF`/`ENOENT`, then 0 without reaching the write checks, so a
    // read-only mount answers success.
    let both_omitted = atime.is_none() && mtime.is_none();

    let names_fd = match path.as_ref() {
        None => true,
        Some(p) => p.is_empty() && flags & AT_EMPTY_PATH != 0,
    };
    let rc = if names_fd {
        if dirfd == AT_FDCWD {
            return Err(Errno::EFAULT);
        }
        if both_omitted {
            validate_fd(pid, dirfd)
        } else {
            file_set_times_fd(pid, dirfd, atime, mtime)
        }
    } else {
        let Some(path) = path.as_ref() else {
            return Err(Errno::EFAULT);
        };
        let resolve = resolve_flags_from(flags, path.as_bytes());
        with_dir_base(ctx, pid, dirfd, path.as_bytes(), |cwd| {
            if both_omitted {
                validate_path(path.as_bytes(), cwd, resolve)
            } else {
                file_utimens_at(path.as_bytes(), cwd, atime, mtime, resolve)
            }
        })?
    };
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});
