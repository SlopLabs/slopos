//! The extension store on disk. The kernel keeps the store's directory and
//! those on the way to it where they are. Only a sealed file is believed, and
//! a writer seals each one through the descriptor it wrote and reads it back,
//! so a process holding `DacOverride` without `Seal` can neither rewrite a
//! record nor plant a key. Nothing is renamed or rewritten in place: a record
//! or the keys go in as a generation beside the last, which stays believed
//! until the new one is sealed, so a writer cut off anywhere leaves either.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{O_DIRECTORY, O_NOFOLLOW};
use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use slopos_abi::syscall::posix::{LOCK_EX, O_CLOEXEC, O_NONBLOCK};
use slopos_extension_core::keys::Keys;
use slopos_extension_core::record::Record;
use slopos_extension_core::{
    KEYS, RECORD, STORE, generation, image_file, sidecar_file, signature_file,
};

use crate::syscall::fs as sys_fs;
use crate::syscall::process;

const SMALL_MAX: u64 = 64 * 1024;

pub struct Store {
    root: PathBuf,
    _lock: Option<File>,
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
fn open_entry(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32 | O_NONBLOCK as i32)
        .open(path)
}

/// The sealed regular file at `path`, `None` when there is none; one there
/// unsealed is something other than a writer left, and an error.
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
    read_all(&file, max).map(Some).map_err(|e| shown(path, e))
}

fn read_all(file: &File, max: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Whether `path` is a sealed regular file, not followed if a link.
pub fn sealed_file(path: &Path) -> bool {
    open_entry(path)
        .is_ok_and(|file| file.metadata().is_ok_and(|m| m.is_file()) && is_sealed(&file))
}

/// Seal the directory at `path`.
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
fn create_sealed(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let path = dir.join(name);
    remove_entry(&path)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
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
fn put_sealed(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), String> {
    let held = read_sealed(&dir.join(name), bytes.len() as u64)
        .ok()
        .flatten();
    if held.as_deref() == Some(bytes) {
        return Ok(());
    }
    create_sealed(dir, name, bytes)
}

/// Take away what is at `path` in a directory open for changes, sealed or
/// not; nothing there is no error. A sealed file with a second name is
/// left sealed, since unsealing it would unseal that name too.
fn remove_entry(path: &Path) -> Result<(), String> {
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
fn newest(dir: &Path, stem: &str) -> Result<Option<(u64, Vec<u8>)>, String> {
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
fn put_generation(dir: &Path, stem: &str, bytes: &[u8]) -> Result<(), String> {
    let next = newest(dir, stem)?.map_or(1, |(g, _)| g + 1);
    let name = format!("{stem}.{next}");
    create_sealed(dir, &name, bytes)?;
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

/// Make `bytes` the sealed file at `path` in place of what is there, unless
/// it holds them sealed already. A sealed file can be neither renamed nor
/// renamed over, so the old one is unsealed for the rename and the new one
/// sealed after: in between a reader that believes only a sealed file finds
/// none.
pub fn replace_sealed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if read_sealed(path, bytes.len() as u64)
        .ok()
        .flatten()
        .as_deref()
        == Some(bytes)
    {
        return Ok(());
    }
    let dir = path.parent().ok_or_else(|| shown(path, "no directory"))?;
    let mut fresh = path.as_os_str().to_owned();
    fresh.push(".new");
    let fresh = PathBuf::from(fresh);
    remove_entry(&fresh)?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
        .open(&fresh)
        .map_err(|e| shown(&fresh, e))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| shown(&fresh, e))?;
    match open_entry(path) {
        Ok(old) if is_sealed(&old) => {
            set_sealed(&old, false).map_err(|e| shown(path, format!("unsealing: {e}")))?
        }
        _ => {}
    }
    fs::rename(&fresh, path).map_err(|e| shown(path, e))?;
    set_sealed(&file, true).map_err(|e| shown(path, format!("sealing: {e}")))?;
    let back = file
        .seek(SeekFrom::Start(0))
        .and_then(|_| read_all(&file, bytes.len() as u64));
    let here = fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino(), 1));
    let ours = file.metadata().ok().map(|m| (m.dev(), m.ino(), m.nlink()));
    if back.ok().as_deref() != Some(bytes) || here.is_none() || here != ours {
        let _ = set_sealed(&file, false);
        return Err(shown(path, "changed by another process as it was written"));
    }
    open_dir(dir)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| shown(dir, e))
}

