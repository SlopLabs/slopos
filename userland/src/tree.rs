//! A tree installed onto a root from a directory, by `slopos-tree-core`'s
//! rule: the installer's toolchain and apps, init's apps on a live root, and
//! `tree-install`'s as the session.

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, lchown, symlink};
use std::path::Path;

use slopos_abi::fs::O_NOFOLLOW;
use slopos_tls_core::hash::{Hash, Sha256};
use slopos_tree_core::{Item, Kind, MANIFEST_DIR, Manifest, Root, State, Step, plan};

const COPY_CHUNK: usize = 1 << 20;

pub fn make_dir(path: &str) -> Result<(), String> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e)
            if e.kind() == ErrorKind::AlreadyExists
                && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) =>
        {
            Ok(())
        }
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// `path` and, a link included, nothing it names, given to `owner`.
pub fn give(path: &str, (uid, gid): (u32, u32)) -> Result<(), String> {
    lchown(path, Some(uid), Some(gid)).map_err(|e| format!("{path}: {e}"))
}

/// A file made where nothing is, a link included, which a plain create
/// would follow.
pub fn create_new(path: &str) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn replace(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    let temp = format!("{path}.part");
    match fs::remove_file(&temp) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    create_new(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&temp, path))
}

/// `bytes` at `path` through a flushed sibling and a rename, so a crash
/// leaves the old file or the new one.
pub fn write_durably(path: &str, bytes: &[u8]) -> Result<(), String> {
    replace(path, bytes).map_err(|e| format!("{path}: {e}"))
}

/// A file the root holds at `path`, if any; anything else there is refused.
pub fn read_regular(path: &str) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("{path}: {e}")),
        Ok(_) => Err(format!("{path} is not a file")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

/// The manifest installing `dir` records: its entries parents first, and an
/// identity that is a digest of every entry's kind, mode, path and bytes.
pub fn scan(dir: &Path) -> Result<Manifest, String> {
    fn walk(
        dir: &Path,
        prefix: &str,
        items: &mut Vec<Item>,
        digest: &mut Sha256,
    ) -> Result<(), String> {
        let mut names: Vec<_> = fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        names.sort();
        for name in names {
            let name = name
                .into_string()
                .map_err(|n| format!("{}: {n:?} is not UTF-8", dir.display()))?;
            let path = dir.join(&name);
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let meta =
                fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let kind = if meta.file_type().is_symlink() {
                Kind::Link
            } else if meta.is_dir() {
                Kind::Dir
            } else if meta.is_file() {
                Kind::File
            } else {
                return Err(format!(
                    "{} is not a file, directory or link",
                    path.display()
                ));
            };
            digest.update(match kind {
                Kind::Dir => b"d",
                Kind::File => b"f",
                Kind::Link => b"l",
            });
            digest.update(&meta.permissions().mode().to_le_bytes());
            digest.update(rel.as_bytes());
            digest.update(&[0]);
            let size = match kind {
                Kind::File => {
                    let mut file =
                        File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                    let mut chunk = vec![0u8; COPY_CHUNK];
                    loop {
                        match file.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(n) => digest.update(&chunk[..n]),
                            Err(e) => return Err(format!("{}: {e}", path.display())),
                        }
                    }
                    Some(meta.len())
                }
                Kind::Link => {
                    let target =
                        fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                    digest.update(target.as_os_str().as_encoded_bytes());
                    None
                }
                Kind::Dir => None,
            };
            items.push(Item {
                kind,
                size,
                rel: rel.clone(),
            });
            if kind == Kind::Dir {
                walk(&path, &rel, items, digest)?;
            }
        }
        Ok(())
    }
    let mut items = Vec::new();
    let mut digest = Sha256::new();
    walk(dir, "", &mut items, &mut digest)?;
    let identity = digest
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(Manifest { identity, items })
}

/// A root as `lstat` reads it below `prefix`.
struct Target<'a>(&'a str);

impl Root for Target<'_> {
    fn state(&mut self, path: &str) -> State {
        match fs::symlink_metadata(format!("{}{path}", self.0)) {
            Err(_) => State::Absent,
            Ok(meta) if meta.file_type().is_symlink() => State::Link,
            Ok(meta) if meta.is_dir() => State::Dir,
            Ok(meta) if meta.is_file() => State::File,
            Ok(_) => State::Other,
        }
    }

    fn children(&mut self, dir: &str) -> Vec<String> {
        fs::read_dir(format!("{}{dir}", self.0))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect()
    }
}

/// The tree `name`, read from `source`, installed at `guest` on the root
/// mounted at `root` (empty for the running system's).
pub struct Install<'a> {
    pub root: &'a str,
    pub name: &'a str,
    pub guest: &'a str,
    pub source: &'a Path,
    /// Who what the install makes, and the manifest, belong to; whoever
    /// installs when `None`.
    pub owner: Option<(u32, u32)>,
}

