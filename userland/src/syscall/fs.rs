//! File descriptor operations, as typed wrappers returning `SyscallResult<T>`.

use core::ffi::{CStr, c_char};

use super::RawFd;
use super::error::{SyscallResult, demux};
use super::numbers::*;
use super::raw::{syscall2, syscall3};
use slopos_abi::fs::{
    AT_FDCWD, DT_BLK, DT_CHR, DT_DIR, DT_LNK, DT_REG, FS_LIST_CURSOR_END, FS_TYPE_BLOCKDEV,
    FS_TYPE_CHARDEV, FS_TYPE_DIRECTORY, FS_TYPE_FILE, FS_TYPE_SYMLINK, FS_TYPE_UNKNOWN,
    O_DIRECTORY, O_RDONLY, USER_NAME_MAX, UserFsEntry,
};
use slopos_abi::syscall::{
    SEEK_SET, TIOCGPTPEER, TIOCGSID, TIOCGWINSZ, TIOCSCTTY, TIOCSWINSZ, UserPollFd, UserTermios,
    UserTimeval, UserWinsize,
};
use slopos_abi::{UserFsList, UserFsStat, UserStatfs};
use slopos_slibc::io::dirent::DirentIter;
use slopos_slibc::pal::{Pal, Sys};

/// Open a file by path.
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EACCES` - Permission denied
/// * `EINVAL` - Invalid flags
#[inline(always)]
pub fn open_path(path: *const c_char, flags: u32) -> SyscallResult<super::OwnedFd> {
    Sys::open(path as *const u8, flags as i32, 0)
        // SAFETY: fd is a valid descriptor just returned by the kernel.
        .map(|fd| unsafe { super::OwnedFd::from_raw(fd as RawFd) })
        .map_err(Into::into)
}

#[inline(always)]
pub fn open_cstr(path: &CStr, flags: u32) -> SyscallResult<super::OwnedFd> {
    open_path(path.as_ptr(), flags)
}

/// Escape hatch for well-known fds (0/1/2) and fds taken out of an `OwnedFd`;
/// prefer dropping the `OwnedFd`.
#[inline(always)]
pub fn close_fd_raw(fd: RawFd) -> SyscallResult<()> {
    Sys::close(fd).map_err(Into::into)
}

/// Commits `fd`'s whole filesystem, not the inode.
#[inline(always)]
pub fn fsync(fd: RawFd) -> SyscallResult<()> {
    Sys::fsync(fd).map_err(Into::into)
}

#[inline(always)]
pub fn fdatasync(fd: RawFd) -> SyscallResult<()> {
    Sys::fdatasync(fd).map_err(Into::into)
}

#[inline(always)]
pub fn sync() -> SyscallResult<()> {
    Sys::sync().map_err(Into::into)
}

/// Replace `path`'s contents, committing before returning — for the settings
/// a reboot must not lose. `O_SYNC` rather than a trailing `fsync` so a short
/// write is still durable up to where it got, and so the durability cannot be
/// skipped by an early return between the two calls.
pub fn write_durable(path: &CStr, data: &[u8]) -> SyscallResult<()> {
    use slopos_abi::fs::{O_CREAT, O_SYNC, O_TRUNC, O_WRONLY};
    let fd = open_cstr(path, O_WRONLY | O_CREAT | O_TRUNC | O_SYNC)?;
    let mut written = 0usize;
    while written < data.len() {
        match write_slice(fd.raw(), &data[written..])? {
            0 => return Err(super::error::SyscallError::from(slopos_slibc::errno::EIO)),
            n => written += n,
        }
    }
    Ok(())
}

/// Consumes the handle so `Drop` cannot double-close. On failure the fd is
/// still consumed: the kernel either closed it or it was invalid.
#[inline(always)]
pub fn close_fd(fd: super::OwnedFd) -> SyscallResult<()> {
    close_fd_raw(fd.into_raw())
}

/// Read from a file descriptor into a buffer. Returns 0 at EOF.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
#[inline(always)]
pub fn read_slice(fd: RawFd, buf: &mut [u8]) -> SyscallResult<usize> {
    Sys::read(fd, buf.as_mut_ptr(), buf.len()).map_err(Into::into)
}

/// Write to a file descriptor from a buffer.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `EIO` - I/O error
/// * `ENOSPC` - No space left on device
#[inline(always)]
pub fn write_slice(fd: RawFd, buf: &[u8]) -> SyscallResult<usize> {
    Sys::write(fd, buf.as_ptr(), buf.len()).map_err(Into::into)
}

/// Get file status/metadata.
///
/// # Errors
/// * `ENOENT` - File not found
#[inline(always)]
pub fn stat_path(path: *const c_char, out_stat: &mut UserFsStat) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_STAT, path as u64, out_stat as *mut _ as u64) };
    demux(result).map(|_| ())
}

/// Set a regular file's length by path, freeing blocks past it or extending
/// sparsely.
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EISDIR` - The path names a directory
/// * `EROFS` - The mount is read-only, or the inode is sealed
#[inline(always)]
pub fn truncate_path(path: *const c_char, length: u64) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_TRUNCATE, path as u64, length) };
    demux(result).map(|_| ())
}