fn text(path: &Path, bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| shown(path, "not UTF-8"))
}

impl Store {
    /// The store. A writer locks out every other, ignores the termination
    /// signals a requester relays, and seals the store's directory if no
    /// writer has yet; a reader takes nothing, since nothing it can see is
    /// half made.
    pub fn open(write: bool) -> Result<Store, String> {
        Self::open_at(Path::new("/"), write)
    }

    /// The store of the root at `top`, a mounted disk's as the rescue shell
    /// reaches one, as [`open`](Self::open) opens this root's.
    pub fn open_at(top: &Path, write: bool) -> Result<Store, String> {
        let root = top.join(STORE.trim_start_matches('/'));
        let mut walked = PathBuf::from("/");
        for part in root.iter().skip(1) {
            walked.push(part);
            open_dir(&walked).map_err(|e| shown(&walked, e))?;
        }
        if !write {
            return Ok(Store { root, _lock: None });
        }
        for signum in [SIGHUP, SIGINT, SIGQUIT, SIGTERM] {
            let _ = process::ignore_signal(signum);
        }
        let mut at = root.clone().into_os_string();
        at.push(".lock");
        let at = PathBuf::from(at);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
            .open(&at)
            .map_err(|e| shown(&at, e))?;
        sys_fs::flock(lock.as_raw_fd(), LOCK_EX).map_err(|e| shown(&at, e))?;
        let dir = open_dir(&root).map_err(|e| shown(&root, e))?;
        set_sealed(&dir, true).map_err(|e| shown(&root, format!("sealing: {e}")))?;
        Ok(Store {
            root,
            _lock: Some(lock),
        })
    }

    pub fn keys(&self) -> Result<Keys, String> {
        let Some((g, bytes)) = newest(&self.root, KEYS)? else {
            return Ok(Keys::default());
        };
        let path = self.root.join(format!("{KEYS}.{g}"));
        Keys::parse(&text(&path, bytes)?).map_err(|e| shown(&path, e))
    }

    pub fn put_keys(&self, keys: &Keys) -> Result<(), String> {
        unsealed(&self.root, || {
            put_generation(&self.root, KEYS, keys.render().as_bytes())
        })
    }

    /// The extensions the store keeps a directory for, by name.
    pub fn names(&self) -> Result<Vec<String>, String> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|e| shown(&self.root, e))? {
            let entry = entry.map_err(|e| shown(&self.root, e))?;
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if slopos_abi::extension::valid_name(name.as_bytes()) && open_dir(&entry.path()).is_ok()
            {
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
        let dir = self.dir(name);
        match open_dir(&dir) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(shown(&dir, e)),
        }
        let Some((g, bytes)) = newest(&dir, RECORD)? else {
            return Ok(None);
        };
        let path = dir.join(format!("{RECORD}.{g}"));
        let record =
            Record::parse(&text(&path, bytes)?).ok_or_else(|| shown(&path, "not a record"))?;
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
    /// [`Edit`] it is handed, the record last, and what the believed record
    /// does not name is taken away after.
    pub fn edit(
        &self,
        name: &str,
        f: impl FnOnce(&Edit<'_>) -> Result<(), String>,
    ) -> Result<(), String> {
        let dir = self.dir(name);
        if open_dir(&dir).is_err() {
            unsealed(&self.root, || {
                remove_entry(&dir)?;
                fs::create_dir(&dir).map_err(|e| shown(&dir, e))
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
        put_sealed(self.dir, &image_file(digest), image)?;
        if let Some((sidecar, signature)) = signed {
            put_sealed(self.dir, &sidecar_file(digest), sidecar)?;
            put_sealed(self.dir, &signature_file(digest), signature)?;
        }
        Ok(())
    }

    pub fn put_record(&self, record: &Record) -> Result<(), String> {
        put_generation(self.dir, RECORD, record.render().as_bytes())
    }

    fn collect(&self) -> Result<(), String> {
        let believed = newest(self.dir, RECORD)?;
        let record = believed
            .as_ref()
            .and_then(|(_, bytes)| std::str::from_utf8(bytes).ok())
            .and_then(Record::parse);
        let mut kept: Vec<String> = believed
            .iter()
            .map(|(g, _)| format!("{RECORD}.{g}"))
            .collect();
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
