use slopos_abi::Errno;
use slopos_abi::fs::{AT_SYMLINK_NOFOLLOW, UserFsStat};

use slopos_fs::fileio::{
    file_close_fd, file_read_fd, file_readlink_at, file_rmdir_at, file_symlink_at, file_sync_fd,
    file_truncate_at, file_unlink_at, file_write_fd,
};

use slopos_mm::user_copy::copy_bytes_to_user;
use slopos_mm::user_io_buf::{UserReadBuf, UserWriteBuf};
use slopos_mm::user_ptr::UserBytes as MmUserBytes;
use slopos_ostd::KVec;

use crate::syscall::args::{Fd, UserBytes, UserPath, UserPtr};
use crate::syscall::common::{USER_PATH_MAX, errno_from_neg};
use crate::syscall::fs::at_handlers::{chmod_at, mkdir_at, open_at, stat_at_into_user};
use crate::syscall::fs::dirfd::{reject_non_directory, with_cwd_base};

define_syscall!(syscall_open
    (ctx, path: UserPath, flags: u32, mode: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    with_cwd_base(ctx, |cwd| open_at(pid, path.as_bytes(), cwd, flags, mode))
});

define_syscall!(syscall_close
    (ctx, fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_close_fd(pid, fd.raw());
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_read
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

define_syscall!(syscall_write
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

define_syscall!(syscall_stat
    (ctx, path: UserPath, out: UserPtr<UserFsStat>) cap(NoneFd)
    -> Result<(), Errno>
{
    with_cwd_base(ctx, |cwd| stat_at_into_user(path.as_bytes(), cwd, 0, out.inner()))
});

define_syscall!(syscall_lstat
    (ctx, path: UserPath, out: UserPtr<UserFsStat>) cap(NoneFd)
    -> Result<(), Errno>
{
    with_cwd_base(ctx, |cwd| {
        stat_at_into_user(path.as_bytes(), cwd, AT_SYMLINK_NOFOLLOW, out.inner())
    })
});

define_syscall!(syscall_mkdir
    (ctx, path: UserPath, mode: u32) cap(NoneFd)
    -> Result<(), Errno>
{
    with_cwd_base(ctx, |cwd| mkdir_at(path.as_bytes(), cwd, mode))
});

define_syscall!(syscall_unlink
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
    with_cwd_base(ctx, |cwd| chmod_at(path.as_bytes(), cwd, mode, 0))
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