/// Create a directory with the usual `0o755` permissions; the wrapper takes no
/// mode of its own.
///
/// # Errors
/// * `EEXIST` - Directory already exists
/// * `ENOENT` - Parent directory not found
/// * `ENOSPC` - No space left on device
#[inline(always)]
pub fn mkdir_path(path: *const c_char) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_MKDIR, path as u64, 0o755) };
    demux(result).map(|_| ())
}

/// Remove a file or empty directory.
///
/// # Errors
/// * `ENOENT` - File not found
/// * `EISDIR` - Is a non-empty directory
/// * `EBUSY` - File is in use
#[inline(always)]
pub fn unlink_path(path: *const c_char) -> SyscallResult<()> {
    Sys::unlink(path as *const u8).map_err(Into::into)
}

/// Atomically rename/move a file or directory.
///
/// # Errors
/// * `ENOENT` - Source not found
/// * `EXDEV` - Cross-device rename
/// * `ENOTSUP` - Filesystem doesn't support rename
#[inline(always)]
pub fn rename(old_path: *const c_char, new_path: *const c_char) -> SyscallResult<()> {
    Sys::rename(old_path as *const u8, new_path as *const u8).map_err(Into::into)
}

/// List directory contents into `list.entries`, resuming from `list.cursor`
/// and updating it: zero it for the first call, carry it back verbatim, and
/// stop once it reads [`FS_LIST_CURSOR_END`].
///
/// A directory larger than `max_entries` therefore takes several calls rather
/// than being cut off. Names are raw bytes, NUL-terminated inside the entry.
///
/// # Errors
/// * `ENOENT` - Directory not found
/// * `ENOTDIR` - Path is not a directory
pub fn list_dir(path: *const c_char, list: &mut UserFsList) -> SyscallResult<()> {
    let fd = Sys::openat(
        AT_FDCWD,
        path as *const u8,
        (O_RDONLY | O_DIRECTORY) as i32,
        0,
    )?;
    let result = read_entries(fd, list);
    let _ = Sys::close(fd);
    result
}

