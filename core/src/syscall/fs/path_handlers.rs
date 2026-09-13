use slopos_abi::Errno;
use slopos_abi::fs::{USER_FS_MAX_ENTRIES, UserFsEntry, UserFsList, UserFsStat};

use slopos_fs::fileio::{
    file_chmod_at, file_close_fd, file_list_at_from, file_mkdir_at, file_open_at, file_read_fd,
    file_readlink_at, file_rmdir_at, file_stat_at, file_symlink_at, file_sync_fd, file_truncate_at,
    file_unlink_at, file_write_fd,
};

use slopos_mm::user_copy::{copy_bytes_to_user, copy_from_user, copy_to_user};
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};
use slopos_mm::user_ptr::{UserBytes as MmUserBytes, UserPtr as MmUserPtr};
use slopos_ostd::KVec;

use crate::syscall::args::{Fd, UserBytes, UserPath, UserPtr};
use crate::syscall::common::{USER_PATH_MAX, errno_from_neg};
use crate::syscall::fs::dirfd::{
    open_resolve_flags, reject_non_directory, resolve_flags_from, with_cwd_base,
};

define_syscall!(syscall_fs_open
    (ctx, path: UserPath, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let resolve = open_resolve_flags(flags, path.as_bytes());
    let fd = with_cwd_base(ctx, |cwd| {
        file_open_at(pid, path.as_bytes(), cwd, flags, resolve, None)
    });
    if fd < 0 {
        Err(errno_from_neg(fd))
    } else {
        Ok(fd as u64)
    }
});

define_syscall!(syscall_fs_close
    (ctx, fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_close_fd(pid, fd.raw());
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_read
    (ctx, fd: Fd, buf: UserBytes)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let count = buf.len();
    let mut io_buf = UserWriteBuf::new(buf.base_u64(), count).ok_or(Errno::EFAULT)?;
    let bytes = file_read_fd(pid, fd.raw(), &mut io_buf);
    if bytes == -512 {
        return Err(Errno::ERESTARTSYS);
    }
    if bytes < 0 {
        return Err(Errno::from_raw(bytes as i32).unwrap_or(Errno::EINVAL));
    }
    Ok(bytes as u64)
});

define_syscall!(syscall_fs_write
    (ctx, fd: Fd, buf: UserBytes)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let count = buf.len();
    let io_buf = UserReadBuf::new(buf.base_u64(), count).ok_or(Errno::EFAULT)?;
    let bytes = file_write_fd(pid, fd.raw(), &io_buf);
    if bytes < 0 {
        Err(Errno::from_raw(bytes as i32).unwrap_or(Errno::EINVAL))
    } else {
        Ok(bytes as u64)
    }
});

// Commits one inode, so the lock is held for that inode's blocks rather than
// every dirty block on the mount. It is still ext2's one global sleeping
// mutex, so a path walk on the same filesystem waits behind it.
define_syscall!(syscall_fsync
    (ctx, fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_sync_fd(pid, fd.raw(), false);
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fdatasync
    (ctx, fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_sync_fd(pid, fd.raw(), true);
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

// No fd and no capability, and it commits every mount — so this is the one
// that can still stall the machine. `ext2_vfs_sync` coalesces callers so a
// loop of them costs one writeback pass rather than one per call; what stays
// unbounded is the wait behind the pass in flight.
define_syscall!(syscall_sync
    (ctx)
    cap(NoneSelf)
    -> Result<(), Errno>
{
    slopos_fs::vfs::vfs_sync_all().map_err(|e| e.to_errno())
});

define_syscall!(syscall_fs_stat
    (ctx, path: UserPath, out: UserPtr<UserFsStat>) cap(NoneFd)
    -> Result<(), Errno>
{
    let mut stat = UserFsStat::default();
    let resolve = resolve_flags_from(0, path.as_bytes());
    let rc = with_cwd_base(ctx, |cwd| {
        file_stat_at(path.as_bytes(), cwd, resolve, &mut stat)
    });
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }
    copy_to_user(out.inner(), &stat).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_fs_mkdir
    (ctx, path: UserPath) cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| file_mkdir_at(path.as_bytes(), cwd));
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_unlink
    (ctx, path: UserPath) cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| {
        if let Err(e) = reject_non_directory(path.as_bytes(), cwd) {
            return e.raw();
        }
        file_unlink_at(path.as_bytes(), cwd)
    });
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_rmdir
    (ctx, path: UserPath) cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| file_rmdir_at(path.as_bytes(), cwd));
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_symlink
    (ctx, target: UserPath, link_path: UserPath) cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| {
        file_symlink_at(target.as_bytes(), link_path.as_bytes(), cwd)
    });
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

