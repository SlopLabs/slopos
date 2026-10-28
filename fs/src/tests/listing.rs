//! Directory listings as the fs tests read them back.

use slopos_ostd::KVec;

use crate::vfs::{FileSystem, FileType, InodeId, VfsError, VfsResult, resolve_path};

/// One entry of a directory [`list_dir`] read.
pub(crate) struct Listed {
    name: [u8; crate::MAX_NAME_LEN],
    len: usize,
    pub inode: InodeId,
    pub file_type: FileType,
}

impl Listed {
    pub(crate) fn name(&self) -> &[u8] {
        &self.name[..self.len]
    }
}

/// Entries one page of [`list_dir_from`] holds: a `KVec` of many more would
/// outgrow the slab's 1 MiB allocation ceiling.
const LIST_PAGE: usize = 2048;

/// Every entry of the directory `path` names, `.` and `..` included, read
/// through the filesystem's `readdir_cookie` as `getdents64` reads it.
#[inline(never)]
pub(crate) fn list_dir(path: &[u8]) -> VfsResult<(&'static dyn FileSystem, KVec<Listed>)> {
    list_page(path, 0, usize::MAX).map(|(fs, entries, _)| (fs, entries))
}

/// Up to [`LIST_PAGE`] entries of the directory `path` names from `cookie`, and
/// the cookie the next page starts at while any may follow.
#[inline(never)]
pub(crate) fn list_dir_from(
    path: &[u8],
    cookie: u64,
) -> VfsResult<(&'static dyn FileSystem, KVec<Listed>, Option<u64>)> {
    list_page(path, cookie, LIST_PAGE)
}

/// The entries are collected first, since a filesystem may hold its lock
/// across the callback.
fn list_page(
    path: &[u8],
    cookie: u64,
    limit: usize,
) -> VfsResult<(&'static dyn FileSystem, KVec<Listed>, Option<u64>)> {
    let resolved = resolve_path(path)?;
    if resolved.fs.stat(resolved.inode)?.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    let mut entries = KVec::new();
    let mut failed = None;
    let mut resume = cookie;
    resolved.fs.readdir_cookie(
        resolved.inode,
        cookie,
        &mut |next, name, inode, file_type| {
            if name.len() > crate::MAX_NAME_LEN {
                failed = Some(VfsError::NameTooLong);
                return false;
            }
            let mut entry = Listed {
                name: [0; crate::MAX_NAME_LEN],
                len: name.len(),
                inode,
                file_type,
            };
            entry.name[..name.len()].copy_from_slice(name);
            if entries.push(entry).is_err() {
                failed = Some(VfsError::NoSpace);
                return false;
            }
            resume = next;
            entries.len() < limit
        },
    )?;
    match failed {
        Some(e) => Err(e),
        None => {
            let more = (entries.len() == limit).then_some(resume);
            Ok((resolved.fs, entries, more))
        }
    }
}