/// Drains one `list.max_entries`-sized batch from an open directory fd. The
/// cursor is a `getdents64` `d_off`, which is what `lseek` resumes a directory
/// at, so a truncated batch loses nothing.
fn read_entries(fd: RawFd, list: &mut UserFsList) -> SyscallResult<()> {
    list.count = 0;
    if list.cursor == FS_LIST_CURSOR_END || list.entries.is_null() || list.max_entries == 0 {
        return Ok(());
    }
    if list.cursor != 0 {
        Sys::lseek(fd, list.cursor as i64, SEEK_SET as i32)?;
    }

    let mut batch = [0u8; 4096];
    while list.count < list.max_entries {
        let filled = Sys::getdents64(fd, batch.as_mut_ptr(), batch.len())?;
        if filled == 0 {
            list.cursor = FS_LIST_CURSOR_END;
            return Ok(());
        }
        for record in DirentIter::new(&batch[..filled]) {
            if record.name.is_empty() || record.name.len() > USER_NAME_MAX {
                continue;
            }
            // SAFETY: `count < max_entries` bounds the index, and the caller
            // owns `max_entries` entries at `entries`.
            let entry = unsafe { &mut *list.entries.add(list.count as usize) };
            *entry = UserFsEntry::new();
            entry.name[..record.name.len()].copy_from_slice(record.name);
            entry.type_ = fs_type_of(record.d_type);
            entry.size = entry_size(fd, entry.name.as_ptr());
            list.count += 1;
            list.cursor = record.d_off as u64;
            if list.count == list.max_entries {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn fs_type_of(d_type: u8) -> u8 {
    match d_type {
        DT_REG => FS_TYPE_FILE,
        DT_DIR => FS_TYPE_DIRECTORY,
        DT_CHR => FS_TYPE_CHARDEV,
        DT_BLK => FS_TYPE_BLOCKDEV,
        DT_LNK => FS_TYPE_SYMLINK,
        _ => FS_TYPE_UNKNOWN,
    }
}

/// 0 for an entry that cannot be stat'ed — a listing is not the place to fail
/// over one unreadable name.
fn entry_size(dirfd: RawFd, name: *const u8) -> u64 {
    let mut st = UserFsStat::default();
    match Sys::fstatat(dirfd, name, &mut st, 0) {
        Ok(()) => st.st_size.max(0) as u64,
        Err(_) => 0,
    }
}

#[inline(always)]
pub fn dup(fd: RawFd) -> SyscallResult<super::OwnedFd> {
    Sys::dup(fd)
        // SAFETY: v is a valid fd just returned by the kernel.
        .map(|v| unsafe { super::OwnedFd::from_raw(v as RawFd) })
        .map_err(Into::into)
}

/// Closes whatever was at `new_fd`. The `new_fd` slot is a raw alias
/// afterwards: no `OwnedFd` tracks its lifetime.
#[inline(always)]
pub fn dup2(old_fd: RawFd, new_fd: RawFd) -> SyscallResult<RawFd> {
    Sys::dup2(old_fd, new_fd)
        .map(|v| v as RawFd)
        .map_err(Into::into)
}

#[inline(always)]
pub fn lseek(fd: RawFd, offset: i64, whence: u32) -> SyscallResult<i64> {
    Sys::lseek(fd, offset, whence as i32).map_err(Into::into)
}

/// Returns `(read_end, write_end)`.
#[inline(always)]
pub fn pipe() -> SyscallResult<(super::OwnedFd, super::OwnedFd)> {
    let mut raw = [0i32; 2];
    Sys::pipe(&mut raw as *mut [i32; 2]).map_err(super::SyscallError::from)?;
    // SAFETY: raw fds are valid descriptors just returned by the kernel.
    Ok(unsafe {
        (
            super::OwnedFd::from_raw(raw[0]),
            super::OwnedFd::from_raw(raw[1]),
        )
    })
}

/// Returns `(read_end, write_end)`.
#[inline(always)]
pub fn pipe2(flags: u32) -> SyscallResult<(super::OwnedFd, super::OwnedFd)> {
    let mut raw = [0i32; 2];
    let result = unsafe { syscall2(SYSCALL_PIPE2, raw.as_mut_ptr() as u64, flags as u64) };
    demux(result)?;
    // SAFETY: raw fds are valid descriptors just returned by the kernel.
    Ok(unsafe {
        (
            super::OwnedFd::from_raw(raw[0]),
            super::OwnedFd::from_raw(raw[1]),
        )
    })
}

#[inline(always)]
pub fn poll(fds: &mut [UserPollFd], timeout_ms: i64) -> SyscallResult<usize> {
    let result = unsafe {
        syscall3(
            SYSCALL_POLL,
            fds.as_mut_ptr() as u64,
            fds.len() as u64,
            timeout_ms as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

/// The kernel writes the time left back into `timeout`, so it must be
/// writable and is not reusable across calls unmodified.
#[inline(always)]
pub fn select(
    nfds: usize,
    readfds: *mut u8,
    writefds: *mut u8,
    exceptfds: *mut u8,
    timeout: *mut UserTimeval,
) -> SyscallResult<usize> {
    let result = unsafe {
        super::raw::syscall5(
            SYSCALL_SELECT,
            nfds as u64,
            readfds as u64,
            writefds as u64,
            exceptfds as u64,
            timeout as u64,
        )
    };
    demux(result).map(|v| v as usize)
}

#[inline(always)]
pub fn tcgetpgrp(fd: RawFd) -> SyscallResult<u32> {
    let mut pgid = 0u32;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGPGRP,
            (&mut pgid as *mut u32) as u64,
        )
    };
    demux(result).map(|_| pgid)
}

#[inline(always)]
pub fn tcsetpgrp(fd: RawFd, pgid: u32) -> SyscallResult<()> {
    let mut target = pgid;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCSPGRP,
            (&mut target as *mut u32) as u64,
        )
    };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn tiocsctty(fd: RawFd) -> SyscallResult<()> {
    let result = unsafe { syscall3(SYSCALL_IOCTL, fd as u64, TIOCSCTTY, 0) };
    demux(result).map(|_| ())
}

/// Session id owning the terminal on `fd` (TIOCGSID ioctl).
///
/// Fails when `fd` is not a terminal or the terminal has no session, which is
/// how a shell tells an unclaimed terminal from one already in use.
#[inline(always)]
pub fn tcgetsid(fd: RawFd) -> SyscallResult<u32> {
    let mut sid = 0u32;
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGSID,
            (&mut sid as *mut u32) as u64,
        )
    };
    demux(result).map(|_| sid)
}

/// Open the PTY slave peer of a master FD (TIOCGPTPEER ioctl). The new fd
/// shares the slave's open state with every other slave fd.
#[inline(always)]
pub fn ioctl_tiocgptpeer(master_fd: RawFd) -> SyscallResult<super::OwnedFd> {
    let result = unsafe { syscall3(SYSCALL_IOCTL, master_fd as u64, TIOCGPTPEER, 0) };
    // SAFETY: v is a valid fd just returned by the kernel.
    demux(result).map(|v| unsafe { super::OwnedFd::from_raw(v as i32) })
}

#[inline(always)]
pub fn tcgetattr(fd: RawFd) -> SyscallResult<UserTermios> {
    let mut t = UserTermios::default();
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TCGETS,
            (&mut t as *mut UserTermios) as u64,
        )
    };
    demux(result).map(|_| t)
}