impl Install<'_> {
    fn path(&self, path: &str) -> String {
        format!("{}{path}", self.root)
    }

    /// Whether `path` and each directory above it on the root is a directory,
    /// making those that are absent when `make`. A link on the way is refused
    /// rather than followed.
    fn walk_dirs(&self, path: &str, make: bool) -> Result<bool, String> {
        let ends = path.match_indices('/').map(|(at, _)| at).skip(1);
        for end in ends.chain([path.len()]) {
            let dir = &path[..end];
            match fs::symlink_metadata(self.path(dir)) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(format!("{dir} on the root is not a directory")),
                Err(e) if e.kind() == ErrorKind::NotFound && make => {
                    make_dir(&self.path(dir))?;
                    fs::set_permissions(self.path(dir), fs::Permissions::from_mode(0o755))
                        .map_err(|e| format!("{dir}: {e}"))?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(format!("{dir}: {e}")),
            }
        }
        Ok(true)
    }

    /// The manifest at `path`, whose directory holds a manifest of every
    /// tree, rewritten in place when the installer may not make a sibling
    /// there: the session's own manifest in uid 0's directory.
    fn record(&self, path: &str, manifest: &Manifest) -> Result<(), String> {
        let text = manifest.render();
        match replace(path, text.as_bytes()) {
            Err(e) if e.kind() == ErrorKind::PermissionDenied => OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(O_NOFOLLOW as i32)
                .open(path)
                .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all())),
            done => done,
        }
        .map_err(|e| format!("recording {path}: {e}"))?;
        self.owner.map_or(Ok(()), |owner| give(path, owner))
    }

    fn copy(&self, from: &Path, to: &str, copied: &mut dyn FnMut(u64)) -> Result<(), String> {
        let mut source = File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
        let mut dest = create_new(to).map_err(|e| format!("{to}: {e}"))?;
        let mut chunk = vec![0u8; COPY_CHUNK];
        loop {
            let n = source
                .read(&mut chunk)
                .map_err(|e| format!("{}: {e}", from.display()))?;
            if n == 0 {
                return Ok(());
            }
            dest.write_all(&chunk[..n])
                .map_err(|e| format!("{to}: {e}"))?;
            copied(n as u64);
        }
    }

    fn put(&self, item: &Item, path: &str, copied: &mut dyn FnMut(u64)) -> Result<(), String> {
        let source = self.source.join(&item.rel);
        let dest = self.path(path);
        match item.kind {
            Kind::Dir => make_dir(&dest)?,
            Kind::File => self.copy(&source, &dest, copied)?,
            Kind::Link => {
                let link =
                    fs::read_link(&source).map_err(|e| format!("{}: {e}", source.display()))?;
                symlink(link, &dest).map_err(|e| format!("{dest}: {e}"))?;
            }
        }
        if item.kind != Kind::Link {
            let mode = fs::metadata(&source)
                .map_err(|e| format!("{}: {e}", source.display()))?
                .permissions()
                .mode();
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|e| format!("{dest}: {e}"))?;
        }
        self.owner.map_or(Ok(()), |owner| give(&dest, owner))
    }

    /// The bytes copied, or `None` when the root records `new` installed
    /// already.
    pub fn run(&self, new: &Manifest, copied: &mut dyn FnMut(u64)) -> Result<Option<u64>, String> {
        let recorded = slopos_tree_core::manifest_path(self.name);
        let old = if self.walk_dirs(MANIFEST_DIR, false)? {
            read_regular(&self.path(&recorded))?
                .map(|text| {
                    Manifest::parse(&text).map_err(|e| {
                        format!("{recorded} on the root, line {}: not a manifest", e.line)
                    })
                })
                .transpose()?
        } else {
            None
        };
        if old
            .as_ref()
            .is_some_and(|old| old.is_finished() && old.identity == new.identity)
        {
            return Ok(None);
        }
        let steps =
            plan(&mut Target(self.root), self.guest, old.as_ref(), new).map_err(|conflicts| {
                format!(
                    "the root holds what installing {} at {} would overwrite:\n  {}\nmove it aside",
                    self.name,
                    self.guest,
                    conflicts.0.join("\n  ")
                )
            })?;
        self.walk_dirs(MANIFEST_DIR, true)?;
        self.record(&self.path(&recorded), &steps.during)?;
        let mut total = 0;
        let mut count = |n| {
            total += n;
            copied(n);
        };
        for step in &steps.steps {
            match step {
                Step::Remove(path) => {
                    fs::remove_file(self.path(path)).map_err(|e| format!("{path}: {e}"))?
                }
                Step::RemoveDir(path) => {
                    fs::remove_dir(self.path(path)).map_err(|e| format!("{path}: {e}"))?
                }
                Step::MakeDir(path) => {
                    make_dir(&self.path(path))?;
                    if let Some(owner) = self.owner {
                        give(&self.path(path), owner)?;
                    }
                }
                Step::Install { item, path } => self.put(item, path, &mut count)?,
            }
        }
        self.record(&self.path(&recorded), &steps.done)?;
        Ok(Some(total))
    }
}
