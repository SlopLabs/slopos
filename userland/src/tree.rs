//! A tree installed onto a root from a directory, by `slopos-tree-core`'s
//! rule: the installer's toolchain and apps, init's apps on a live root, and
//! `tree-install`'s as the session.

use std::ffi::{CString, OsStr};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{
    DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, lchown, symlink,
};
use std::path::{Component, Path, PathBuf};

use slopos_abi::UserFsStat;
use slopos_abi::fs::{
    AT_SYMLINK_NOFOLLOW, O_DIRECTORY, O_NOFOLLOW, O_RDONLY, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG,
};
use slopos_abi::syscall::{O_CLOEXEC, O_NONBLOCK};
use slopos_slibc::pal::{Pal, Sys};
use slopos_tls_core::hash::{Hash, Sha256};

use crate::syscall::process::geteuid;
use slopos_tree_core::{Item, Kind, MANIFEST_DIR, Manifest, Root, State, Step, UNFINISHED, plan};

const COPY_CHUNK: usize = 1 << 20;
const OWNER_WRITES: u32 = 0o7755;

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
pub fn create_new(path: impl AsRef<Path>) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

/// `bytes` at `path`, `owner`'s when given, through a flushed sibling and a
/// rename, so a crash leaves the old file or the new one.
fn replace(path: &str, bytes: &[u8], owner: Option<(u32, u32)>) -> std::io::Result<()> {
    let temp = format!("{path}.part");
    match fs::remove_file(&temp) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    create_new(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| owner.map_or(Ok(()), |(uid, gid)| lchown(&temp, Some(uid), Some(gid))))
        .and_then(|()| fs::rename(&temp, path))
}

/// `bytes` at `path` through a flushed sibling and a rename, so a crash
/// leaves the old file or the new one.
pub fn write_durably(path: &str, bytes: &[u8]) -> Result<(), String> {
    replace(path, bytes, None).map_err(|e| format!("{path}: {e}"))
}

/// The manifest of the tree `name`, made for `keeper` to rewrite in place
/// where it may not make one: an install begun with nothing recorded. Kept
/// when there is one.
pub fn hand_over(name: &str, keeper: (u32, u32)) -> Result<(), String> {
    let path = slopos_tree_core::manifest_path(name);
    let empty = Manifest {
        identity: UNFINISHED.to_string(),
        at: None,
        items: Vec::new(),
    };
    make_dir(MANIFEST_DIR)?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            replace(&path, empty.render().as_bytes(), Some(keeper))
                .map_err(|e| format!("{path}: {e}"))
        }
        Err(e) => Err(format!("{path}: {e}")),
        Ok(_) => Ok(()),
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
            if !slopos_tree_core::valid_rel(&rel) {
                return Err(format!("{}: a manifest cannot name it", path.display()));
            }
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
                    digest.update(&meta.len().to_le_bytes());
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
                    let target = target.as_os_str().as_encoded_bytes();
                    digest.update(&(target.len() as u64).to_le_bytes());
                    digest.update(target);
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
    Ok(Manifest {
        identity,
        at: None,
        items,
    })
}

/// Where uid 0 copies a tree it installs for someone else: a directory only
/// it may enter, which one install through privd at a time uses.
const STAGING: &str = "/var/lib/slopos/tree-install";

pub struct Staged(PathBuf);

impl Staged {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn io_error(errno: slopos_slibc::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno.0)
}

fn c_name(name: &OsStr) -> std::io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| ErrorKind::InvalidInput.into())
}

