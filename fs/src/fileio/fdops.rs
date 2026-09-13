use core::ffi::c_int;
use core::sync::atomic::Ordering;

use super::*;

use slopos_abi::Errno;
use slopos_abi::fs::{UserDirent64, UserFlock, UserFsEntry, UserFsStat};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::syscall::{
    F_DUPFD, F_GETFD, F_GETFL, F_RDLCK, F_SETFD, F_SETFL, F_SETLK, F_SETLKW, F_WRLCK, FD_CLOEXEC,
    O_CLOEXEC, O_NOCTTY, O_NONBLOCK, SEEK_CUR, SEEK_END, SEEK_SET,
};

use crate::pipe;
use crate::pipe_file_ops::{PIPE_READ_OPS, PIPE_WRITE_OPS, pipe_backings};
use crate::vfs::path::{RESOLVE_FOLLOW, RESOLVE_MUST_BE_DIR};
use crate::vfs::{FileType, FsStats, InodeId, VfsError, vfs_mkdir_at, vfs_unlink_at};
use crate::vfs_file_ops::{
    VFS_FILE_OPS, vfs_dir_handle_still_names, vfs_file_inode, vfs_file_statfs,
    vfs_open_dir_handle_at, vfs_open_handle_flags_at, vnode_backing,
};
use slopos_abi::tty_error::TtyError;
use slopos_ostd::process::quota::FileBacking;

#[allow(non_camel_case_types)]
type ssize_t = isize;

/// Consumes `backing`, dropping it on every error path, so callers must not
/// tear the subsystem object down on their own error arm.
fn install_fd_entry(
    table: FdTable,
    ops: &'static dyn FileOps,
    handle: usize,
    mut flags: OpenMode,
    fd_flags: FdFlags,
    call_tty_policy: Option<TtyIndex>,
    backing: Option<KArc<dyn FileBacking>>,
    dir_path: Option<KVec<u8>>,
) -> c_int {
    let mut position = 0u64;
    if flags.contains(OpenMode::APPEND) {
        match ops.size(handle) {
            Some(size) => position = size,
            None => return Errno::ENXIO.raw(),
        }
    }

    if ops.kind() == FileKind::Socket {
        let mode_bits = flags & (OpenMode::READ | OpenMode::WRITE);
        flags = mode_bits;
        let _ = ops.set_status_flags(handle, flags.bits());
    }

    let cloexec = fd_flags.cloexec || (flags.bits() & O_CLOEXEC as u32) != 0;
    let close_on_fork = fd_flags.close_on_fork;

    let Some(open_file) = new_open_file_with_dir(ops, handle, flags, position, backing, dir_path)
    else {
        return Errno::ENFILE.raw();
    };

    // Charged before the table lock is taken, so a refusal never unwinds under it.
    let Ok(reservation) = try_charge::<FdSlot>(table.account(), 1) else {
        drop(open_file);
        return Errno::EMFILE.raw();
    };

    let Some(mut inner) = lock_table_slot(table) else {
        return Errno::ESRCH.raw();
    };

    let slot_result = {
        match find_free_slot(&inner) {
            Some(slot_idx) => {
                inner.descriptors[slot_idx] = Some(FdEntry::new(
                    open_file,
                    FdFlags {
                        cloexec,
                        close_on_fork,
                    },
                    reservation,
                ));
                Ok(slot_idx as c_int)
            }
            None => Err(open_file),
        }
    };

    drop(inner);

    match slot_result {
        Ok(fd) => {
            if let Some(tty_idx) = call_tty_policy {
                maybe_acquire_controlling_tty_on_open(tty_idx, flags.bits());
            }
            fd
        }
        Err(open_file) => {
            // Detach-then-drop: teardown runs only after the slot lock is released.
            drop(open_file);
            Errno::EMFILE.raw()
        }
    }
}

fn current_tty_ops() -> &'static dyn FileOps {
    with_open_files(|state| effective_tty_ops(&state.external_ops))
}

fn current_socket_ops() -> Option<&'static dyn FileOps> {
    with_open_files(|state| external_socket_ops(&state.external_ops))
}

pub fn file_open_for_process(table: FdTable, path: &[u8], posix_flags: u32) -> c_int {
    file_open_at(table, path, b"/", posix_flags, RESOLVE_FOLLOW, None)
}

/// `openat(2)`.
pub fn file_open_at(
    table: FdTable,
    path: &[u8],
    cwd: &[u8],
    posix_flags: u32,
    resolve_flags: u32,
    create_mode: Option<u16>,
) -> c_int {
    let flags = posix_to_open_mode(posix_flags);
    if !flags.intersects(OpenMode::READ | OpenMode::WRITE) {
        return Errno::EINVAL.raw() as _;
    }
    if flags.contains(OpenMode::APPEND) && !flags.contains(OpenMode::WRITE) {
        return Errno::EINVAL.raw() as _;
    }

    if path == b"/dev/tty" {
        let tty_idx = match current_task_controlling_tty() {
            Some(idx) => idx,
            None => return Errno::ENXIO.raw(),
        };
        let backing = match tty::open_tty(tty_idx) {
            Ok(b) => b,
            Err(e) => return tty_open_errno(e).raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            tty_idx.0 as usize,
            flags,
            FdFlags::NONE,
            None,
            Some(backing),
            None,
        );
    }

    if path == b"/dev/ptmx" {
        // The `/dev/ptmx` opener is the master, and it pays for the pair's two slots.
        let (master_idx, backing) = match tty::alloc_pty(table.account()) {
            Ok(v) => v,
            Err(_) => return Errno::ENFILE.raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            master_idx.0 as usize,
            flags.with_raw(O_NOCTTY as u32),
            FdFlags::NONE,
            None,
            Some(backing),
            None,
        );
    }

    if let Some(slave_idx) = parse_pts_path(path) {
        let backing = match tty::open_pty_slave(slave_idx) {
            Ok(b) => b,
            Err(e) => return tty_open_errno(e).raw() as _,
        };
        let tty_ops = current_tty_ops();
        return install_fd_entry(
            table,
            tty_ops,
            slave_idx.0 as usize,
            flags,
            FdFlags::NONE,
            Some(slave_idx),
            Some(backing),
            None,
        );
    }

    let create = flags.contains(OpenMode::CREAT);
    let exclusive = (posix_flags & slopos_abi::fs::O_EXCL) != 0;
    let truncate = (posix_flags & slopos_abi::fs::O_TRUNC) != 0;
    let writable = flags.contains(OpenMode::WRITE);
    let open_flags = crate::vfs::ops::VfsOpenFlags {
        create,
        exclusive,
        truncate,
        writable,
    };
    let existed = create_mode.is_none()
        || !create
        || crate::vfs::vfs_stat_at(path, cwd, RESOLVE_FOLLOW).is_ok();
    let directory_only = resolve_flags & RESOLVE_MUST_BE_DIR != 0;
    let vfs_handle = if directory_only {
        None
    } else {
        match vfs_open_handle_flags_at(path, cwd, open_flags, resolve_flags) {
            Ok(h) => Some(h),
            // `open(dir, O_RDONLY)` is how a `dirfd` and `getdents64` are
            // obtained; only a writer is refused a directory.
            Err(Errno::EISDIR) if !writable && !create => None,
            Err(e) => return e.raw() as _,
        }
    };

    let (vfs_handle, dir_path) = match vfs_handle {
        Some(handle) => (handle, None),
        None => {
            if writable || create {
                return Errno::EISDIR.raw() as _;
            }
            match open_directory_handle(path, cwd, resolve_flags) {
                Ok(pair) => pair,
                Err(e) => return e.raw() as _,
            }
        }
    };

    // `create` reports the request, not the outcome, so the pre-open lookup is
    // what separates "created by this call" from "opened something existing".
    if let Some(mode) = create_mode.filter(|_| create && !existed)
        && let Some((fs, inode)) = vfs_file_inode(vfs_handle)
    {
        let _ = fs.set_mode(inode, mode);
    }

    let Some(backing) = vnode_backing(vfs_handle, table.account()) else {
        return Errno::ENFILE.raw() as _;
    };
    install_fd_entry(
        table,
        &VFS_FILE_OPS,
        vfs_handle,
        flags,
        FdFlags::NONE,
        None,
        Some(backing),
        dir_path,
    )
}

