//! The extension store on disk. The kernel keeps the store's directory and
//! those on the way to it where they are. Only a sealed file is believed
//! ([`crate::sealed`]): a record or the keys go in as a generation beside the
//! last.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use slopos_abi::fs::O_NOFOLLOW;
use slopos_abi::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use slopos_abi::syscall::posix::O_CLOEXEC;
use slopos_extension_core::image::{ImageError, Sink, runs, verify_tree};
use slopos_extension_core::keys::Keys;
use slopos_extension_core::record::Record;
use slopos_extension_core::{KEYS, RECORD, STORE, image_file, sidecar_file, signature_file};

use super::disk::Disk;
use crate::sealed::{
    SMALL_MAX, is_sealed, lock, newest, open_dir, open_entry, put_generation, put_sealed,
    read_sealed, remove_entry, set_sealed, shown, unsealed,
};
use crate::syscall::process;

pub struct Store {
    root: PathBuf,
    _lock: Option<File>,
}

pub trait Copies: FnOnce(&mut (dyn Sink + Send)) -> Result<(), ImageError> {}
impl<F: FnOnce(&mut (dyn Sink + Send)) -> Result<(), ImageError>> Copies for F {}

fn create_sealed_image(
    dir: &Path,
    name: &str,
    len: u64,
    digest: &[u8; 32],
    copy: impl Copies,
) -> Result<(), String> {
    let path = dir.join(name);
    remove_entry(&path)?;
    let copied = copy_sealed_image(&path, len, digest, copy);
    if copied.is_err() {
        let _ = remove_entry(&path);
    }
    copied
}

/// Read back only as far as its tree: every reader holds data blocks to it.
fn copy_sealed_image(
    path: &Path,
    len: u64,
    digest: &[u8; 32],
    copy: impl Copies,
) -> Result<(), String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(O_NOFOLLOW as i32 | O_CLOEXEC as i32)
        .open(path)
        .map_err(|e| shown(path, e))?;
    written_behind(path, &file, copy)?;
    file.set_len(len)
        .and_then(|()| file.sync_all())
        .map_err(|e| shown(path, e))?;
    set_sealed(&file, true).map_err(|e| shown(path, format!("sealing: {e}")))?;
    let here = fs::symlink_metadata(path)
        .ok()
        .map(|m| (m.dev(), m.ino(), 1));
    let ours = file.metadata().ok().map(|m| (m.dev(), m.ino(), m.nlink()));
    if here.is_none() || here != ours {
        return Err(shown(path, "changed by another process as it was written"));
    }
    verify_tree(&mut Disk(&file), digest).map_err(|e| shown(path, format!("the copy: {e}")))
}

const BEHIND: usize = 4;
/// Queued, being written, two read ahead, and the leaves'.
const BUFFERS: usize = BEHIND + 4;
/// The writer's stack: a run's write, not std's 2 MiB.
const WRITER_STACK: usize = 64 << 10;

fn put_runs(path: &Path, file: &File, at: u64, bytes: &[u8]) -> Result<(), String> {
    for run in runs(bytes) {
        file.write_all_at(&bytes[run.clone()], at + run.start as u64)
            .map_err(|e| shown(path, e))?;
    }
    Ok(())
}

struct Behind {
    queue: SyncSender<(u64, Vec<u8>)>,
    done: Receiver<Vec<u8>>,
    made: usize,
}

impl Sink for Behind {
    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        if let Ok(buffer) = self.done.try_recv() {
            return Ok(buffer);
        }
        if self.made < BUFFERS {
            self.made += 1;
            return Ok(Vec::new());
        }
        self.done.recv().map_err(|_| "the writer stopped".into())
    }

    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), String> {
        self.queue
            .send((at, bytes))
            .map_err(|_| "the writer stopped".into())
    }
}

/// Without a writer thread; a write's failure is kept to be told first.
struct Here<'a> {
    path: &'a Path,
    file: &'a File,
    spare: Vec<Vec<u8>>,
    failed: Option<String>,
}