/// POSIX `isatty(3)`. The TCGETS probe is exact, not approximate:
/// `syscall_ioctl` resolves the descriptor through `file_get_tty_index`, which
/// yields nothing unless the file's kind is `Tty`.
#[inline]
pub fn isatty(fd: RawFd) -> bool {
    tcgetattr(fd).is_ok()
}

#[inline(always)]
pub fn tcsetattr(fd: RawFd, t: &UserTermios) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TCSETS,
            (t as *const UserTermios) as u64,
        )
    };
    demux(result).map(|_| ())
}

#[inline(always)]
pub fn tiocgwinsz(fd: RawFd) -> SyscallResult<UserWinsize> {
    let mut ws = UserWinsize::default();
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCGWINSZ,
            (&mut ws as *mut UserWinsize) as u64,
        )
    };
    demux(result).map(|_| ws)
}

/// The kernel raises SIGWINCH to the slave foreground process group when the
/// row/col dimensions change.
#[inline(always)]
pub fn tiocswinsz(fd: RawFd, ws: &UserWinsize) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_IOCTL,
            fd as u64,
            TIOCSWINSZ,
            (ws as *const UserWinsize) as u64,
        )
    };
    demux(result).map(|_| ())
}

/// Works on any fd type (pipes, sockets, files).
#[inline(always)]
pub fn set_fd_nonblocking(fd: RawFd) -> SyscallResult<()> {
    use super::error::SyscallError;
    use slopos_abi::syscall::{F_GETFL, F_SETFL, O_NONBLOCK};
    let current = Sys::fcntl(fd, F_GETFL as i32, 0).map_err(SyscallError::from)?;
    let _ = Sys::fcntl(fd, F_SETFL as i32, (current as u64) | O_NONBLOCK)
        .map_err(SyscallError::from)?;
    Ok(())
}

/// Spawned children never inherit a `FD_CLOEXEC` descriptor (spawn is
/// fork+exec in one step), and `exec` strips it from forked children.
#[inline(always)]
pub fn set_fd_cloexec(fd: RawFd) -> SyscallResult<()> {
    use super::error::SyscallError;
    use slopos_abi::syscall::{F_GETFD, F_SETFD, FD_CLOEXEC};
    let current = Sys::fcntl(fd, F_GETFD as i32, 0).map_err(SyscallError::from)?;
    let _ = Sys::fcntl(fd, F_SETFD as i32, (current as u64) | FD_CLOEXEC)
        .map_err(SyscallError::from)?;
    Ok(())
}