/// A directory vnode plus the canonical path a `*at` call resolves against.
#[inline(never)]
fn open_directory_handle(
    path: &[u8],
    cwd: &[u8],
    resolve_flags: u32,
) -> Result<(usize, Option<KVec<u8>>), Errno> {
    let (handle, canon) = vfs_open_dir_handle_at(path, cwd, resolve_flags)?;
    let bytes = canon.as_bytes();
    let mut owned = match KVec::<u8>::zeroed(bytes.len()) {
        Ok(v) => v,
        Err(_) => return Err(Errno::ENOMEM),
    };
    owned.copy_from_slice(bytes);
    Ok((handle, Some(owned)))
}

/// A locked PTY slave reports `EIO`, following Linux devpts behaviour.
fn tty_open_errno(e: TtyError) -> Errno {
    match e {
        TtyError::DeviceBusy => Errno::EBUSY,
        TtyError::PermissionDenied => Errno::EIO,
        TtyError::OutOfMemory => Errno::ENOMEM,
        _ => Errno::ENXIO,
    }
}

pub fn file_read_fd(table: FdTable, fd: c_int, buf: &mut dyn IoBufWrite) -> ssize_t {
    let open_file = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s.open_file,
            None => return Errno::EBADF.raw() as _,
        }
    };
    read_open_file(&open_file, buf, false)
}

/// Holding the `KArc<OpenFile>` keeps the shared-offset update correct if a
/// concurrent close drops the fd alias mid-read.
fn read_open_file(
    open_file: &KArc<OpenFile>,
    buf: &mut dyn IoBufWrite,
    force_nonblock: bool,
) -> ssize_t {
    if !open_file.status_flags().contains(OpenMode::READ) {
        return Errno::EBADF.raw() as _;
    }
    let ops = open_file.ops;

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let used_offset = if seekable { open_file.position() } else { 0 };
    let mut flag_bits = open_file.status_flags().bits();
    let mut socket_guard = None;
    if force_nonblock {
        flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
        socket_guard =
            ForcedNonblockGuard::engage(ops, open_file.handle, open_file.status_flags().bits());
    }
    let rc = ops.read(open_file.handle, buf, used_offset, flag_bits);
    drop(socket_guard);
    if rc > 0 && seekable {
        open_file.position.fetch_add(rc as u64, Ordering::AcqRel);
    }
    rc
}

/// The ring's own reference keeps the description addressable after userland
/// closed the fd.
pub fn file_read_ref_nonblock(file: &FileRef, buf: &mut dyn IoBufWrite) -> ssize_t {
    read_open_file(&file.open_file, buf, true)
}

pub fn file_write_fd(table: FdTable, fd: c_int, buf: &dyn IoBufRead) -> ssize_t {
    let open_file = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s.open_file,
            None => return Errno::EBADF.raw() as _,
        }
    };
    write_open_file(&open_file, buf, false)
}

fn write_open_file(
    open_file: &KArc<OpenFile>,
    buf: &dyn IoBufRead,
    force_nonblock: bool,
) -> ssize_t {
    if !open_file.status_flags().contains(OpenMode::WRITE) {
        return Errno::EBADF.raw() as _;
    }
    let ops = open_file.ops;

    if buf.len() == 0 {
        return 0;
    }

    let seekable = ops.seekable();
    let rc = {
        // Serialises the offset read, the write and the offset advance against
        // another writer sharing this description; an unlocked
        // read-modify-write on `position` lets two writers land on the same
        // offset. A killed task fails the acquire rather than proceeding
        // unserialised.
        let _pos_guard = if seekable {
            match open_file.position_lock.lock() {
                Ok(guard) => Some(guard),
                Err(_) => return Errno::EINTR.raw() as _,
            }
        } else {
            None
        };
        let used_offset = if seekable { open_file.position() } else { 0 };
        let mut flag_bits = open_file.status_flags().bits();
        let mut socket_guard = None;
        if force_nonblock {
            flag_bits |= slopos_abi::syscall::O_NONBLOCK as u32;
            socket_guard =
                ForcedNonblockGuard::engage(ops, open_file.handle, open_file.status_flags().bits());
        }
        let rc = ops.write(open_file.handle, buf, used_offset, flag_bits);
        drop(socket_guard);
        if rc > 0 && seekable {
            open_file.position.fetch_add(rc as u64, Ordering::AcqRel);
        }
        rc
    };

    // A failed commit is reported as this write's error even though the offset
    // already advanced: `O_SYNC` promises durability, not transactionality, and
    // rewinding would misreport bytes the filesystem does hold.
    //
    // `EINVAL` is the exception, because it is `FileOps::sync`'s default: a tty,
    // pipe or socket has no backing store to commit and every write to one is
    // already as durable as it will get. Failing the write there would make
    // `O_SYNC` unusable on a descriptor Linux accepts it on.
    if rc > 0
        && let Some(data_only) = open_sync_policy(open_file.status_flags())
    {
        let sync_rc = ops.sync(open_file.handle, data_only);
        if sync_rc != 0 && sync_rc != Errno::EINVAL.raw() {
            return sync_rc as ssize_t;
        }
    }
    rc
}

pub fn file_write_ref_nonblock(file: &FileRef, buf: &dyn IoBufRead) -> ssize_t {
    write_open_file(&file.open_file, buf, true)
}

/// Forces a socket fd's *stored* nonblocking flag on for a ring probe, then
/// restores it; a no-op for other fds, which honour the per-call `O_NONBLOCK`.
struct ForcedNonblockGuard {
    ops: &'static dyn FileOps,
    handle: usize,
    restore_bits: u32,
}

