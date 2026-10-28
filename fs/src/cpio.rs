//! The initramfs's `newc` cpio archive, read through `slopos_boot_core::newc`
//! — the reading `bootctl` gives a base too — and unpacked into the root.

use slopos_boot_core::newc;
use slopos_ostd::{KVec, klog_info};

use crate::vfs::{VfsError, VfsOpenFlags, vfs_mkdir, vfs_open_flags, vfs_set_mode, vfs_symlink};
use crate::{MAX_NAME_LEN, MAX_PATH_LEN};
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpioError {
    /// The archive ended before a record (header/name/data) was complete.
    Truncated,
    /// A record header did not start with the `070701` magic.
    BadMagic,
    /// A header field was not valid ASCII-hex.
    BadField,
    /// A path component exceeded [`MAX_NAME_LEN`] (or the path [`MAX_PATH_LEN`]).
    NameTooLong,
    /// A path climbs with `..`, or passes through a file as if it were a
    /// directory, or an entry changes what kind of file a path names.
    BadPath,
    NoMemory,
    /// The base was indexed once already.
    Installed,
    /// A VFS operation failed while materializing an entry.
    Vfs(VfsError),
}

impl From<newc::Error> for CpioError {
    fn from(e: newc::Error) -> Self {
        match e {
            newc::Error::Truncated => CpioError::Truncated,
            newc::Error::BadMagic => CpioError::BadMagic,
            newc::Error::BadField => CpioError::BadField,
        }
    }
}

