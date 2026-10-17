//! The extension store on disk. The kernel keeps the store's directory and
//! those on the way to it where they are; every file and directory in it is
//! sealed by the mechanism that wrote it, and nothing unsealed is believed,
//! so a process holding `DacOverride` without `Seal` can neither rewrite a
//! record nor plant a key. A writer takes the store's lock, opens a sealed
//! directory only for as long as it writes in it, and replaces a file by
//! renaming a whole new one over it.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{O_DIRECTORY, O_NOFOLLOW};
use slopos_abi::syscall::posix::{LOCK_EX, LOCK_SH, O_CLOEXEC, O_NONBLOCK};
use slopos_extension_core::keys::Keys;
use slopos_extension_core::record::Record;
use slopos_extension_core::{KEYS, RECORD, STORE, image_file, sidecar_file, signature_file};

use crate::syscall::fs as sys_fs;

const SMALL_MAX: u64 = 64 * 1024;

pub struct Store {
    root: PathBuf,
    _lock: File,
}

fn shown(path: &Path, e: impl std::fmt::Display) -> String {
    format!("{}: {e}", path.display())
}

fn open_dir(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((O_DIRECTORY | O_NOFOLLOW) as i32 | O_CLOEXEC as i32)
        .open(path)
}

fn is_sealed(file: &File) -> bool {
    sys_fs::inode_flags(file.as_raw_fd()).is_ok_and(|flags| flags & FS_IMMUTABLE_FL != 0)
}

fn set_sealed(file: &File, sealed: bool) -> Result<(), String> {
    let flags = sys_fs::inode_flags(file.as_raw_fd()).map_err(|e| e.to_string())?;
    let want = if sealed {
        flags | FS_IMMUTABLE_FL
    } else {
        flags & !FS_IMMUTABLE_FL
    };
    match sys_fs::set_inode_flags(file.as_raw_fd(), want) {
        _ if want == flags => Ok(()),
        Ok(()) => Ok(()),
        Err(crate::syscall::SyscallError::EOPNOTSUPP) => Err(
            "a filesystem that keeps a seal for good, as a RAM root does; extensions need a disk root"
                .into(),
        ),
        Err(e) => Err(e.to_string()),
    }
}

/// The entry at `path`, never followed: a file or a directory.
fn open_entry(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32 | O_NONBLOCK as i32)
        .open(path)
}

/// The sealed regular file at `path`, `None` when there is none; one there
/// unsealed is something other than a mechanism wrote, and an error.
fn read_sealed(path: &Path, max: u64) -> Result<Option<Vec<u8>>, String> {
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
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| shown(path, e))?;
    Ok(Some(bytes))
}

/// Run `f` with the sealed directory `dir` open for changes, sealing it
/// again however `f` ends.
fn unsealed<T>(dir: &Path, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let handle = open_dir(dir).map_err(|e| shown(dir, e))?;
    set_sealed(&handle, false).map_err(|e| shown(dir, format!("unsealing: {e}")))?;
    let out = f();
    set_sealed(&handle, true).map_err(|e| shown(dir, format!("sealing: {e}")))?;
    out
}

