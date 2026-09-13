use slopos_abi::Errno;
use slopos_abi::fs::{UserFlock, UserFsStat};
use slopos_abi::syscall::{F_GETLK, F_SETLK, F_SETLKW, O_CLOEXEC, O_NONBLOCK};

use slopos_fs::fileio::{
    FdTable, file_dup_fd, file_dup2_fd, file_dup3_fd, file_fcntl_fd, file_fcntl_lock_fd,
    file_fstat_fd, file_pipe_create, file_seek_fd,
};

use slopos_mm::user_copy::{copy_from_user, copy_to_user};
use slopos_mm::user_ptr::UserPtr as MmUserPtr;

use crate::syscall::args::{Fd, UserPtr};
use crate::syscall::common::{errno_from_neg, errno_from_neg64};

define_syscall!(syscall_dup
    (ctx, fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let new_fd = file_dup_fd(pid, fd.raw());
    if new_fd < 0 { Err(errno_from_neg(new_fd)) } else { Ok(new_fd as u64) }
});

define_syscall!(syscall_dup2
    (ctx, old_fd: Fd, new_fd: Fd)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let rc = file_dup2_fd(pid, old_fd.raw(), new_fd.raw());
    if rc < 0 { Err(errno_from_neg(rc)) } else { Ok(rc as u64) }
});

define_syscall!(syscall_dup3
    (ctx, old_fd: Fd, new_fd: Fd, flags: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let rc = file_dup3_fd(pid, old_fd.raw(), new_fd.raw(), flags);
    if rc < 0 { Err(errno_from_neg(rc)) } else { Ok(rc as u64) }
});

/// The record-lock commands take a `struct flock` through `arg`; every other
/// `fcntl` command takes an integer. Own frame: the 32-byte copy must not sit
/// in the dispatch frame.
#[inline(never)]
fn fcntl_lock(table: FdTable, fd: i32, cmd: u64, arg: u64) -> Result<u64, Errno> {
    let ptr = MmUserPtr::<UserFlock>::try_new(arg).map_err(|_| Errno::EFAULT)?;
    let mut lock = copy_from_user(ptr).map_err(|_| Errno::EFAULT)?;
    let rc = file_fcntl_lock_fd(table, fd, cmd, &mut lock);
    if rc < 0 {
        return Err(errno_from_neg64(rc));
    }
    if cmd == F_GETLK {
        copy_to_user(ptr, &lock).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

define_syscall!(syscall_fcntl
    (ctx, fd: Fd, cmd: u64, arg: u64)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if matches!(cmd, F_GETLK | F_SETLK | F_SETLKW) {
        return fcntl_lock(pid, fd.raw(), cmd, arg);
    }
    let rc = file_fcntl_fd(pid, fd.raw(), cmd, arg);
    if rc < 0 { Err(errno_from_neg64(rc)) } else { Ok(rc as u64) }
});

define_syscall!(syscall_lseek
    (ctx, fd: Fd, off: i64, whence: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let new_offset = file_seek_fd(pid, fd.raw(), off, whence);
    if new_offset < 0 { Err(errno_from_neg64(new_offset)) } else { Ok(new_offset as u64) }
});

define_syscall!(syscall_fstat
    (ctx, fd: Fd, out: UserPtr<UserFsStat>)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let mut stat = UserFsStat::default();
    let rc = file_fstat_fd(pid, fd.raw(), &mut stat);
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }
    copy_to_user(out.inner(), &stat).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_pipe
    (ctx, fds: UserPtr<[i32; 2]>)
    cap(NoneSelf)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let mut read_fd: i32 = -1;
    let mut write_fd: i32 = -1;
    let rc = file_pipe_create(pid, 0, &mut read_fd, &mut write_fd);
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }
    let pair = [read_fd, write_fd];
    copy_to_user(fds.inner(), &pair).map_err(|_| Errno::EFAULT)?;
    Ok(())
});

define_syscall!(syscall_pipe2
    (ctx, fds: UserPtr<[i32; 2]>, flags: u32)
    cap(NoneSelf)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    if (flags & !(O_CLOEXEC as u32 | O_NONBLOCK as u32)) != 0 {
        return Err(Errno::EINVAL);
    }
    let mut read_fd: i32 = -1;
    let mut write_fd: i32 = -1;
    let rc = file_pipe_create(pid, flags, &mut read_fd, &mut write_fd);
    if rc != 0 {
        return Err(errno_from_neg(rc));
    }
    let pair = [read_fd, write_fd];
    copy_to_user(fds.inner(), &pair).map_err(|_| Errno::EFAULT)?;
    Ok(())
});