impl ForcedNonblockGuard {
    fn engage(ops: &'static dyn FileOps, handle: usize, orig_bits: u32) -> Option<Self> {
        if ops.kind() != FileKind::Socket {
            return None;
        }
        ops.set_status_flags(handle, slopos_abi::syscall::O_NONBLOCK as u32);
        Some(Self {
            ops,
            handle,
            restore_bits: orig_bits,
        })
    }
}

impl Drop for ForcedNonblockGuard {
    fn drop(&mut self) {
        self.ops.set_status_flags(self.handle, self.restore_bits);
    }
}

pub fn file_close_fd(table: FdTable, fd: c_int) -> c_int {
    let taken = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return Err(Errno::EBADF);
        }
        match inner.descriptors[fd as usize].take() {
            Some(entry) => Ok(entry),
            None => Err(Errno::EBADF),
        }
    });
    match taken {
        Some(Ok(entry)) => {
            // POSIX: closing *any* descriptor on a file drops this process's
            // record locks on it, whether or not other descriptors remain.
            let key = lock_key_of_entry(&entry);
            drop(entry);
            super::flock::release_record_locks_on_close(table.handle(), key);
            0
        }
        Some(Err(e)) => e.raw() as _,
        None => Errno::ESRCH.raw() as _,
    }
}

/// Holds the open-file reference across the dispatch, as read and write do:
/// `sync` reaches block I/O and may deschedule, so a concurrent `close` must
/// not free the description the handle names.
pub fn file_sync_fd(table: FdTable, fd: c_int, data_only: bool) -> c_int {
    let open_file = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::EBADF.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s.open_file,
            None => return Errno::EBADF.raw() as _,
        }
    };
    open_file.ops.sync(open_file.handle, data_only) as _
}

pub fn file_seek_fd(table: FdTable, fd: c_int, offset: i64, whence: u32) -> i64 {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as i64;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as i64,
        }
    };

    let ops = snap.ops();
    if !ops.seekable() {
        return Errno::ESPIPE.raw() as i64;
    }

    let size = match ops.size(snap.handle()) {
        Some(v) => v as i64,
        None => return Errno::EBADF.raw() as i64,
    };

    let new_pos = match whence as u64 {
        SEEK_SET => offset,
        SEEK_CUR => (snap.position() as i64).saturating_add(offset),
        SEEK_END => size.saturating_add(offset),
        _ => return Errno::EINVAL.raw() as i64,
    };
    if new_pos < 0 {
        return Errno::EINVAL.raw() as i64;
    }

    snap.open_file
        .position
        .store(new_pos as u64, Ordering::Release);
    new_pos
}

pub fn file_get_size_fd(table: FdTable, fd: c_int) -> usize {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return usize::MAX;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return usize::MAX,
        }
    };
    snap.ops()
        .size(snap.handle())
        .map(|v| v as usize)
        .unwrap_or(usize::MAX)
}

pub fn file_unlink_at(path: &[u8], cwd: &[u8]) -> c_int {
    match vfs_unlink_at(path, cwd) {
        Ok(()) => 0,
        Err(VfsError::ReadOnly) => Errno::EROFS.raw() as _,
        Err(VfsError::PermissionDenied) => Errno::EACCES.raw() as _,
        Err(VfsError::IsDirectory) => Errno::EISDIR.raw() as _,
        Err(_) => Errno::ENOENT.raw() as _,
    }
}

pub fn file_rmdir_at(path: &[u8], cwd: &[u8]) -> c_int {
    errno_of(crate::vfs::vfs_rmdir_at(path, cwd))
}

pub fn file_symlink_at(target: &[u8], link_path: &[u8], cwd: &[u8]) -> c_int {
    errno_of(crate::vfs::vfs_symlink_at(target, link_path, cwd))
}

pub fn file_readlink_at(path: &[u8], cwd: &[u8], buf: &mut [u8]) -> isize {
    match crate::vfs::vfs_readlink_at(path, cwd, buf) {
        Ok(n) => n as isize,
        Err(e) => e.to_errno().raw() as isize,
    }
}

pub fn file_truncate_at(path: &[u8], cwd: &[u8], length: u64) -> c_int {
    let resolved = match crate::vfs::path::resolve_path_at(path, cwd, RESOLVE_FOLLOW) {
        Ok(r) => r,
        Err(e) => return e.to_errno().raw() as _,
    };
    truncate_resolved(resolved.fs, resolved.inode, resolved.read_only(), length)
}

/// The shared tail of `truncate(2)` and `ftruncate(2)`.
fn truncate_resolved(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: InodeId,
    read_only: bool,
    length: u64,
) -> c_int {
    let stat = match fs.stat(inode) {
        Ok(s) => s,
        Err(e) => return e.to_errno().raw() as _,
    };
    if stat.file_type == FileType::Directory {
        return Errno::EISDIR.raw() as _;
    }
    if stat.sealed {
        return Errno::EACCES.raw() as _;
    }
    if read_only {
        return Errno::EROFS.raw() as _;
    }
    // A live page set holds pre-truncate pages, and its writeback would put
    // them back over the region this call clears. Forgotten, not flushed: the
    // bytes are the ones the caller asked to discard.
    crate::filemap::forget_inode(fs, inode);
    errno_of(fs.truncate(inode, length))
}

pub fn file_chmod_at(path: &[u8], cwd: &[u8], mode: u16, resolve_flags: u32) -> c_int {
    errno_of(crate::vfs::vfs_set_mode_at(path, cwd, mode, resolve_flags))
}

fn errno_of(result: crate::vfs::VfsResult<()>) -> c_int {
    match result {
        Ok(()) => 0,
        Err(e) => e.to_errno().raw() as _,
    }
}

pub fn file_mkdir_at(path: &[u8], cwd: &[u8]) -> c_int {
    match vfs_mkdir_at(path, cwd, None) {
        Ok(()) => 0,
        Err(VfsError::AlreadyExists) => Errno::EEXIST.raw() as _,
        Err(VfsError::NotFound) => Errno::ENOENT.raw() as _,
        Err(VfsError::NotDirectory) => Errno::ENOTDIR.raw() as _,
        Err(VfsError::PermissionDenied) => Errno::EACCES.raw() as _,
        Err(VfsError::NoSpace) => Errno::ENOSPC.raw() as _,
        Err(VfsError::ReadOnly) => Errno::EROFS.raw() as _,
        Err(_) => Errno::EIO.raw() as _,
    }
}

/// `stat`/`lstat`/`fstatat`. Zeroes `out` first: it is copied to userland by
/// size, so a field left alone is a field of kernel memory handed over.
pub fn file_stat_at(path: &[u8], cwd: &[u8], resolve_flags: u32, out: &mut UserFsStat) -> c_int {
    match crate::vfs::vfs_stat_at(path, cwd, resolve_flags) {
        Ok(stat) => {
            *out = UserFsStat::default();
            stat.fill_user_stat(out);
            0
        }
        Err(e) => e.to_errno().raw() as _,
    }
}