fn open_at(dir: &File, name: &OsStr, flags: u32) -> std::io::Result<File> {
    let name = c_name(name)?;
    let flags = flags | O_NOFOLLOW | O_CLOEXEC as u32 | O_NONBLOCK as u32;
    let fd =
        Sys::openat(dir.as_raw_fd(), name.as_ptr().cast(), flags as i32, 0).map_err(io_error)?;
    // SAFETY: the descriptor was just opened, and the file takes it over.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// With `search`, whether everyone may search it; otherwise read it, and
/// search a directory too.
fn everyone_may(meta: &fs::Metadata, search: bool) -> bool {
    let want = match (search, meta.is_dir()) {
        (true, _) => 0o1,
        (false, true) => 0o5,
        (false, false) => 0o4,
    };
    meta.mode() & want == want
}

/// Whether a link `depth` directories into a tree, naming `target`, stays in
/// the tree: relative, and climbing no higher than its top before it names
/// anything.
fn stays_within(target: &Path, depth: usize) -> bool {
    let mut climbed = 0;
    let mut named = false;
    for part in target.components() {
        match part {
            Component::ParentDir if !named => climbed += 1,
            Component::Normal(_) => named = true,
            Component::CurDir => {}
            _ => return false,
        }
    }
    climbed <= depth
}

/// A copy of the tree at `source` that uid 0 installs for someone else:
/// reached from `/` through directories alone, each entry opened without
/// following a link and copied as its descriptor reads it, so neither a link
/// nor an entry changed during the copy reaches what not everyone may read,
/// and every link left naming something within the tree. Anything else in
/// the tree refuses it. The copy of a run cut short is taken away first.
pub fn stage(source: &Path) -> Result<Staged, String> {
    let shown = |e: std::io::Error| format!("{}: {e}", source.display());
    let absolute = std::env::current_dir().map_err(shown)?.join(source);
    let mut walked = PathBuf::from("/");
    let mut dir = File::open("/").map_err(shown)?;
    for part in absolute.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                walked.push(name);
                dir = open_at(&dir, name, O_RDONLY | O_DIRECTORY).map_err(shown)?;
                if !dir.metadata().is_ok_and(|meta| everyone_may(&meta, true)) {
                    return Err(format!(
                        "{}: not everyone may reach it, and only what everyone may read goes in for everyone",
                        source.display()
                    ));
                }
            }
            _ => return Err(format!("{}: a tree is named without ..", source.display())),
        }
    }
    if Path::new(STAGING).starts_with(&walked) {
        return Err(format!(
            "{} holds {STAGING}, where it is copied",
            source.display()
        ));
    }
    let staging = |e: std::io::Error| format!("{STAGING}: {e}");
    match fs::remove_dir_all(STAGING) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(staging(e)),
        _ => {}
    }
    DirBuilder::new()
        .mode(0o700)
        .create(STAGING)
        .map_err(staging)?;
    let staged = Staged(PathBuf::from(STAGING));
    copy_at(&dir, source, staged.path(), 0)?;
    Ok(staged)
}

