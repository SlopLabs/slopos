//! `dirfd` resolution for the `*at(2)` family.
//!
//! A directory descriptor carries the canonical path it was opened on, and
//! that path is the base. Walking from the descriptor's inode instead would
//! need `..` answerable from an inode alone, which no filesystem here can do.

use slopos_abi::Errno;
use slopos_abi::fs::{AT_FDCWD, AT_SYMLINK_NOFOLLOW, O_DIRECTORY, O_NOFOLLOW};
use slopos_fs::fileio::{FdTable, with_fd_dir_path};
use slopos_fs::vfs::path::{RESOLVE_FOLLOW, RESOLVE_MUST_BE_DIR, RESOLVE_NOFOLLOW_FINAL};

use crate::syscall::context::SyscallContext;

/// Run `f` with the absolute directory `path` is taken against.
pub fn with_dir_base<R>(
    ctx: &SyscallContext<'_>,
    table: FdTable,
    dirfd: i32,
    path: &[u8],
    f: impl FnOnce(&[u8]) -> R,
) -> Result<R, Errno> {
    // Linux never reads `dfd` for an absolute path and libc wrappers rely on
    // it: resolving the descriptor here would make a junk one beside an
    // absolute name `EBADF`.
    if path.first() == Some(&b'/') {
        return Ok(f(b"/"));
    }
    if dirfd == AT_FDCWD {
        return Ok(with_cwd_base(ctx, f));
    }
    if dirfd < 0 {
        return Err(Errno::EBADF);
    }
    with_fd_dir_path(table, dirfd, f)
}

/// Run `f` with the caller's working directory. The slice is NUL-terminated;
/// `canonicalise_at` trims it.
pub fn with_cwd_base<R>(ctx: &SyscallContext<'_>, f: impl FnOnce(&[u8]) -> R) -> R {
    ctx.with_cwd(f)
}

/// The resolve flags an `*at` call's own flag word and path spelling imply.
///
/// A trailing slash has to be read here — canonicalisation drops it as an
/// empty component — and POSIX makes it assert a directory, so a final
/// symlink is followed whatever `AT_SYMLINK_NOFOLLOW` asked for.
pub fn resolve_flags_from(at_flags: u32, path: &[u8]) -> u32 {
    if names_directory(path) {
        return RESOLVE_MUST_BE_DIR;
    }
    if at_flags & AT_SYMLINK_NOFOLLOW != 0 {
        RESOLVE_NOFOLLOW_FINAL
    } else {
        RESOLVE_FOLLOW
    }
}

pub fn open_resolve_flags(open_flags: u32, path: &[u8]) -> u32 {
    if open_flags & O_DIRECTORY != 0 {
        RESOLVE_MUST_BE_DIR
    } else if open_flags & O_NOFOLLOW != 0 {
        resolve_flags_from(AT_SYMLINK_NOFOLLOW, path)
    } else {
        resolve_flags_from(0, path)
    }
}

#[inline]
pub fn names_directory(path: &[u8]) -> bool {
    path.last() == Some(&b'/')
}

/// Refuse a trailing slash the operation cannot carry into the walk: the
/// parent-resolving mutators take no resolve flags, so `unlink("file/")`
/// would otherwise remove `file`. Every other outcome is the mutator's own.
#[inline(never)]
pub fn reject_non_directory(path: &[u8], cwd: &[u8]) -> Result<(), Errno> {
    if !names_directory(path) {
        return Ok(());
    }
    match slopos_fs::vfs::vfs_stat_at(path, cwd, RESOLVE_MUST_BE_DIR) {
        Err(slopos_fs::VfsError::NotDirectory) => Err(Errno::ENOTDIR),
        _ => Ok(()),
    }
}