/// The stored name is NUL-terminated within its `namesize` field.
pub(crate) fn nul_terminated(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

/// Walk every record of a `newc` cpio archive, invoking `f` for each entry up
/// to (but not including) the `TRAILER!!!` sentinel. Returns the number of
/// entries visited.
pub fn for_each_cpio_entry<'a, F>(archive: &'a [u8], mut f: F) -> Result<usize, CpioError>
where
    F: FnMut(&newc::Entry<'a>) -> Result<(), CpioError>,
{
    let mut count = 0usize;
    for entry in newc::entries(archive) {
        f(&entry?)?;
        count += 1;
    }
    Ok(count)
}

fn in_base(path: &[u8]) -> bool {
    slopos_abi::fs::BASE_DIRS.iter().any(|dir| {
        path.starts_with(dir.as_bytes()) && (path.len() == dir.len() || path[dir.len()] == b'/')
    })
}

/// Unpack what a `newc` cpio archive holds outside the base directories into
/// the currently mounted root filesystem, through the VFS; the base's own are
/// served from the archive by [`crate::basefs`]. Returns the number of entries
/// materialized (directories, regular files and symlinks).
pub fn unpack_cpio_into_root(archive: &[u8]) -> Result<usize, CpioError> {
    let mut created = 0usize;
    // One buffer for the whole archive: `MAX_PATH_LEN` is 4096 and a kernel
    // frame is bounded at 2 KiB.
    let mut buf = KVec::<u8>::zeroed(MAX_PATH_LEN).map_err(|_| CpioError::NameTooLong)?;

    for_each_cpio_entry(archive, |entry| {
        let len = match normalize_path(entry.path, buf.as_mut_slice())? {
            Some(len) => len,
            None => return Ok(()),
        };
        let path = &buf.as_slice()[..len];
        validate_components(path)?;
        if in_base(path) {
            return Ok(());
        }

        match entry.mode & S_IFMT {
            S_IFDIR => {
                ensure_parents(path)?;
                match vfs_mkdir(path) {
                    Ok(()) => {
                        vfs_set_mode(path, (entry.mode & 0o7777) as u16).map_err(CpioError::Vfs)?
                    }
                    Err(VfsError::AlreadyExists) => {}
                    Err(e) => return Err(CpioError::Vfs(e)),
                }
                created += 1;
            }
            S_IFREG => {
                ensure_parents(path)?;
                write_file(path, entry.data, entry.mode)?;
                created += 1;
            }
            // `newc` stores a symlink's target in the entry body; dropping
            // these left an initramfs with a `/lib -> /usr/lib` link unbootable.
            S_IFLNK => {
                ensure_parents(path)?;
                make_symlink(path, entry.data)?;
                created += 1;
            }
            other => {
                klog_info!("initramfs: skipping unsupported entry (mode {:#o})", other);
            }
        }
        Ok(())
    })
    .map(|_| created)
}

/// Normalize a cpio entry name into an absolute VFS path written into `out`.
/// Strips a leading `./` and collapses leading slashes to exactly one. Returns
/// the byte length written, `None` for the root / `.`, or [`CpioError::NameTooLong`].
fn normalize_path(name: &[u8], out: &mut [u8]) -> Result<Option<usize>, CpioError> {
    let mut n = name;
    if n == b"." {
        return Ok(None);
    }
    if n.len() >= 2 && n[0] == b'.' && n[1] == b'/' {
        n = &n[2..];
    }
    while n.first() == Some(&b'/') {
        n = &n[1..];
    }
    if n.is_empty() {
        return Ok(None);
    }
    let total = n.len().checked_add(1).ok_or(CpioError::NameTooLong)?;
    if total > out.len() {
        return Err(CpioError::NameTooLong);
    }
    out[0] = b'/';
    out[1..total].copy_from_slice(n);
    Ok(Some(total))
}

/// The target is the entry body, NUL-terminated by some writers and not by
/// others.
fn make_symlink(path: &[u8], target: &[u8]) -> Result<(), CpioError> {
    let target = nul_terminated(target);
    if target.is_empty() {
        return Err(CpioError::BadField);
    }
    match vfs_symlink(target, path) {
        Ok(()) | Err(VfsError::AlreadyExists) => Ok(()),
        Err(e) => Err(CpioError::Vfs(e)),
    }
}

/// Verify every path component fits in [`MAX_NAME_LEN`] so the VFS does not
/// silently truncate a name (which would corrupt the directory tree).
fn validate_components(path: &[u8]) -> Result<(), CpioError> {
    let mut comp_len = 0usize;
    for &b in &path[1..] {
        if b == b'/' {
            if comp_len > MAX_NAME_LEN {
                return Err(CpioError::NameTooLong);
            }
            comp_len = 0;
        } else {
            comp_len += 1;
        }
    }
    if comp_len > MAX_NAME_LEN {
        return Err(CpioError::NameTooLong);
    }
    Ok(())
}

/// Create every intermediate directory of an absolute path (not the final
/// component), tolerating directories that already exist.
fn ensure_parents(path: &[u8]) -> Result<(), CpioError> {
    let mut i = 1;
    while i < path.len() {
        if path[i] == b'/' {
            mkdir_ignore_exists(&path[..i])?;
        }
        i += 1;
    }
    Ok(())
}

fn mkdir_ignore_exists(path: &[u8]) -> Result<(), CpioError> {
    match vfs_mkdir(path) {
        Ok(()) | Err(VfsError::AlreadyExists) => Ok(()),
        Err(e) => Err(CpioError::Vfs(e)),
    }
}

fn write_file(path: &[u8], data: &[u8], mode: u32) -> Result<(), CpioError> {
    let handle = vfs_open_flags(
        path,
        VfsOpenFlags {
            create: true,
            exclusive: false,
            truncate: true,
            readable: false,
            writable: true,
        },
    )
    .map_err(CpioError::Vfs)?;

    let mut off = 0usize;
    while off < data.len() {
        let n = handle
            .write(off as u64, &data[off..])
            .map_err(CpioError::Vfs)?;
        if n == 0 {
            return Err(CpioError::Vfs(VfsError::NoSpace));
        }
        off += n;
    }

    // The VFS create path defaults regular files to 0o644, losing the exec bit.
    vfs_set_mode(path, (mode & 0o7777) as u16).map_err(CpioError::Vfs)
}