/// What the directory `dir`, `depth` directories into the tree as `from`,
/// holds as everyone may read it, copied into `to`.
fn copy_at(dir: &File, from: &Path, to: &Path, depth: usize) -> Result<(), String> {
    let refused = |what: &Path| format!("{}: not everyone may read it", what.display());
    if !dir.metadata().is_ok_and(|meta| everyone_may(&meta, false)) {
        return Err(refused(from));
    }
    let names = crate::syscall::fs::names(dir.as_raw_fd())
        .map_err(|e| format!("{}: {e:?}", from.display()))?;
    for name in names {
        let name = std::ffi::OsString::from_vec(name);
        let (source, dest) = (from.join(&name), to.join(&name));
        let shown = |e: std::io::Error| format!("{}: {e}", source.display());
        let c = c_name(&name).map_err(shown)?;
        let mut stat = UserFsStat::default();
        Sys::fstatat(
            dir.as_raw_fd(),
            c.as_ptr().cast(),
            &mut stat,
            AT_SYMLINK_NOFOLLOW,
        )
        .map_err(io_error)
        .map_err(shown)?;
        match stat.st_mode & S_IFMT {
            S_IFLNK => {
                let mut target = vec![0u8; slopos_abi::fs::USER_PATH_MAX];
                let len = Sys::readlinkat(
                    dir.as_raw_fd(),
                    c.as_ptr().cast(),
                    target.as_mut_ptr(),
                    target.len(),
                )
                .map_err(io_error)
                .map_err(shown)?;
                let target = Path::new(OsStr::from_bytes(&target[..len]));
                if !stays_within(target, depth) {
                    return Err(format!(
                        "{} names {}, outside the tree",
                        source.display(),
                        target.display()
                    ));
                }
                symlink(target, &dest).map_err(shown)?;
            }
            S_IFDIR => {
                let sub = open_at(dir, &name, O_RDONLY | O_DIRECTORY).map_err(shown)?;
                fs::create_dir(&dest).map_err(shown)?;
                copy_at(&sub, &source, &dest, depth + 1)?;
                let mode = sub.metadata().map_err(shown)?.mode() & 0o7777 | 0o700;
                fs::set_permissions(&dest, fs::Permissions::from_mode(mode)).map_err(shown)?;
            }
            S_IFREG => {
                let mut file = open_at(dir, &name, O_RDONLY).map_err(shown)?;
                let meta = file.metadata().map_err(shown)?;
                if !meta.is_file() || !everyone_may(&meta, false) {
                    return Err(refused(&source));
                }
                let mut copy = create_new(&dest).map_err(shown)?;
                std::io::copy(&mut file, &mut copy).map_err(shown)?;
                copy.set_permissions(fs::Permissions::from_mode(meta.mode() & 0o7777 | 0o600))
                    .map_err(shown)?;
            }
            _ => {
                return Err(format!(
                    "{} is not a file, directory or link",
                    source.display()
                ));
            }
        }
    }
    Ok(())
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
/// mounted at `root` (empty for the running system's), its manifest kept in
/// `manifests`.
pub struct Install<'a> {
    pub root: &'a str,
    pub name: &'a str,
    pub guest: &'a str,
    pub manifests: &'a str,
    pub source: &'a Path,
    /// Who what the install makes belongs to; whoever installs when `None`.
    pub owner: Option<(u32, u32)>,
    /// Who the manifest belongs to: the session for a tree it installs
    /// again itself, uid 0 for one only uid 0 installs. `None` is whoever
    /// installs, who must keep a manifest already there.
    pub keeper: Option<(u32, u32)>,
    /// What a refusal tells the person to do with what is in the way.
    pub remedy: &'a str,
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

    /// Record `manifest` at `path` through a sibling and a rename, or in place
    /// where the installer may not make a sibling: as the session, in uid 0's
    /// directory.
    fn record(&self, path: &str, manifest: &Manifest) -> Result<(), String> {
        let text = manifest.render();
        match replace(path, text.as_bytes(), self.keeper) {
            Err(e) if e.kind() == ErrorKind::PermissionDenied => OpenOptions::new()
                .write(true)
                .truncate(true)
                .custom_flags(O_NOFOLLOW as i32)
                .open(path)
                .and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all())),
            done => done,
        }
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => format!("{path} does not exist, and only uid 0 may make it"),
            _ => format!("recording {path}: {e}"),
        })
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
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & OWNER_WRITES))
                .map_err(|e| format!("{dest}: {e}"))?;
        }
        self.owner.map_or(Ok(()), |owner| give(&dest, owner))
    }

    /// The manifest at `path` if uid 0 or this installer wrote it and it
    /// parses; otherwise the new tree unfinished, so the install takes back
    /// only the new tree's paths.
    fn recorded(&self, path: &str, new: &Manifest) -> Result<Option<Manifest>, String> {
        let meta = match fs::symlink_metadata(path) {
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{path}: {e}")),
            Ok(meta) if !meta.is_file() => return Err(format!("{path} is not a file")),
            Ok(meta) => meta,
        };
        let believed = meta.uid() == 0 || meta.uid() == geteuid();
        let read = believed
            .then(|| fs::read(path).ok())
            .flatten()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| Manifest::parse(&text).ok());
        Ok(Some(read.unwrap_or_else(|| {
            Manifest {
                identity: UNFINISHED.to_string(),
                at: None,
                items: new
                    .items
                    .iter()
                    .map(|item| Item {
                        size: None,
                        ..item.clone()
                    })
                    .collect(),
            }
        })))
    }

    /// Whether every entry `manifest` names is on the root as it names it.
    fn intact(&self, manifest: &Manifest) -> bool {
        manifest.items.iter().all(|item| {
            let path = format!("{}/{}", self.guest.trim_end_matches('/'), item.rel);
            fs::symlink_metadata(self.path(&path)).is_ok_and(|meta| {
                let kind = match item.kind {
                    Kind::Dir => meta.is_dir(),
                    Kind::File => meta.is_file(),
                    Kind::Link => meta.file_type().is_symlink(),
                };
                kind && item.size.is_none_or(|size| size == meta.len())
            })
        })
    }

    /// The bytes copied, or `None` when the root holds `new` already.
    pub fn run(&self, new: &Manifest, copied: &mut dyn FnMut(u64)) -> Result<Option<u64>, String> {
        let recorded = self.path(&format!("{}/{}", self.manifests, self.name));
        if !self.walk_dirs(self.manifests, false)? {
            return self.install(&recorded, None, new, copied).map(Some);
        }
        if self.keeper.is_none()
            && geteuid() != 0
            && let Ok(meta) = fs::symlink_metadata(&recorded)
            && meta.uid() != geteuid()
        {
            return Err(format!(
                "{recorded} is uid {}'s, which installs {} again",
                meta.uid(),
                self.name
            ));
        }
        let old = self.recorded(&recorded, new)?;
        if old
            .as_ref()
            .is_some_and(|old| old.installs(&new.identity, self.guest) && self.intact(old))
        {
            return Ok(None);
        }
        self.install(&recorded, old.as_ref(), new, copied).map(Some)
    }

    fn install(
        &self,
        recorded: &str,
        old: Option<&Manifest>,
        new: &Manifest,
        copied: &mut dyn FnMut(u64),
    ) -> Result<u64, String> {
        let steps = plan(&mut Target(self.root), self.guest, old, new).map_err(|conflicts| {
            format!(
                "the root holds what installing {} at {} would overwrite:\n  {}\n{}",
                self.name,
                self.guest,
                conflicts.0.join("\n  "),
                self.remedy
            )
        })?;
        self.walk_dirs(self.manifests, true)?;
        self.record(recorded, &steps.during)?;
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
        self.record(recorded, &steps.done)?;
        Ok(total)
    }
}