/// Filesystem statistics for the mount `path` resolves through.
///
/// # Errors
/// * `ENOENT` - Path not found
/// * `EOPNOTSUPP` - The filesystem reports no capacity (devfs)
#[inline(always)]
pub fn statfs_path(path: *const c_char) -> SyscallResult<UserStatfs> {
    let mut stats = UserStatfs::default();
    Sys::statfs(path as *const u8, (&mut stats as *mut UserStatfs).cast())
        .map(|()| stats)
        .map_err(Into::into)
}

/// Filesystem statistics for the filesystem `fd`'s file lives on.
///
/// # Errors
/// * `EBADF` - Invalid file descriptor
/// * `ENOSYS` - The descriptor has no filesystem behind it (pipe, socket, tty)
#[inline(always)]
pub fn fstatfs(fd: RawFd) -> SyscallResult<UserStatfs> {
    let mut stats = UserStatfs::default();
    Sys::fstatfs(fd, (&mut stats as *mut UserStatfs).cast())
        .map(|()| stats)
        .map_err(Into::into)
}

/// NUL-terminate `src` inside `dst`; a value with no room for the terminator
/// is refused, since the kernel reads these as C strings.
fn copy_mount_field(dst: &mut [u8], src: &[u8]) -> bool {
    if src.len() >= dst.len() {
        return false;
    }
    dst[..src.len()].copy_from_slice(src);
    dst[src.len()] = 0;
    true
}

/// `mount(2)`. `source` is meaningful only for a filesystem with a device to
/// name: `ramfs` and `devfs` ignore it, and `ext2` refuses a non-empty one
/// because the kernel holds exactly one instance, bound at boot to `root=`.
///
/// # Errors
/// * `EPERM` - The caller does not hold the mount capability
/// * `ENODEV` - Unsupported `fstype`, or no ext2 attached
/// * `ENOTDIR` - `target` is not a directory
/// * `EBUSY` - `target` is `/` or already a mount point
/// * `ENOSPC` - The mount table or the ramfs pool is full
pub fn mount(source: &[u8], target: &[u8], fstype: &[u8], flags: u32) -> SyscallResult<()> {
    use super::error::SyscallError;
    let mut source_buf = [0u8; 256];
    let mut target_buf = [0u8; 256];
    let mut fstype_buf = [0u8; slopos_abi::fs::MOUNT_FSTYPE_MAX];
    if !copy_mount_field(&mut source_buf, source)
        || !copy_mount_field(&mut target_buf, target)
        || !copy_mount_field(&mut fstype_buf, fstype)
    {
        return Err(SyscallError::from(slopos_slibc::errno::ENAMETOOLONG));
    }
    Sys::mount(
        source_buf.as_ptr(),
        target_buf.as_ptr(),
        fstype_buf.as_ptr(),
        flags,
    )
    .map_err(Into::into)
}

/// `umount2(2)`. `MNT_DETACH` drops a mount a descriptor still holds.
///
/// # Errors
/// * `EPERM` - The caller does not hold the mount capability
/// * `EINVAL` - Nothing is mounted at `target`
/// * `EBUSY` - `target` is `/`, or a descriptor still holds the filesystem
pub fn umount2(target: *const c_char, flags: u32) -> SyscallResult<()> {
    Sys::umount2(target as *const u8, flags).map_err(Into::into)
}

/// `newdirfd` is `AT_FDCWD` to resolve `link` against the working directory.
///
/// # Errors
/// * `EEXIST` - `link` already exists
/// * `ENOENT` - A component of `link`'s parent is missing
/// * `EROFS` - The mount is read-only
#[inline(always)]
pub fn symlinkat(target: *const c_char, newdirfd: i32, link: *const c_char) -> SyscallResult<()> {
    let result = unsafe {
        syscall3(
            SYSCALL_SYMLINKAT,
            target as u64,
            newdirfd as i64 as u64,
            link as u64,
        )
    };
    demux(result).map(|_| ())
}

/// Advisory whole-file lock. `operation` is `LOCK_SH`, `LOCK_EX` or
/// `LOCK_UN`, optionally `| LOCK_NB`. The lock belongs to the open file
/// description, so a second `open` of the same path contends with the first.
///
/// # Errors
/// * `EWOULDBLOCK` - `LOCK_NB` was set and the lock is held elsewhere
/// * `EBADF` - Invalid file descriptor
#[inline(always)]
pub fn flock(fd: RawFd, operation: u64) -> SyscallResult<()> {
    let result = unsafe { syscall2(SYSCALL_FLOCK, fd as i64 as u64, operation) };
    demux(result).map(|_| ())
}