/// `link(2)`/`linkat(2)`.
pub fn file_link_at(
    old_path: &[u8],
    old_cwd: &[u8],
    new_path: &[u8],
    new_cwd: &[u8],
    follow: bool,
) -> c_int {
    errno_of(crate::vfs::vfs_link_at(
        old_path, old_cwd, new_path, new_cwd, follow,
    ))
}

/// `utimensat(2)`. `None` leaves a field alone, which is `UTIME_OMIT`.
pub fn file_utimens_at(
    path: &[u8],
    cwd: &[u8],
    atime: Option<u64>,
    mtime: Option<u64>,
    resolve_flags: u32,
) -> c_int {
    errno_of(crate::vfs::vfs_utimens(
        path,
        cwd,
        atime,
        mtime,
        resolve_flags,
    ))
}

/// `access(2)`/`faccessat(2)`. Identity is uid 0, so only the file's own mode
/// bits and the mount's writability can refuse anything.
pub fn file_access_at(path: &[u8], cwd: &[u8], mode: u32, resolve_flags: u32) -> c_int {
    use slopos_abi::fs::{R_OK, W_OK, X_OK};
    if mode & !(R_OK | W_OK | X_OK) != 0 {
        return Errno::EINVAL.raw() as _;
    }
    let resolved = match crate::vfs::path::resolve_path_at(path, cwd, resolve_flags) {
        Ok(r) => r,
        Err(e) => return e.to_errno().raw() as _,
    };
    let stat = match resolved.fs.stat(resolved.inode) {
        Ok(s) => s,
        Err(e) => return e.to_errno().raw() as _,
    };
    if mode & W_OK != 0 && (resolved.read_only() || stat.sealed) {
        return Errno::EACCES.raw() as _;
    }
    if mode & X_OK != 0 && stat.file_type != FileType::Directory && stat.mode & 0o111 == 0 {
        return Errno::EACCES.raw() as _;
    }
    0
}

/// Paged listing: `cursor` is the ABI-packed resumption point, read and
/// written in place. A caller loops until it comes back
/// [`slopos_abi::fs::FS_LIST_CURSOR_END`].
pub fn file_list_at_from(
    path: &[u8],
    cwd: &[u8],
    entries: &mut [UserFsEntry],
    cursor: &mut u64,
    out_count: &mut u32,
) -> c_int {
    if entries.is_empty() {
        return Errno::EINVAL.raw() as _;
    }
    let mut state = crate::vfs::ListCursor::from_abi(*cursor);
    match crate::vfs::vfs_list_from_at(path, cwd, entries, &mut state) {
        Ok(count) => {
            *out_count = count as u32;
            *cursor = state.to_abi();
            0
        }
        Err(e) => e.to_errno().raw() as _,
    }
}

pub fn file_is_console_fd(table: FdTable, fd: c_int) -> bool {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return false;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return false,
        }
    };
    kind_is_tty(snap.ops().kind())
}

pub fn file_get_tty_index(table: FdTable, fd: c_int) -> Option<TtyIndex> {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return None;
        };
        snapshot_fd(&inner, fd)?
    };
    if snap.ops().kind() == FileKind::Tty {
        Some(TtyIndex(snap.handle() as u8))
    } else {
        None
    }
}

/// Consumes the caller's owning TTY backing, so a failed open is undone by
/// that backing's drop.
pub fn file_open_tty_fd(
    table: FdTable,
    tty_idx: TtyIndex,
    posix_flags: u32,
    backing: KArc<dyn FileBacking>,
) -> c_int {
    let tty_ops = current_tty_ops();
    let base = OpenMode::READ | OpenMode::WRITE;
    let kept = posix_flags & (O_CLOEXEC as u32 | O_NOCTTY as u32 | O_NONBLOCK as u32);
    let flags = if kept != 0 { base.with_raw(kept) } else { base };
    install_fd_entry(
        table,
        tty_ops,
        tty_idx.0 as usize,
        flags,
        FdFlags::NONE,
        Some(tty_idx),
        Some(backing),
        None,
    )
}

pub fn file_pipe_create(
    table: FdTable,
    flags: u32,
    out_read_fd: &mut c_int,
    out_write_fd: &mut c_int,
) -> c_int {
    if flags & !(O_NONBLOCK as u32 | O_CLOEXEC as u32) != 0 {
        return Errno::EINVAL.raw() as _;
    }

    let pipe_handle = match pipe::alloc_slot(table.account()) {
        Some(h) => h,
        None => return Errno::ENOMEM.raw() as _,
    };

    // Prime both ends before wrapping them, so every error path below is a
    // plain drop rather than an explicit free.
    if pipe::with_pipe_mut(pipe_handle, |slot| {
        slot.readers = 1;
        slot.writers = 1;
    })
    .is_none()
    {
        pipe::free_slot(pipe_handle);
        return Errno::ENOMEM.raw() as _;
    }
    let Some((read_backing, write_backing)) = pipe_backings(pipe_handle) else {
        return Errno::ENFILE.raw() as _;
    };

    let nonblock = (flags & O_NONBLOCK as u32) != 0;
    let cloexec = (flags & O_CLOEXEC as u32) != 0;
    let read_flags = if nonblock {
        OpenMode::READ.with_raw(O_NONBLOCK as u32)
    } else {
        OpenMode::READ
    };
    let write_flags = if nonblock {
        OpenMode::WRITE.with_raw(O_NONBLOCK as u32)
    } else {
        OpenMode::WRITE
    };

    let Some(read_of) = new_open_file(
        &PIPE_READ_OPS,
        pipe_handle.as_usize(),
        read_flags,
        0,
        Some(read_backing),
    ) else {
        return Errno::ENFILE.raw() as _;
    };
    let Some(write_of) = new_open_file(
        &PIPE_WRITE_OPS,
        pipe_handle.as_usize(),
        write_flags,
        0,
        Some(write_backing),
    ) else {
        drop(read_of);
        return Errno::ENFILE.raw() as _;
    };

    let account = table.account();
    let Some(mut inner) = lock_table_slot(table) else {
        drop(read_of);
        drop(write_of);
        return Errno::ESRCH.raw() as _;
    };

    let result: Result<(c_int, c_int), Errno> = (|| {
        let read_res = try_charge::<FdSlot>(account, 1).map_err(|_| Errno::EMFILE)?;
        let write_res = try_charge::<FdSlot>(account, 1).map_err(|_| Errno::EMFILE)?;
        let read_idx = find_free_slot(&inner).ok_or(Errno::EMFILE)?;
        inner.descriptors[read_idx] = Some(FdEntry::new(
            read_of.clone(),
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            read_res,
        ));
        let write_idx = match find_free_slot(&inner) {
            Some(idx) => idx,
            None => {
                inner.descriptors[read_idx] = None;
                return Err(Errno::EMFILE);
            }
        };
        inner.descriptors[write_idx] = Some(FdEntry::new(
            write_of.clone(),
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            write_res,
        ));
        Ok((read_idx as c_int, write_idx as c_int))
    })();

    drop(inner);

    match result {
        Ok((r, w)) => {
            *out_read_fd = r;
            *out_write_fd = w;
            drop(read_of);
            drop(write_of);
            0
        }
        Err(e) => {
            drop(read_of);
            drop(write_of);
            e.raw() as _
        }
    }
}

