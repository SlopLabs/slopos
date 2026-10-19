//! Files only a holder of `Seal` makes believable. A reader believes a file
//! only while it is a sealed regular file; a writer seals each one through the
//! descriptor it wrote and reads it back, so a process holding `DacOverride`
//! without `Seal` neither rewrites one nor plants one. A file written as
//! generations is never renamed or rewritten in place: the next generation
//! goes in beside the last, which stays believed until the new one is sealed,
//! so a writer cut off anywhere leaves either.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{O_DIRECTORY, O_NOFOLLOW};
use slopos_abi::syscall::posix::{O_CLOEXEC, O_NONBLOCK};
use slopos_extension_core::generation;

use crate::syscall::fs as sys_fs;

/// The most a small sealed file, a record or a key list, may hold.
pub const SMALL_MAX: u64 = 64 * 1024;

pub fn shown(path: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", path.display())
}

pub fn open_dir(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((O_DIRECTORY | O_NOFOLLOW) as i32 | O_CLOEXEC as i32)
        .open(path)
}

pub fn is_sealed(file: &File) -> bool {
    sys_fs::inode_flags(file.as_raw_fd()).is_ok_and(|flags| flags & FS_IMMUTABLE_FL != 0)
}

pub fn set_sealed(file: &File, sealed: bool) -> Result<(), String> {
    let flags = sys_fs::inode_flags(file.as_raw_fd()).map_err(|e| e.to_string())?;
    let want = if sealed {
        flags | FS_IMMUTABLE_FL
    } else {
        flags & !FS_IMMUTABLE_FL
    };
    if want == flags {
        return Ok(());
    }
    sys_fs::set_inode_flags(file.as_raw_fd(), want).map_err(|e| match e {
        crate::syscall::SyscallError::EOPNOTSUPP => {
            "a filesystem that keeps a seal for good, as a RAM root does; extensions need a disk root"
                .into()
        }
        e => e.to_string(),
    })
}

/// The entry at `path`, never followed: a file or a directory.
pub fn open_entry(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32 | O_NONBLOCK as i32)
        .open(path)
}

/// The sealed regular file at `path`, `None` when there is none; one there
/// unsealed is something other than a writer left, and an error.
pub fn read_sealed(path: &Path, max: u64) -> Result<Option<Vec<u8>>, String> {
    let file = match open_entry(path) {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(shown(path, e)),
    };
    let meta = file.metadata().map_err(|e| shown(path, e))?;
    if !meta.is_file() {
        return Err(shown(path, "not a regular file"));
    }
    if !is_sealed(&file) {
        return Err(shown(path, "not sealed, so not the store's"));
    }
    if meta.len() > max {
        return Err(shown(path, format!("larger than {max} bytes")));
    }
    read_all(&file, max).map(Some).map_err(|e| shown(path, e))
}