impl Sink for Here<'_> {
    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        Ok(self.spare.pop().unwrap_or_default())
    }

    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), String> {
        if let Err(e) = put_runs(self.path, self.file, at, &bytes) {
            self.failed = Some(e);
            return Err("the writer stopped".into());
        }
        self.spare.push(bytes);
        Ok(())
    }
}

/// A write's failure is told before the copy's, which follows from it.
fn written_behind(path: &Path, file: &File, copy: impl Copies) -> Result<(), String> {
    let (queue, queued) = mpsc::sync_channel::<(u64, Vec<u8>)>(BEHIND);
    let (written, done) = mpsc::sync_channel::<Vec<u8>>(BUFFERS);
    thread::scope(|scope| {
        let writer = thread::Builder::new()
            .stack_size(WRITER_STACK)
            .spawn_scoped(scope, move || {
                queued.iter().try_for_each(|(at, bytes)| {
                    put_runs(path, file, at, &bytes)?;
                    let _ = written.try_send(bytes);
                    Ok::<(), String>(())
                })
            });
        let Ok(writer) = writer else {
            let mut here = Here {
                path,
                file,
                spare: Vec::new(),
                failed: None,
            };
            let copied = copy(&mut here);
            return here
                .failed
                .map_or(Ok(()), Err)
                .and(copied.map_err(|e| e.to_string()));
        };
        let copied = copy(&mut Behind {
            queue,
            done,
            made: 0,
        });
        writer
            .join()
            .unwrap_or_else(|_| Err(shown(path, "the writer stopped")))?;
        copied.map_err(|e| e.to_string())
    })
}

/// Whether `path` holds the image whose root is `digest`, sealed, `len`
/// long and its tree whole.
fn holds_image(path: &Path, len: u64, digest: &[u8; 32]) -> bool {
    let Ok(file) = open_entry(path) else {
        return false;
    };
    let held = file
        .metadata()
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.len());
    is_sealed(&file) && held == Some(len) && verify_tree(&mut Disk(&file), digest).is_ok()
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
        if !top.is_absolute() {
            return Err(shown(top, "not an absolute path"));
        }
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
        let lock = lock(&at)?;
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
            put_generation(&self.root, KEYS, keys.render().as_bytes(), 0o644)
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

    /// The sealed image `digest` names of `name`.
    pub fn image(&self, name: &str, digest: &[u8; 32]) -> Result<Disk, String> {
        let path = self.dir(name).join(image_file(digest));
        let file = match open_entry(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Err(shown(&path, "missing")),
            Err(e) => return Err(shown(&path, e)),
        };
        if !file.metadata().is_ok_and(|m| m.is_file()) || !is_sealed(&file) {
            return Err(shown(
                &path,
                "not a sealed regular file, so not the store's",
            ));
        }
        Ok(Disk(file))
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
    /// does not name is taken away after, whether or not `f` finished.
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
            let written = f(&edit);
            let collected = edit.collect();
            written.and(collected)
        })
    }
}

/// A directory of the store open for changes.
pub struct Edit<'a> {
    dir: &'a Path,
}

impl Edit<'_> {
    /// Copies only when the store does not hold the image already.
    pub fn put_image(
        &self,
        digest: &[u8; 32],
        len: u64,
        signed: Option<(&[u8], &[u8])>,
        copy: impl Copies,
    ) -> Result<(), String> {
        let name = image_file(digest);
        if !holds_image(&self.dir.join(&name), len, digest) {
            create_sealed_image(self.dir, &name, len, digest, copy)?;
        }
        if let Some((sidecar, signature)) = signed {
            put_sealed(self.dir, &sidecar_file(digest), sidecar)?;
            put_sealed(self.dir, &signature_file(digest), signature)?;
        }
        Ok(())
    }

    pub fn put_record(&self, record: &Record) -> Result<(), String> {
        put_generation(self.dir, RECORD, record.render().as_bytes(), 0o644)
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