pub fn file_dup_fd(table: FdTable, old_fd: c_int) -> c_int {
    file_dup_fd_min(table, old_fd, 0)
}

fn file_dup_fd_min(table: FdTable, old_fd: c_int, min_fd: usize) -> c_int {
    let account = table.account();
    with_table_slot(table, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Errno::EBADF.raw() as _;
        };
        // `try_alias` refuses a non-duplicable entry, but EMFILE would be the
        // wrong answer for it: distinguish before charging.
        if !src.rights.duplicate {
            return Errno::EINVAL.raw() as _;
        }
        let Some(mut alias) = src.try_alias(account) else {
            return Errno::EMFILE.raw() as _;
        };
        // `cloexec` is a preference on the fd number, so a dup starts it clear;
        // `close_on_fork` names the description and carries over.
        alias.cloexec = false;

        let Some(new_idx) = find_free_slot_from(inner, min_fd) else {
            return Errno::EMFILE.raw() as _;
        };

        inner.descriptors[new_idx] = Some(alias);
        new_idx as c_int
    })
    .unwrap_or(Errno::ESRCH.raw() as _)
}

pub fn file_dup2_fd(table: FdTable, old_fd: c_int, new_fd: c_int) -> c_int {
    dup_into(table, old_fd, new_fd, false, false)
}

pub fn file_dup3_fd(table: FdTable, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return Errno::EINVAL.raw() as _;
    }
    dup_into(
        table,
        old_fd,
        new_fd,
        (flags & FD_CLOEXEC as u32) != 0,
        true,
    )
}

/// dup2 with `old_fd == new_fd` is a validity check (no-op success); dup3
/// forbids it (handled by the caller).
fn dup_into(table: FdTable, old_fd: c_int, new_fd: c_int, cloexec: bool, is_dup3: bool) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    if old_fd == new_fd && !is_dup3 {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as _;
        };
        return if get_fd_entry(&inner, old_fd).is_some() {
            new_fd
        } else {
            Errno::EBADF.raw() as _
        };
    }

    let account = table.account();
    let outcome = with_table_slot(table, |inner| {
        let Some(src) = get_fd_entry(inner, old_fd) else {
            return Err(Errno::EBADF);
        };
        if !src.rights.duplicate {
            return Err(Errno::EINVAL);
        }
        // Only a *free* target needs a fresh charge; an occupied one reuses the
        // displaced entry's, keeping exactly one charge per number at all times.
        let occupied = inner.descriptors[new_fd as usize].is_some();
        let alias = if occupied {
            None
        } else {
            match src.try_alias(account) {
                Some(alias) => Some(alias),
                None => return Err(Errno::EMFILE),
            }
        };
        let open_file = src.open_file.clone();
        let close_on_fork = src.close_on_fork;

        let displaced = inner.descriptors[new_fd as usize].take();
        let (mut entry, released) = match (alias, displaced) {
            (Some(alias), _) => (alias, None),
            (None, Some(previous)) => {
                let (entry, released) = previous.replacing(open_file, close_on_fork);
                (entry, Some(released))
            }
            (None, None) => return Err(Errno::EMFILE),
        };
        entry.cloexec = cloexec;
        inner.descriptors[new_fd as usize] = Some(entry);
        Ok(released)
    });

    match outcome {
        Some(Ok(displaced)) => {
            // `dup2`/`dup3` close whatever held the target number, and a close
            // drops this process's record locks on that file.
            let key = displaced
                .as_ref()
                .map(|previous| lock_key_of(previous.ops, previous.handle));
            drop(displaced);
            if let Some(key) = key {
                super::flock::release_record_locks_on_close(table.handle(), key);
            }
            new_fd
        }
        Some(Err(e)) => e.raw() as _,
        None => Errno::ESRCH.raw() as _,
    }
}

pub fn file_fcntl_fd(table: FdTable, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(table, fd, arg as usize) as i64,
        F_GETFD => {
            let Some(inner) = lock_table_slot(table) else {
                return Errno::ESRCH.raw() as i64;
            };
            match get_fd_entry(&inner, fd) {
                Some(entry) => {
                    if entry.cloexec {
                        FD_CLOEXEC as i64
                    } else {
                        0
                    }
                }
                None => Errno::EBADF.raw() as i64,
            }
        }
        F_SETFD => {
            let Some(mut inner) = lock_table_slot(table) else {
                return Errno::ESRCH.raw() as i64;
            };
            match get_fd_entry_mut(&mut inner, fd) {
                Some(entry) => {
                    entry.cloexec = (arg & FD_CLOEXEC) != 0;
                    0
                }
                None => Errno::EBADF.raw() as i64,
            }
        }
        F_GETFL => {
            let snap = {
                let Some(inner) = lock_table_slot(table) else {
                    return Errno::ESRCH.raw() as i64;
                };
                match snapshot_fd(&inner, fd) {
                    Some(s) => s,
                    None => return Errno::EBADF.raw() as i64,
                }
            };
            openmode_to_posix_bits(snap.status_flags()) as i64
        }
        F_SETFL => {
            let snap = {
                let Some(inner) = lock_table_slot(table) else {
                    return Errno::ESRCH.raw() as i64;
                };
                match snapshot_fd(&inner, fd) {
                    Some(s) => s,
                    None => return Errno::EBADF.raw() as i64,
                }
            };
            let posix_arg = arg as u32;
            let current = snap.status_flags();
            let mode_bits = current & (OpenMode::READ | OpenMode::WRITE);
            let mut next_flags = mode_bits;
            if posix_arg & slopos_abi::fs::O_APPEND != 0 {
                next_flags |= OpenMode::APPEND;
            }
            let mut raw = current.bits() & (O_NOCTTY as u32);
            if posix_arg & O_NONBLOCK as u32 != 0 {
                raw |= O_NONBLOCK as u32;
            }
            let next_flags = next_flags.with_raw(raw);
            snap.open_file.set_status_flags(next_flags);
            let _ = snap
                .ops()
                .set_status_flags(snap.handle(), openmode_to_posix_bits(next_flags));
            0
        }
        _ => Errno::EINVAL.raw() as i64,
    }
}

pub fn file_fstat_fd(
    table: FdTable,
    fd: c_int,
    out_stat: &mut slopos_abi::fs::UserFsStat,
) -> c_int {
    let snap = {
        let Some(inner) = lock_table_slot(table) else {
            return Errno::ESRCH.raw() as _;
        };
        match snapshot_fd(&inner, fd) {
            Some(s) => s,
            None => return Errno::EBADF.raw() as _,
        }
    };
    snap.ops().stat(snap.handle(), out_stat)
}