pub fn read_all(file: &File, max: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `path` is a sealed regular file, not followed if a link.
pub fn sealed_file(path: &Path) -> bool {
    open_entry(path)
        .is_ok_and(|file| file.metadata().is_ok_and(|m| m.is_file()) && is_sealed(&file))
}

/// Whether `path` is a sealed directory, not followed if a link.
pub fn sealed_dir(path: &Path) -> bool {
    open_dir(path).is_ok_and(|dir| is_sealed(&dir))
}

pub fn seal_dir(path: &Path) -> Result<(), String> {
    let dir = open_dir(path).map_err(|e| shown(path, e))?;
    set_sealed(&dir, true).map_err(|e| shown(path, format!("sealing: {e}")))
}

/// Run `f` with the sealed directory `dir` open for changes, sealing it
/// again however `f` ends.
pub fn unsealed<T>(dir: &Path, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let handle = open_dir(dir).map_err(|e| shown(dir, e))?;
    set_sealed(&handle, false).map_err(|e| shown(dir, format!("unsealing: {e}")))?;
    let out = f();
    set_sealed(&handle, true).map_err(|e| shown(dir, format!("sealing: {e}")))?;
    out
}

/// Make `bytes` the sealed file `name` in `dir`, which is open for changes.
/// What another process wrote into it before the seal is found reading it
/// back, a name it gave it before the seal in its link count, and what it
/// put at its name after is found there.
pub fn create_sealed(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let path = dir.join(name);
    remove_entry(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
        .open(&path)
        .map_err(|e| shown(&path, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| shown(&path, e))?;
    set_sealed(&file, true).map_err(|e| shown(&path, format!("sealing: {e}")))?;
    let back = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| read_all(&file, bytes.len() as u64));
    let here = fs::symlink_metadata(&path)
        .ok()
        .map(|m| (m.dev(), m.ino(), 1));
    let ours = file.metadata().ok().map(|m| (m.dev(), m.ino(), m.nlink()));
    if back.ok().as_deref() != Some(bytes) || here.is_none() || here != ours {
        let _ = set_sealed(&file, false);
        return Err(shown(&path, "changed by another process as it was written"));
    }
    Ok(())
}

/// [`create_sealed`], unless `name` holds `bytes` sealed already.
pub fn put_sealed(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let held = read_sealed(&dir.join(name), bytes.len() as u64)
        .ok()
        .flatten();
    if held.as_deref() == Some(bytes) {
        return Ok(());
    }
    create_sealed(dir, name, bytes, 0o644)
}

/// Take away what is at `path` in a directory open for changes, sealed or
/// not; nothing there is no error. A sealed file with a second name is
/// left sealed, since unsealing it would unseal that name too.
pub fn remove_entry(path: &Path) -> Result<(), String> {
    match open_entry(path) {
        Ok(file) if file.metadata().is_ok_and(|m| m.is_dir()) => {
            fs::remove_dir_all(path).map_err(|e| shown(path, e))
        }
        Ok(file) => {
            if is_sealed(&file) && !file.metadata().is_ok_and(|m| m.nlink() == 1) {
                return Err(shown(path, "sealed under another name too"));
            }
            set_sealed(&file, false).map_err(|e| shown(path, e))?;
            fs::remove_file(path).map_err(|e| shown(path, e))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) if e.raw_os_error() == Some(slopos_abi::Errno::ELOOP.raw()) => {
            fs::remove_file(path).map_err(|e| shown(path, e))
        }
        Err(e) => Err(shown(path, e)),
    }
}

/// The believed generation of `stem` in `dir`, the newest sealed one, and
/// its bytes.
pub fn newest(dir: &Path, stem: &str) -> Result<Option<(u64, Vec<u8>)>, String> {
    let mut generations: Vec<u64> = fs::read_dir(dir)
        .map_err(|e| shown(dir, e))?
        .filter_map(|entry| generation(entry.ok()?.file_name().to_str()?, stem))
        .collect();
    generations.sort_unstable_by(|a, b| b.cmp(a));
    for g in generations {
        if let Ok(Some(bytes)) = read_sealed(&dir.join(format!("{stem}.{g}")), SMALL_MAX) {
            return Ok(Some((g, bytes)));
        }
    }
    Ok(None)
}

/// Seal `bytes` as the next generation of `stem` in `dir`, which is open for
/// changes, then take every other away.
pub fn put_generation(dir: &Path, stem: &str, bytes: &[u8], mode: u32) -> Result<(), String> {
    let next = newest(dir, stem)?.map_or(1, |(g, _)| g + 1);
    let name = format!("{stem}.{next}");
    create_sealed(dir, &name, bytes, mode)?;
    let prefix = format!("{stem}.");
    for entry in fs::read_dir(dir).map_err(|e| shown(dir, e))? {
        let entry = entry.map_err(|e| shown(dir, e))?;
        let other = entry.file_name();
        let other = other.to_string_lossy();
        if other != name && other.starts_with(&prefix) {
            remove_entry(&entry.path())?;
        }
    }
    Ok(())
}

/// What `replace_sealed` writes `<name>`'s bytes as before the rename,
/// `.<name>` with this after it.
const TEMPORARY: &str = ".new";

/// Whether `path` names a file [`replace_sealed`] writes before its rename.
pub fn temporary(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix('.')?.strip_suffix(TEMPORARY))
        .is_some_and(|name| !name.is_empty())
}

/// Make `bytes` the sealed file at `path` in place of what is there, unless
/// it holds them sealed already. A sealed file can be neither renamed nor
/// renamed over, so the old one is unsealed for the rename and the new one
/// sealed after: in between a reader that believes only a sealed file finds
/// none. What another process changed in between is taken away rather than
/// left at `path` unsealed.
pub fn replace_sealed(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if read_sealed(path, bytes.len() as u64)
        .ok()
        .flatten()
        .as_deref()
        == Some(bytes)
    {
        return Ok(());
    }
    let dir = path.parent().ok_or_else(|| shown(path, "no directory"))?;
    let name = path.file_name().ok_or_else(|| shown(path, "no name"))?;
    let mut fresh = std::ffi::OsString::from(".");
    fresh.push(name);
    fresh.push(TEMPORARY);
    let fresh = dir.join(fresh);
    remove_entry(&fresh)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
        .open(&fresh)
        .map_err(|e| shown(&fresh, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| shown(&fresh, e))?;
    let old = open_entry(path).ok().filter(is_sealed);
    if let Some(old) = &old {
        set_sealed(old, false).map_err(|e| shown(path, format!("unsealing: {e}")))?;
    }
    if let Err(e) = fs::rename(&fresh, path) {
        if let Some(old) = &old {
            let _ = set_sealed(old, true);
        }
        let _ = remove_entry(&fresh);
        return Err(shown(path, e));
    }
    let sealed = set_sealed(&file, true).map_err(|e| shown(path, format!("sealing: {e}")));
    let back = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| read_all(&file, bytes.len() as u64));
    let here = fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino(), 1));
    let ours = file.metadata().ok().map(|m| (m.dev(), m.ino(), m.nlink()));
    if sealed.is_err() || back.ok().as_deref() != Some(bytes) || here.is_none() || here != ours {
        let _ = set_sealed(&file, false);
        let _ = remove_entry(path);
        return sealed.and(Err(shown(
            path,
            "changed by another process as it was written",
        )));
    }
    open_dir(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| shown(dir, e))
}
