//! Why the computer refused to open or change a file, from what the kernel
//! says of the file and of the filesystem it is on, never from its path: a
//! path names nothing a mount cannot move.

use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{BASEFS_MAGIC, EXTENSION_MAGIC, O_NOFOLLOW, ST_RDONLY, UserStatfs};
use slopos_abi::syscall::posix::{O_NOCTTY, O_NONBLOCK};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// One of the system's own files, which nothing changes while it runs:
    /// the base's, or the install medium's, which the kernel serves alike.
    System,
    /// One of an extension's files, which change only with the extension.
    Extension,
    /// On a disk mounted read-only, or one that is write-protected.
    ReadOnlyDisk,
    /// Sealed, which a person reads as locked against changes.
    Sealed,
    /// Its permissions, or a folder's on the way to it, keep the person out.
    Denied,
}

impl Refusal {
    pub const ALL: [Refusal; 5] = [
        Refusal::System,
        Refusal::Extension,
        Refusal::ReadOnlyDisk,
        Refusal::Sealed,
        Refusal::Denied,
    ];

    /// Whether an error of `kind` is a refusal rather than a failure.
    pub fn is(kind: ErrorKind) -> bool {
        matches!(
            kind,
            ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
        )
    }

    /// Why opening `path` to read it was refused. Only permissions refuse a
    /// read; the system's and an extension's files are told apart because
    /// no one changes theirs.
    pub fn reading(path: &Path) -> Refusal {
        match filesystem(path).map(|fs| fs.f_type) {
            Some(BASEFS_MAGIC) => Refusal::System,
            Some(EXTENSION_MAGIC) => Refusal::Extension,
            _ => Refusal::Denied,
        }
    }

    /// Why changing `path`, or making it in its folder, was refused. A link
    /// is changed in its folder, not where it leads.
    pub fn changing(path: &Path) -> Refusal {
        let fs = match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => folder_filesystem(path),
            _ => filesystem(path),
        };
        let Some(fs) = fs else {
            return Refusal::Denied;
        };
        match fs.f_type {
            BASEFS_MAGIC => Refusal::System,
            EXTENSION_MAGIC => Refusal::Extension,
            _ if fs.f_flags & ST_RDONLY != 0 => Refusal::ReadOnlyDisk,
            _ if sealed(path) => Refusal::Sealed,
            _ => Refusal::Denied,
        }
    }
}

/// The filesystem `path` is on, else its folder's, where a name not made yet
/// would be.
fn filesystem(path: &Path) -> Option<UserStatfs> {
    statfs(path).or_else(|| folder_filesystem(path))
}

fn folder_filesystem(path: &Path) -> Option<UserStatfs> {
    match path.parent()? {
        folder if folder.as_os_str().is_empty() => statfs(Path::new(".")),
        folder => statfs(folder),
    }
}

fn statfs(path: &Path) -> Option<UserStatfs> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    slopos_cabi::statfs(&path).ok()
}

/// Whether `path` names a sealed file itself, not through a link. Only what
/// was a regular file a moment before is opened to read its flags, since
/// opening a FIFO's read end would let a writer waiting on it through.
fn sealed(path: &Path) -> bool {
    if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        return false;
    }
    OpenOptions::new()
        .read(true)
        .custom_flags((O_NONBLOCK | O_NOCTTY | u64::from(O_NOFOLLOW)) as i32)
        .open(path)
        .ok()
        .filter(|file| file.metadata().is_ok_and(|meta| meta.is_file()))
        .and_then(|file| slopos_cabi::inode_flags(file.as_raw_fd()).ok())
        .is_some_and(|flags| flags & FS_IMMUTABLE_FL != 0)
}