/// `fstatfs(2)`: the capacity of the filesystem the descriptor's file lives
/// on.
///
/// A pipe, a tty or a socket answers `ENOSYS` rather than a fabricated
/// geometry — the same refusal [`crate::vfs::FileSystem::statfs`] defaults to.
pub fn file_statfs_fd(table: FdTable, fd: c_int) -> Result<FsStats, Errno> {
    let snap = {
        let inner = lock_table_slot(table).ok_or(Errno::ESRCH)?;
        snapshot_fd(&inner, fd).ok_or(Errno::EBADF)?
    };
    if snap.ops().kind() != FileKind::Regular {
        return Err(Errno::ENOSYS);
    }
    // Off the table lock: a filesystem's `statfs` may take a sleeping mutex.
    match vfs_file_statfs(snap.handle()) {
        Some(result) => result.map_err(|e| e.to_errno()),
        None => Err(Errno::EBADF),
    }
}

fn snapshot(table: FdTable, fd: c_int) -> Result<FdSnapshot, Errno> {
    let inner = lock_table_slot(table).ok_or(Errno::ESRCH)?;
    snapshot_fd(&inner, fd).ok_or(Errno::EBADF)
}

/// Run `f` with the canonical path a directory descriptor was opened on — the
/// base a `*at` syscall resolves a relative path against.
///
/// `ENOTDIR` for a descriptor that is not a directory. `ESTALE` once that path
/// no longer names the descriptor's own inode: the base is a path, so without
/// the re-check a concurrent `rename("/tmp/a", "/tmp/old"); mkdir("/tmp/a")`
/// silently redirects every later `*at` call into the new directory.
pub fn with_fd_dir_path<R>(
    table: FdTable,
    fd: c_int,
    f: impl FnOnce(&[u8]) -> R,
) -> Result<R, Errno> {
    let snap = snapshot(table, fd)?;
    let path = snap.open_file.dir_base().ok_or(Errno::ENOTDIR)?;
    if !vfs_dir_handle_still_names(snap.handle(), path) {
        return Err(Errno::ESTALE);
    }
    Ok(f(path))
}

/// `pread64(2)`. The offset is the caller's, so neither the description's
/// position nor its lock is touched: threads sharing a descriptor can overlap.
pub fn file_pread_fd(table: FdTable, fd: c_int, buf: &mut dyn IoBufWrite, offset: u64) -> ssize_t {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    if !snap.status_flags().contains(OpenMode::READ) {
        return Errno::EBADF.raw() as _;
    }
    if !snap.ops().seekable() {
        return Errno::ESPIPE.raw() as _;
    }
    if buf.is_empty() {
        return 0;
    }
    let flags = snap.status_flags().bits();
    snap.ops().read(snap.handle(), buf, offset, flags)
}

/// `pwrite64(2)`. `O_APPEND` is stripped: POSIX has the explicit offset win,
/// and the append path would resolve the offset from the file's size instead.
pub fn file_pwrite_fd(table: FdTable, fd: c_int, buf: &dyn IoBufRead, offset: u64) -> ssize_t {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    if !snap.status_flags().contains(OpenMode::WRITE) {
        return Errno::EBADF.raw() as _;
    }
    if !snap.ops().seekable() {
        return Errno::ESPIPE.raw() as _;
    }
    if buf.is_empty() {
        return 0;
    }
    let status = snap.status_flags();
    let flags = status.bits() & !slopos_abi::fs::O_APPEND;
    let rc = snap.ops().write(snap.handle(), buf, offset, flags);
    if rc > 0
        && let Some(data_only) = open_sync_policy(status)
    {
        let sync_rc = snap.ops().sync(snap.handle(), data_only);
        if sync_rc != 0 && sync_rc != Errno::EINVAL.raw() {
            return sync_rc as ssize_t;
        }
    }
    rc
}

/// The `(filesystem, inode)` behind a descriptor. `EBADF` when there is none —
/// a pipe, a tty, a socket.
fn fd_vnode(snap: &FdSnapshot) -> Result<(&'static dyn crate::vfs::FileSystem, InodeId), Errno> {
    if snap.ops().kind() != FileKind::Regular {
        return Err(Errno::EINVAL);
    }
    vfs_file_inode(snap.handle()).ok_or(Errno::EBADF)
}

/// Bytes a `getdents64` record occupies: the header, the name, its NUL, and
/// padding to the 8-byte alignment the next header needs.
fn dirent_reclen(name_len: usize) -> usize {
    (core::mem::size_of::<UserDirent64>() + name_len + 1).next_multiple_of(8)
}