/// Make `bytes` the sealed file `name` in `dir`, which is open for changes,
/// in one rename.
fn write_sealed(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let staged = dir.join(format!(".{name}.new"));
    remove_entry(&staged)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
        .open(&staged)
        .map_err(|e| shown(&staged, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| shown(&staged, e))?;
    drop(file);
    let target = dir.join(name);
    match open_entry(&target) {
        Ok(old) => set_sealed(&old, false).map_err(|e| shown(&target, e))?,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(shown(&target, e)),
    }
    fs::rename(&staged, &target).map_err(|e| shown(&target, e))?;
    let written = open_entry(&target).map_err(|e| shown(&target, e))?;
    set_sealed(&written, true).map_err(|e| shown(&target, format!("sealing: {e}")))?;
    written.sync_all().map_err(|e| shown(&target, e))
}

/// Take away what is at `path` in a directory open for changes, sealed or
/// not; nothing there is no error. Only an unsealed directory, which no
/// mechanism made, goes with what it holds.
fn remove_entry(path: &Path) -> Result<(), String> {
    match open_entry(path) {
        Ok(file) if file.metadata().is_ok_and(|m| m.is_dir()) => {
            fs::remove_dir_all(path).map_err(|e| shown(path, e))
        }
        Ok(file) => {
            let _ = set_sealed(&file, false);
            fs::remove_file(path).map_err(|e| shown(path, e))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) if e.raw_os_error() == Some(slopos_abi::Errno::ELOOP.raw()) => {
            fs::remove_file(path).map_err(|e| shown(path, e))
        }
        Err(e) => Err(shown(path, e)),
    }
}

impl Store {
    /// The store, locked against every other `extctl`: shared to read, and
    /// to write exclusively and sealed first if no writer has sealed it yet.
    pub fn open(write: bool) -> Result<Store, String> {
        let root = PathBuf::from(STORE);
        let mut walked = PathBuf::from("/");
        for part in root.iter().skip(1) {
            walked.push(part);
            let dir = open_dir(&walked).map_err(|e| shown(&walked, e))?;
            if !dir.metadata().is_ok_and(|m| m.is_dir()) {
                return Err(shown(&walked, "not a directory"));
            }
        }
        let lock = open_dir(&root).map_err(|e| shown(&root, e))?;
        let operation = if write { LOCK_EX } else { LOCK_SH };
        sys_fs::flock(lock.as_raw_fd(), operation).map_err(|e| shown(&root, e))?;
        if write && !is_sealed(&lock) {
            set_sealed(&lock, true).map_err(|e| shown(&root, format!("sealing: {e}")))?;
        }
        Ok(Store { root, _lock: lock })
    }

    /// Whether anything in the store is believed: a writer has sealed it.
    pub fn believed(&self) -> bool {
        is_sealed(&self._lock)
    }

    pub fn keys(&self) -> Result<Keys, String> {
        if !self.believed() {
            return Ok(Keys::default());
        }
        let path = self.root.join(KEYS);
        match read_sealed(&path, SMALL_MAX)? {
            None => Ok(Keys::default()),
            Some(bytes) => {
                let text = String::from_utf8(bytes).map_err(|_| shown(&path, "not UTF-8"))?;
                Keys::parse(&text).map_err(|e| shown(&path, e))
            }
        }
    }

    pub fn put_keys(&self, keys: &Keys) -> Result<(), String> {
        unsealed(&self.root, || {
            write_sealed(&self.root, KEYS, keys.render().as_bytes())
        })
    }

    /// The extensions the store keeps a sealed directory for, by name.
    pub fn names(&self) -> Result<Vec<String>, String> {
        if !self.believed() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|e| shown(&self.root, e))? {
            let entry = entry.map_err(|e| shown(&self.root, e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let sealed_dir = open_dir(&entry.path()).is_ok_and(|dir| is_sealed(&dir));
            if slopos_abi::extension::valid_name(name.as_bytes()) && sealed_dir {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    pub fn record(&self, name: &str) -> Result<Option<Record>, String> {
        if !self.names()?.iter().any(|n| n == name) {
            return Ok(None);
        }
        let path = self.dir(name).join(RECORD);
        let Some(bytes) = read_sealed(&path, SMALL_MAX)? else {
            return Ok(None);
        };
        let record = std::str::from_utf8(&bytes)
            .ok()
            .and_then(Record::parse)
            .ok_or_else(|| shown(&path, "not a record"))?;
        if record.name != name {
            return Err(shown(&path, "the record of another extension"));
        }
        Ok(Some(record))
    }

    pub fn image(&self, name: &str, digest: &[u8; 32], max: u64) -> Result<Vec<u8>, String> {
        let path = self.dir(name).join(image_file(digest));
        read_sealed(&path, max)?.ok_or_else(|| shown(&path, "missing"))
    }

    /// The sidecar and signature a key installed `digest` with, if it was.
    pub fn signed(
        &self,
        name: &str,
        digest: &[u8; 32],
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>, String> {
        let dir = self.dir(name);
        let sidecar = read_sealed(&dir.join(sidecar_file(digest)), SMALL_MAX)?;
        let signature = read_sealed(&dir.join(signature_file(digest)), SMALL_MAX)?;
        Ok(sidecar.zip(signature))
    }

    /// Change what the store keeps of `name`: `f` writes through the
    /// [`Edit`] it is handed, the record last, and what the record no
    /// longer names is taken away after.
    pub fn edit(
        &self,
        name: &str,
        f: impl FnOnce(&Edit<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let dir = self.dir(name);
        let exists = open_dir(&dir).is_ok();
        if !exists {
            unsealed(&self.root, || {
                fs::create_dir(&dir).map_err(|e| shown(&dir, e))?;
                let made = open_dir(&dir).map_err(|e| shown(&dir, e))?;
                set_sealed(&made, true).map_err(|e| shown(&dir, e))
            })?;
        }
        unsealed(&dir, || {
            let edit = Edit { dir: &dir };
            f(&edit)?;
            edit.collect()
        })
    }
}

/// A directory of the store open for changes.
pub struct Edit<'a> {
    dir: &'a Path,
}

impl Edit<'_> {
    pub fn put_image(
        &self,
        digest: &[u8; 32],
        image: &[u8],
        signed: Option<(&[u8], &[u8])>,
    ) -> Result<(), String> {
        write_sealed(self.dir, &image_file(digest), image)?;
        if let Some((sidecar, signature)) = signed {
            write_sealed(self.dir, &sidecar_file(digest), sidecar)?;
            write_sealed(self.dir, &signature_file(digest), signature)?;
        }
        Ok(())
    }

    pub fn put_record(&self, record: &Record) -> Result<(), String> {
        write_sealed(self.dir, RECORD, record.render().as_bytes())
    }

    /// Take away every file the record does not name: images it left, and
    /// what was staged and never renamed into place.
    fn collect(&self) -> Result<(), String> {
        let record = read_sealed(&self.dir.join(RECORD), SMALL_MAX)?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| Record::parse(&text));
        let mut kept = vec![RECORD.to_owned()];
        for image in record
            .iter()
            .flat_map(|r| r.current.iter().chain(&r.previous))
        {
            kept.extend([
                image_file(&image.sha256),
                sidecar_file(&image.sha256),
                signature_file(&image.sha256),
            ]);
        }
        for entry in fs::read_dir(self.dir).map_err(|e| shown(self.dir, e))? {
            let entry = entry.map_err(|e| shown(self.dir, e))?;
            let name = entry.file_name();
            if !kept.iter().any(|k| *k == *name.to_string_lossy()) {
                remove_entry(&entry.path())?;
            }
        }
        Ok(())
    }
}