// Never NUL-terminates, per POSIX: the count is the answer, and a target
// longer than the buffer is truncated rather than an error.
define_syscall!(syscall_readlink
    (ctx, path: UserPath, buf: UserBytes) cap(NoneFd)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let len = buf.len().min(USER_PATH_MAX);
    let n = with_cwd_base(ctx, |cwd| {
        readlink_at_into_user(path.as_bytes(), cwd, buf.base_u64(), len)
    })?;
    Ok(n as u64)
});

/// Its own frame and its own heap buffer: a `USER_PATH_MAX` staging array next
/// to the caller's already-heaped `UserPath` would be 4 KiB of stack.
#[inline(never)]
pub(crate) fn readlink_at_into_user(
    path: &[u8],
    cwd: &[u8],
    user_buf: u64,
    len: usize,
) -> Result<usize, Errno> {
    if len == 0 {
        return Ok(0);
    }
    let mut staging = KVec::<u8>::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    let rc = file_readlink_at(path, cwd, &mut staging[..len]);
    if rc < 0 {
        return Err(errno_from_neg(rc as i32));
    }
    let n = (rc as usize).min(len);
    let user = MmUserBytes::try_new(user_buf, n).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user, &staging[..n]).map_err(|_| Errno::EFAULT)?;
    Ok(n)
}

define_syscall!(syscall_truncate
    (ctx, path: UserPath, length: u64) cap(NoneFd)
    -> Result<(), Errno>
{
    let rc = with_cwd_base(ctx, |cwd| {
        if let Err(e) = reject_non_directory(path.as_bytes(), cwd) {
            return e.raw();
        }
        file_truncate_at(path.as_bytes(), cwd, length)
    });
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_chmod
    (ctx, path: UserPath, mode: u32) cap(NoneFd)
    -> Result<(), Errno>
{
    let resolve = resolve_flags_from(0, path.as_bytes());
    let rc = with_cwd_base(ctx, |cwd| {
        file_chmod_at(path.as_bytes(), cwd, (mode & 0o7777) as u16, resolve)
    });
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_fs_list
    (ctx, path: UserPath, list: UserPtr<UserFsList>) cap(NoneFd)
    -> Result<(), Errno>
{
    let mut list_hdr = copy_from_user(list.inner()).map_err(|_| Errno::EFAULT)?;

    let cap = list_hdr.max_entries;
    if cap == 0 || cap > USER_FS_MAX_ENTRIES || list_hdr.entries.is_null() {
        return Err(Errno::EINVAL);
    }

    let cap_usize = cap as usize;
    let zero_entry = UserFsEntry::default();
    let mut tmp = KVec::<UserFsEntry>::with_capacity(cap_usize).map_err(|_| Errno::ENOMEM)?;
    for _ in 0..cap_usize {
        tmp.push(zero_entry).map_err(|_| Errno::ENOMEM)?;
    }

    let mut count: u32 = 0;
    let mut cursor = list_hdr.cursor;
    let rc = with_cwd_base(ctx, |cwd| {
        file_list_at_from(path.as_bytes(), cwd, tmp.as_mut_slice(), &mut cursor, &mut count)
    });
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }

    list_hdr.count = count;
    list_hdr.cursor = cursor;

    let entries_bytes =
        slopos_ostd::util::byte_view::pod_slice_as_bytes(&tmp[..count as usize]);
    let entries_user = MmUserBytes::try_new(list_hdr.entries as u64, entries_bytes.len())
        .map_err(|_| Errno::EFAULT)?;

    copy_bytes_to_user(entries_user, entries_bytes).map_err(|_| Errno::EFAULT)?;
    let hdr_ptr = MmUserPtr::<UserFsList>::try_new(list.as_u64()).map_err(|_| Errno::EFAULT)?;
    copy_to_user(hdr_ptr, &list_hdr).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_rename
    (ctx, old_path: UserPath, new_path: UserPath) cap(NoneFd)
    -> Result<(), Errno>
{
    with_cwd_base(ctx, |cwd| {
        slopos_fs::vfs::vfs_rename_at(old_path.as_bytes(), cwd, new_path.as_bytes(), cwd)
            .map_err(|e| e.to_errno())
    })
});