/// `getdents64(2)`: pack directory entries into `out` from the cursor this
/// description carries in `position`, and answer `(bytes_written, next_cookie)`.
///
/// The cursor is deliberately **not** advanced here: the handler copies `out`
/// to userland afterwards, and a fault there would otherwise lose a whole
/// batch — the caller retries and skips every entry already consumed.
/// [`file_getdents_commit_fd`] is the second half.
///
/// `EINVAL` when `out` cannot hold even the first record, as Linux does: a
/// zero return means end of directory.
pub fn file_getdents_fd(table: FdTable, fd: c_int, out: &mut [u8]) -> Result<(usize, u64), Errno> {
    let snap = snapshot(table, fd)?;
    let (fs, inode) = fd_vnode(&snap).map_err(|_| Errno::ENOTDIR)?;
    match fs.stat(inode) {
        Ok(stat) if stat.file_type == FileType::Directory => {}
        Ok(_) => return Err(Errno::ENOTDIR),
        Err(e) => return Err(e.to_errno()),
    }

    // Held across the cookie read and the walk, so a concurrent commit on a
    // shared description cannot move the cursor mid-batch.
    let Ok(_cursor_guard) = snap.open_file.position_lock.lock() else {
        return Err(Errno::EINTR);
    };
    let cookie = snap.open_file.position();

    let mut written = 0usize;
    let mut resume = cookie;
    let mut ran_out = false;
    let walk = fs.readdir_cookie(inode, cookie, &mut |next, name, ino, file_type| {
        let reclen = dirent_reclen(name.len());
        if written + reclen > out.len() {
            ran_out = true;
            return false;
        }
        let record = &mut out[written..written + reclen];
        record.fill(0);
        record[0..8].copy_from_slice(&ino.to_le_bytes());
        record[8..16].copy_from_slice(&(next as i64).to_le_bytes());
        record[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        record[18] = file_type.to_dt();
        let head_len = core::mem::size_of::<UserDirent64>();
        record[head_len..head_len + name.len()].copy_from_slice(name);
        written += reclen;
        resume = next;
        true
    });

    if let Err(e) = walk {
        return Err(e.to_errno());
    }
    if written == 0 && ran_out {
        return Err(Errno::EINVAL);
    }
    Ok((written, resume))
}

/// Commit the cookie [`file_getdents_fd`] returned, once its bytes have
/// reached userland.
///
/// Advance only: two readers sharing one description start from the same
/// cookie, and a plain store would let the slower one's commit rewind the walk
/// and repeat entries without bound. `lseek` may still rewind — `rewinddir(3)`.
pub fn file_getdents_commit_fd(table: FdTable, fd: c_int, cookie: u64) -> Result<(), Errno> {
    let snap = snapshot(table, fd)?;
    snap.open_file
        .position
        .fetch_max(cookie, core::sync::atomic::Ordering::AcqRel);
    Ok(())
}

/// `fchmod(2)`.
pub fn file_fchmod_fd(table: FdTable, fd: c_int, mode: u16) -> c_int {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    let (fs, inode) = match fd_vnode(&snap) {
        Ok(v) => v,
        Err(e) => return e.raw() as _,
    };
    if let Err(e) = fd_writable_fs(fs) {
        return e.raw() as _;
    }
    match fs.stat(inode) {
        Ok(stat) if stat.sealed => return Errno::EACCES.raw() as _,
        Ok(_) => {}
        Err(e) => return e.to_errno().raw() as _,
    }
    errno_of(fs.set_mode(inode, mode))
}

/// The read-only check the fd-shaped mutators owe.
///
/// A read-only *mount* is not reachable from a descriptor — it records
/// `(filesystem, inode)` — and an `O_RDONLY` open never checked one either, so
/// the filesystem's own flag is what is enforced here.
fn fd_writable_fs(fs: &'static dyn crate::vfs::FileSystem) -> Result<(), Errno> {
    if fs.statfs().map(|s| s.read_only).unwrap_or(false) {
        return Err(Errno::EROFS);
    }
    Ok(())
}

/// `ftruncate(2)` on a regular file. The memfd form lives in `slopos_mm`:
/// there the length is an allocation size, here it is a file size.
pub fn file_ftruncate_fd(table: FdTable, fd: c_int, length: u64) -> c_int {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    if !snap.status_flags().contains(OpenMode::WRITE) {
        return Errno::EINVAL.raw() as _;
    }
    let (fs, inode) = match fd_vnode(&snap) {
        Ok(v) => v,
        Err(e) => return e.raw() as _,
    };
    let read_only = fs.statfs().map(|s| s.read_only).unwrap_or(false);
    truncate_resolved(fs, inode, read_only, length)
}

/// `utimensat(dirfd, NULL, ..)`: the descriptor names the file directly.
pub fn file_set_times_fd(
    table: FdTable,
    fd: c_int,
    atime: Option<u64>,
    mtime: Option<u64>,
) -> c_int {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    let (fs, inode) = match fd_vnode(&snap) {
        Ok(v) => v,
        Err(e) => return e.raw() as _,
    };
    if let Err(e) = fd_writable_fs(fs) {
        return e.raw() as _;
    }
    errno_of(crate::vfs::vfs_set_times(fs, inode, atime, mtime))
}

/// The lock table's key for the file behind a descriptor.
///
/// Must be read before a descriptor's teardown: the `(fs, inode)` half comes
/// from the vnode the teardown releases.
fn lock_key_of(ops: &'static dyn FileOps, handle: usize) -> LockFile {
    if ops.kind() == FileKind::Regular
        && let Some((fs, inode)) = vfs_file_inode(handle)
    {
        let addr = fs as *const dyn crate::vfs::FileSystem as *const () as usize as u64;
        return LockFile::inode(addr, inode);
    }
    LockFile::handle(ops.kind() as u8, handle as u64)
}

fn lock_key(snap: &FdSnapshot) -> LockFile {
    lock_key_of(snap.ops(), snap.handle())
}

/// [`lock_key`] for the close paths, which hold an [`FdEntry`] rather than a
/// snapshot.
pub(super) fn lock_key_of_entry(entry: &FdEntry) -> LockFile {
    lock_key_of(entry.open_file.ops, entry.open_file.handle)
}

/// [`lock_key`] for a fixture that has to build the same key by hand.
#[cfg(feature = "tests")]
pub(super) fn lock_key_for_test(table: FdTable, fd: c_int) -> Option<LockFile> {
    snapshot(table, fd).ok().map(|snap| lock_key(&snap))
}

/// `flock(2)`. The lock belongs to the open file description, so it survives
/// `dup` and `fork`; the description's teardown releases it.
pub fn file_flock_fd(table: FdTable, fd: c_int, operation: u32) -> c_int {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as _,
    };
    let key = lock_key(&snap);
    let id = snap.open_file.id;
    // Every reference to the descriptor table is released before the wait.
    drop(snap);
    match file_lock_flock(key, id, table, operation) {
        Ok(()) => 0,
        Err(e) => e.raw() as _,
    }
}

/// `fcntl(2)`'s `F_GETLK`/`F_SETLK`/`F_SETLKW`. The caller has already copied
/// `lock` in from userland; `F_GETLK` writes the holder back into it.
///
/// `F_RDLCK` needs a readable descriptor and `F_WRLCK` a writable one, or
/// `fcntl(2)` answers `EBADF`. `F_GETLK` only reads, so it is exempt.
pub fn file_fcntl_lock_fd(table: FdTable, fd: c_int, cmd: u64, lock: &mut UserFlock) -> i64 {
    let snap = match snapshot(table, fd) {
        Ok(s) => s,
        Err(e) => return e.raw() as i64,
    };
    if matches!(cmd, F_SETLK | F_SETLKW) {
        let need = match lock.l_type {
            F_RDLCK => Some(OpenMode::READ),
            F_WRLCK => Some(OpenMode::WRITE),
            _ => None,
        };
        if let Some(need) = need
            && !snap.status_flags().contains(need)
        {
            return Errno::EBADF.raw() as i64;
        }
    }
    let key = lock_key(&snap);
    let base = match lock.l_whence as u64 {
        SEEK_SET => 0i64,
        SEEK_CUR => snap.position() as i64,
        SEEK_END => match snap.ops().size(snap.handle()) {
            Some(size) => size as i64,
            None => return Errno::EINVAL.raw() as i64,
        },
        _ => return Errno::EINVAL.raw() as i64,
    };
    drop(snap);

    let Some(start) = base.checked_add(lock.l_start) else {
        return Errno::EINVAL.raw() as i64;
    };
    // A negative length names the range ending at `start`, per POSIX.
    let (start, end) = if lock.l_len == 0 {
        (start, i64::MAX)
    } else if lock.l_len > 0 {
        match start.checked_add(lock.l_len) {
            Some(end) => (start, end),
            None => return Errno::EINVAL.raw() as i64,
        }
    } else {
        match start.checked_add(lock.l_len) {
            Some(begin) => (begin, start),
            None => return Errno::EINVAL.raw() as i64,
        }
    };
    if start < 0 || end <= start {
        return Errno::EINVAL.raw() as i64;
    }
    let end = if end == i64::MAX {
        u64::MAX
    } else {
        end as u64
    };

    match file_lock_record(key, table, cmd, start as u64, end, lock) {
        Ok(()) => 0,
        Err(e) => e.raw() as i64,
    }
}

