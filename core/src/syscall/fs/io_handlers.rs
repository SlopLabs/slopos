//! Positional and vectored I/O, plus the descriptor-addressed directory,
//! mode, and lock calls.

use slopos_abi::Errno;
use slopos_abi::fs::UserIovec;

use slopos_fs::fileio::{
    file_fchmod_fd, file_flock_fd, file_getdents_commit_fd, file_getdents_fd, file_pread_fd,
    file_pwrite_fd,
};

use slopos_mm::user_copy::copy_bytes_to_user;
use slopos_mm::user_io_buf::{UserIovecBuf, UserReadBuf, UserWriteBuf, stage_iovec};
use slopos_mm::user_ptr::UserBytes as MmUserBytes;
use slopos_ostd::KVec;

use crate::syscall::args::{Fd, UserBytes, UserSlice};
use crate::syscall::common::errno_from_neg;

/// A user buffer larger than this is served short, which `getdents64` permits.
const GETDENTS_STAGING_MAX: usize = 64 * 1024;

// `stage_iovec` and the segment-list validation live in `slopos_mm` so the
// syscall handlers, `sendmsg`/`recvmsg` and the ring share exactly one copy.

define_syscall!(syscall_pread64
    (ctx, fd: Fd, buf: UserBytes, offset: u64)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let mut io_buf = UserWriteBuf::new(buf.base_u64(), buf.len()).ok_or(Errno::EFAULT)?;
    let bytes = file_pread_fd(pid, fd.raw(), &mut io_buf, offset);
    if bytes < 0 {
        Err(errno_from_neg(bytes as i32))
    } else {
        Ok(bytes as u64)
    }
});

define_syscall!(syscall_pwrite64
    (ctx, fd: Fd, buf: UserBytes, offset: u64)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let io_buf = UserReadBuf::new(buf.base_u64(), buf.len()).ok_or(Errno::EFAULT)?;
    let bytes = file_pwrite_fd(pid, fd.raw(), &io_buf, offset);
    if bytes < 0 {
        Err(errno_from_neg(bytes as i32))
    } else {
        Ok(bytes as u64)
    }
});

define_syscall!(syscall_readv
    (ctx, fd: Fd, iov: UserSlice<UserIovec>)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let segments = stage_iovec(iov.base_u64(), iov.len())?;
    let mut io_buf = UserIovecBuf::new(&segments)?;
    if slopos_abi::io::IoBufWrite::len(&io_buf) == 0 {
        return Ok(0);
    }
    let bytes = slopos_fs::fileio::file_read_fd(pid, fd.raw(), &mut io_buf);
    if bytes == -512 {
        return Err(Errno::ERESTARTSYS);
    }
    if bytes < 0 {
        Err(errno_from_neg(bytes as i32))
    } else {
        Ok(bytes as u64)
    }
});

define_syscall!(syscall_writev
    (ctx, fd: Fd, iov: UserSlice<UserIovec>)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    let segments = stage_iovec(iov.base_u64(), iov.len())?;
    let io_buf = UserIovecBuf::new(&segments)?;
    if slopos_abi::io::IoBufRead::len(&io_buf) == 0 {
        return Ok(0);
    }
    let bytes = slopos_fs::fileio::file_write_fd(pid, fd.raw(), &io_buf);
    if bytes < 0 {
        Err(errno_from_neg(bytes as i32))
    } else {
        Ok(bytes as u64)
    }
});

define_syscall!(syscall_getdents64
    (ctx, fd: Fd, buf: UserBytes)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<u64, Errno>
{
    if buf.base_u64() == 0 {
        return Err(Errno::EFAULT);
    }
    let len = buf.len().min(GETDENTS_STAGING_MAX);
    if len == 0 {
        return Err(Errno::EINVAL);
    }
    let mut staging = KVec::<u8>::zeroed(len).map_err(|_| Errno::ENOMEM)?;
    let (written, cookie) = file_getdents_fd(pid, fd.raw(), &mut staging[..len])?;
    if written == 0 {
        return Ok(0);
    }
    let user = MmUserBytes::try_new(buf.base_u64(), written).map_err(|_| Errno::EFAULT)?;
    copy_bytes_to_user(user, &staging[..written]).map_err(|_| Errno::EFAULT)?;
    // Committed only once the bytes are out: a cursor advanced before a
    // faulting copy-out loses the whole batch, which is not re-readable.
    file_getdents_commit_fd(pid, fd.raw(), cookie)?;
    Ok(written as u64)
});

define_syscall!(syscall_fchmod
    (ctx, fd: Fd, mode: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_fchmod_fd(pid, fd.raw(), (mode & 0o7777) as u16);
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});

define_syscall!(syscall_flock
    (ctx, fd: Fd, operation: u32)
    cap(NoneFd)
    requires(let pid: process_id)
    -> Result<(), Errno>
{
    let rc = file_flock_fd(pid, fd.raw(), operation);
    if rc != 0 { Err(errno_from_neg(rc)) } else { Ok(()) }
});