pub fn fileio_open_socket_fd(
    table: FdTable,
    socket_idx: u32,
    backing: Option<KArc<dyn FileBacking>>,
) -> i32 {
    let Some(socket_ops) = current_socket_ops() else {
        return Errno::ENOTSOCK.raw() as _;
    };
    install_fd_entry(
        table,
        socket_ops,
        socket_idx as usize,
        OpenMode::READ | OpenMode::WRITE,
        FdFlags::NONE,
        None,
        backing,
        None,
    )
}

/// `fd_flags` is mandatory: an fd minted from kernel-side ops carries no
/// POSIX open flags, so its inheritance policy has no other source.
pub fn fileio_open_fd_with_ops(
    table: FdTable,
    ops: &'static dyn FileOps,
    handle: usize,
    backing: Option<KArc<dyn FileBacking>>,
    fd_flags: FdFlags,
) -> i32 {
    install_fd_entry(
        table,
        ops,
        handle,
        OpenMode::READ | OpenMode::WRITE,
        fd_flags,
        None,
        backing,
        None,
    )
}

pub fn fileio_get_open_file_handle(table: FdTable, fd: i32) -> Option<(FileKind, usize, OpenMode)> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.ops().kind(), snap.handle(), snap.status_flags()))
}

/// Confers no ownership: the caller's own fd keeps the file alive for the
/// operation.
pub fn fileio_get_handle_and_ops(table: FdTable, fd: i32) -> Option<(usize, &'static dyn FileOps)> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    Some((snap.handle(), snap.ops()))
}

pub fn fileio_handle_and_ops_from_ref(file: &FileRef) -> (usize, &'static dyn FileOps) {
    (file.open_file.handle, file.open_file.ops)
}

/// Mint a [`FileRef`] alias of an open fd — the SCM_RIGHTS send side. The
/// alias keeps the description alive until dropped or installed.
///
/// Refuses a non-transferable kind. This is the choke point every duplication
/// path funnels through — SCM_RIGHTS, the spawn `CloneFd`/`TransferFd` arms,
/// and the ring's fd resolution — so the predicate is tested once, here,
/// rather than at each caller.
pub fn fileio_clone_file_ref(table: FdTable, fd: i32) -> Option<FileRef> {
    let snap = {
        let inner = lock_table_slot(table)?;
        snapshot_fd(&inner, fd)?
    };
    // The entry's stamped right, not a re-derivation from the kind: rights
    // travel with the entry so a descriptor cannot regain them by being looked
    // up somewhere more permissive.
    if !snap.rights.transfer {
        return None;
    }
    Some(FileRef {
        open_file: snap.open_file,
    })
}

/// Install a received [`FileRef`] — the SCM_RIGHTS receive side. On failure
/// the alias drops here, closing it.
pub fn fileio_install_file_ref(table: FdTable, file: FileRef) -> c_int {
    // The receiver pays for the number; the sender's in-flight custody charge
    // is released by the queue that held it.
    let Ok(reservation) = try_charge::<FdSlot>(table.account(), 1) else {
        drop(file);
        return Errno::EMFILE.raw() as _;
    };
    let Some(mut inner) = lock_table_slot(table) else {
        return Errno::ESRCH.raw() as _;
    };
    let Some(idx) = find_free_slot(&inner) else {
        drop(inner);
        drop(file);
        return Errno::EMFILE.raw() as _;
    };
    inner.descriptors[idx] = Some(FdEntry::new(file.open_file, FdFlags::NONE, reservation));
    idx as c_int
}

/// Install a [`FileRef`] at exactly `target_fd`, displacing any occupant. On
/// failure the alias drops here, closing it.
pub fn fileio_install_file_ref_at(
    table: FdTable,
    target_fd: c_int,
    file: FileRef,
    cloexec: bool,
) -> c_int {
    if target_fd < 0 || target_fd as usize >= FILEIO_MAX_OPEN_FILES {
        drop(file);
        return Errno::EBADF.raw() as _;
    }
    let account = table.account();
    let displaced = {
        let Some(mut inner) = lock_table_slot(table) else {
            drop(file);
            return Errno::ESRCH.raw() as _;
        };
        // Displace first: the charge below is then only for a new number.
        let displaced = inner.descriptors[target_fd as usize].take();
        let Ok(reservation) = try_charge::<FdSlot>(account, 1) else {
            inner.descriptors[target_fd as usize] = displaced;
            drop(inner);
            drop(file);
            return Errno::EMFILE.raw() as _;
        };
        inner.descriptors[target_fd as usize] = Some(FdEntry::new(
            file.open_file,
            FdFlags {
                cloexec,
                close_on_fork: false,
            },
            reservation,
        ));
        displaced
    };
    // Displacing a descriptor number is a close; see `dup_into`.
    let key = displaced.as_ref().map(lock_key_of_entry);
    drop(displaced);
    if let Some(key) = key {
        super::flock::release_record_locks_on_close(table.handle(), key);
    }
    target_fd
}

/// Detach the description at `fd` — the spawn `TransferFd` move.
///
/// Refuses a non-transferable kind, leaving the descriptor in place: a seat
/// moved into another process would leave the arbiter naming a task that no
/// longer holds it.
pub fn fileio_take_file_ref(table: FdTable, fd: c_int) -> Option<FileRef> {
    let entry = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return None;
        }
        let held = inner.descriptors[fd as usize].as_ref()?;
        if !held.rights.transfer {
            return None;
        }
        inner.descriptors[fd as usize].take()
    })??;
    Some(FileRef {
        open_file: entry.open_file,
    })
}

/// Detaches only while the slot still holds `expected`'s description; a slot
/// the owner concurrently closed or repopulated is left untouched.
pub fn fileio_take_file_ref_matching(
    table: FdTable,
    fd: c_int,
    expected: &FileRef,
) -> Option<FileRef> {
    let entry = with_table_slot(table, |inner| {
        if fd < 0 || fd as usize >= FILEIO_MAX_OPEN_FILES {
            return None;
        }
        let held = inner.descriptors[fd as usize].as_ref()?;
        if !KArc::ptr_eq(&held.open_file, &expected.open_file) {
            return None;
        }
        inner.descriptors[fd as usize].take()
    })??;
    Some(FileRef {
        open_file: entry.open_file,
    })
}

/// Open `path` at exactly `target_fd`, displacing any occupant — the spawn
/// `Open` action. The inherited fd is never close-on-exec.
pub fn fileio_open_at_fd(table: FdTable, target_fd: c_int, path: &[u8], posix_flags: u32) -> c_int {
    if target_fd < 0 || target_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return Errno::EBADF.raw() as _;
    }
    let opened = file_open_for_process(table, path, posix_flags & !(O_CLOEXEC as u32));
    if opened < 0 {
        return opened;
    }
    if opened == target_fd {
        return target_fd;
    }
    let rc = file_dup2_fd(table, opened, target_fd);
    let _ = file_close_fd(table, opened);
    if rc < 0 { rc } else { target_fd }
}
