//! The new root, mounted while it is filled: the directories a SlopOS root
//! has, its base mount points sealed, the medium's toolchain installed at
//! `/usr/local` by the rule host trees follow, and its clone seeded at `/src`.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_tree_core::{Kind, MANIFEST_DIR, Manifest, Root, State, Step, manifest_name, plan};

use super::Medium;
use crate::syscall::fs::{inode_flags, mount, set_inode_flags, sync, umount2};

/// Where the new root's mount point is made: memory, which the next boot does
/// not see.
const MOUNT_PARENT: &str = "/tmp";
pub(super) const EXT4_ROOT_INODE: u64 = 2;
/// The directories every root carries, as `scripts/build_fs_image.sh` lays
/// them out.
const ROOT_DIRS: [&str; 6] = ["/bin", "/sbin", "/etc", "/var", "/home", "/media"];
const SLOT_STATE: &str = "/var/lib/slopos/slots";
pub const TOOLCHAIN: &str = "/usr/local";
pub const SOURCE: &str = "/src";
const CLONE_CONFIG: &str = "slopos/.git/config";
const COPY_CHUNK: usize = 1 << 20;
const PROGRESS_EVERY: u64 = 256 << 20;

/// The new root, mounted on a directory this install made under a name
/// nobody could foresee, so no link planted ahead of it redirects the mount;
/// unmounted on drop.
pub struct Mounted {
    dir: String,
}

impl Mounted {
    pub fn at(node: &str) -> Result<Mounted, String> {
        let name: String = super::disk::random_bytes()?[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let dir = format!("{MOUNT_PARENT}/installer-root-{name}");
        fs::create_dir(&dir).map_err(|e| format!("{dir}: {e}"))?;
        mount(node.as_bytes(), dir.as_bytes(), b"ext4", 0)
            .map_err(|e| format!("mounting {node} at {dir}: {e:?}"))?;
        let mounted = Mounted { dir };
        let root =
            fs::symlink_metadata(&mounted.dir).map_err(|e| format!("{}: {e}", mounted.dir))?;
        if !root.is_dir() || std::os::unix::fs::MetadataExt::ino(&root) != EXT4_ROOT_INODE {
            return Err(format!("{} is not the root {node} holds", mounted.dir));
        }
        Ok(mounted)
    }

    fn path(&self, path: &str) -> String {
        format!("{}{path}", self.dir)
    }

    /// Whether `path` and each directory above it on the root is a directory,
    /// making those that are absent when `make`. Anything else on the way, a
    /// link a kept root holds above all, is refused rather than followed, so
    /// nothing the installer writes beneath `path` lands off the root.
    fn walk_dirs(&self, path: &str, make: bool) -> Result<bool, String> {
        let ends = path.match_indices('/').map(|(at, _)| at).skip(1);
        for end in ends.chain([path.len()]) {
            let dir = &path[..end];
            match fs::symlink_metadata(self.path(dir)) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Err(format!("{dir} on the root is not a directory")),
                Err(e) if e.kind() == ErrorKind::NotFound && make => {
                    fs::create_dir(self.path(dir)).map_err(|e| format!("{dir}: {e}"))?;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(format!("{dir}: {e}")),
            }
        }
        Ok(true)
    }

    fn dir(&self, path: &str) -> Result<(), String> {
        self.walk_dirs(path, true).map(drop)
    }

    fn unmount(&self) -> Result<(), String> {
        sync().map_err(|e| format!("sync: {e:?}"))?;
        let path = CString::new(self.dir.as_str()).map_err(|_| "a NUL in the mount point")?;
        umount2(path.as_ptr(), 0).map_err(|e| format!("unmounting {}: {e:?}", self.dir))?;
        let _ = fs::remove_dir(&self.dir);
        Ok(())
    }

    /// Write everything back and unmount, saying so if either fails.
    pub fn finish(self) -> Result<(), String> {
        let result = self.unmount();
        core::mem::forget(self);
        result
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = self.unmount();
    }
}

fn make_dir(path: &str) -> Result<(), String> {
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

/// Seal the directory `path` on the root: the base is mounted over it, and
/// the seal keeps the root from holding anything of its own beneath. Reached
/// a component at a time from the mount, following no link, since any
/// process can see the mount while the installer works.
fn seal(root: &Mounted, path: &str) -> Result<(), String> {
    let mount = CString::new(root.dir.as_str()).map_err(|_| "a NUL in the mount point")?;
    let mut dir = crate::syscall::fs::open_cstr(&mount, slopos_abi::fs::O_RDONLY)
        .map_err(|e| format!("{}: {e:?}", root.dir))?;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        let name = CString::new(name).map_err(|_| "a NUL in a path")?;
        dir = crate::syscall::fs::open_dir_nofollow(dir.raw(), &name)
            .map_err(|e| format!("{path} on the root: {e:?}"))?;
    }
    let flags = inode_flags(dir.raw()).map_err(|e| format!("{path}: {e:?}"))?;
    if flags & FS_IMMUTABLE_FL == 0 {
        set_inode_flags(dir.raw(), flags | FS_IMMUTABLE_FL)
            .map_err(|e| format!("sealing {path}: {e:?}"))?;
    }
    Ok(())
}

/// The directories every root has, and the base's mount points sealed.
pub fn lay_out(root: &Mounted) -> Result<(), String> {
    for dir in ROOT_DIRS {
        root.dir(dir)?;
    }
    for dir in slopos_abi::fs::BASE_DIRS {
        root.dir(dir)?;
        seal(root, dir)?;
    }
    Ok(())
}

/// A kept root's slots hold this install's system now, so what they said of
/// the last one goes, as `bootctl install` has it.
pub fn forget_slots(root: &Mounted) -> Result<(), String> {
    if !root.walk_dirs(SLOT_STATE, false)? {
        return Ok(());
    }
    for slot in slopos_boot_core::layout::SLOTS {
        let path = root.path(&format!("{SLOT_STATE}/{slot}"));
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{path}: {e}")),
        }
    }
    Ok(())
}

/// The new root as `lstat` reads it.
struct Target<'a>(&'a Mounted);

impl Root for Target<'_> {
    fn state(&mut self, path: &str) -> State {
        match fs::symlink_metadata(self.0.path(path)) {
            Err(_) => State::Absent,
            Ok(meta) if meta.file_type().is_symlink() => State::Link,
            Ok(meta) if meta.is_dir() => State::Dir,
            Ok(meta) if meta.is_file() => State::File,
            Ok(_) => State::Other,
        }
    }

    fn children(&mut self, dir: &str) -> Vec<String> {
        fs::read_dir(self.0.path(dir))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .collect()
    }
}

/// A file made where nothing is, a link included, which a plain create
/// would follow.
fn create_new(path: &str) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn write_durably(path: &str, bytes: &[u8]) -> Result<(), String> {
    let temp = format!("{path}.part");
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{temp}: {e}")),
    }
    create_new(&temp)
        .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
        .and_then(|()| fs::rename(&temp, path))
        .map_err(|e| format!("{path}: {e}"))
}

/// A file the root holds at `path`, if any; anything else there is refused.
fn read_regular(path: &str) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => fs::read_to_string(path)
            .map(Some)
            .map_err(|e| format!("{path}: {e}")),
        Ok(_) => Err(format!("{path} is not a file")),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{path}: {e}")),
    }
}

struct Progress {
    copied: u64,
    reported: u64,
}

impl Progress {
    fn add(&mut self, bytes: u64) {
        self.copied += bytes;
        if self.copied - self.reported >= PROGRESS_EVERY {
            self.reported = self.copied;
            println!("installer:   {} copied", super::disk::human(self.copied));
        }
    }
}

fn copy_file(from: &Path, to: &str, progress: &mut Progress) -> Result<(), String> {
    let mut source = File::open(from).map_err(|e| format!("{}: {e}", from.display()))?;
    let mut dest = create_new(to).map_err(|e| format!("{to}: {e}"))?;
    let mut chunk = vec![0u8; COPY_CHUNK];
    loop {
        let n = source
            .read(&mut chunk)
            .map_err(|e| format!("{}: {e}", from.display()))?;
        if n == 0 {
            break;
        }
        dest.write_all(&chunk[..n])
            .map_err(|e| format!("{to}: {e}"))?;
        progress.add(n as u64);
    }
    let mode = fs::metadata(from)
        .map_err(|e| format!("{}: {e}", from.display()))?
        .permissions()
        .mode();
    fs::set_permissions(to, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| format!("{to}: {e}"))
}

fn put(
    root: &Mounted,
    source: &Path,
    kind: Kind,
    path: &str,
    progress: &mut Progress,
) -> Result<(), String> {
    let dest = root.path(path);
    match kind {
        Kind::Dir => {
            make_dir(&dest)?;
            let mode = fs::metadata(source)
                .map_err(|e| format!("{}: {e}", source.display()))?
                .permissions()
                .mode();
            fs::set_permissions(&dest, fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|e| format!("{dest}: {e}"))
        }
        Kind::File => copy_file(source, &dest, progress),
        Kind::Link => {
            let link = fs::read_link(source).map_err(|e| format!("{}: {e}", source.display()))?;
            symlink(link, &dest).map_err(|e| format!("{dest}: {e}"))
        }
    }
}

/// Install the medium's tree at `guest` by the manifest it carries, keeping
/// whatever the root's user put beside what an earlier install left. `None`
/// when the root holds this tree already.
fn install_tree(root: &Mounted, medium: &Medium, guest: &str) -> Result<Option<u64>, String> {
    let name = manifest_name(guest);
    let recorded = format!("{MANIFEST_DIR}/{name}");
    let new_text = fs::read_to_string(medium.payload_path(&recorded))
        .map_err(|e| format!("the medium's manifest of {guest}: {e}"))?;
    let new = Manifest::parse(&new_text)
        .map_err(|e| format!("the medium's manifest of {guest}, line {}", e.line))?;
    root.walk_dirs(MANIFEST_DIR, false)?;
    let old = read_regular(&root.path(&recorded))?
        .map(|text| {
            Manifest::parse(&text)
                .map_err(|e| format!("{recorded} on the root, line {}: not a manifest", e.line))
        })
        .transpose()?;
    if old
        .as_ref()
        .is_some_and(|old| old.is_finished() && old.identity == new.identity)
    {
        return Ok(None);
    }
    let steps = plan(&mut Target(root), guest, old.as_ref(), &new).map_err(|conflicts| {
        format!(
            "the root holds what installing {guest} would overwrite:\n  {}\nmove it aside, or format the root",
            conflicts.0.join("\n  ")
        )
    })?;
    root.dir(MANIFEST_DIR)?;
    write_durably(&root.path(&recorded), steps.during.render().as_bytes())?;
    let mut progress = Progress {
        copied: 0,
        reported: 0,
    };
    for step in &steps.steps {
        match step {
            Step::Remove(path) => {
                fs::remove_file(root.path(path)).map_err(|e| format!("{path}: {e}"))?
            }
            Step::RemoveDir(path) => {
                fs::remove_dir(root.path(path)).map_err(|e| format!("{path}: {e}"))?
            }
            Step::MakeDir(path) => make_dir(&root.path(path))?,
            Step::Install { item, path } => put(
                root,
                &medium.payload_path(&format!("{guest}/{}", item.rel)),
                item.kind,
                path,
                &mut progress,
            )?,
        }
    }
    write_durably(&root.path(&recorded), steps.done.render().as_bytes())?;
    Ok(Some(progress.copied))
}

fn copy_tree(root: &Mounted, medium: &Medium, dir: &str, to: &str) -> Result<u64, String> {
    fn walk(root: &Mounted, from: &Path, to: &str, progress: &mut Progress) -> Result<(), String> {
        let mut names: Vec<_> = fs::read_dir(from)
            .map_err(|e| format!("{}: {e}", from.display()))?
            .filter_map(Result::ok)
            .collect();
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let source = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let dest = format!("{to}/{name}");
            let meta =
                fs::symlink_metadata(&source).map_err(|e| format!("{}: {e}", source.display()))?;
            let kind = if meta.file_type().is_symlink() {
                Kind::Link
            } else if meta.is_dir() {
                Kind::Dir
            } else {
                Kind::File
            };
            put(root, &source, kind, &dest, progress)?;
            if kind == Kind::Dir {
                walk(root, &source, &dest, progress)?;
            }
        }
        Ok(())
    }
    let mut progress = Progress {
        copied: 0,
        reported: 0,
    };
    let from = medium.payload_path(dir);
    put(root, &from, Kind::Dir, to, &mut progress)?;
    walk(root, &from, to, &mut progress)?;
    Ok(progress.copied)
}

/// Copy the medium's clone to the root, which has no `/src`, with its origin
/// at `remote`, and rename it into place whole, so an interrupted install
/// leaves no partial copy the next one would keep.
fn seed(root: &Mounted, medium: &Medium, remote: Option<&str>) -> Result<u64, String> {
    let staging = format!("{SOURCE}.installing");
    match fs::remove_dir_all(root.path(&staging)) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{staging}: {e}")),
    }
    let bytes = copy_tree(root, medium, SOURCE, &staging)?;
    if let Some(url) = remote {
        let path = root.path(&format!("{staging}/{CLONE_CONFIG}"));
        let config =
            fs::read_to_string(&path).map_err(|e| format!("{SOURCE}/{CLONE_CONFIG}: {e}"))?;
        write_durably(&path, with_origin(&config, url).as_bytes())?;
    }
    sync().map_err(|e| format!("sync: {e:?}"))?;
    fs::rename(root.path(&staging), root.path(SOURCE)).map_err(|e| format!("{SOURCE}: {e}"))?;
    Ok(bytes)
}

/// `config` with `[remote "origin"]` fetching from and pushing to `url`, as
/// `git remote set-url origin` leaves it.
fn with_origin(config: &str, url: &str) -> String {
    let mut out = Vec::new();
    let mut in_origin = false;
    let mut found = false;
    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_origin = trimmed == "[remote \"origin\"]";
            out.push(line.to_owned());
            if in_origin && !found {
                out.push(format!("\turl = {url}"));
                found = true;
            }
        } else if !(in_origin && trimmed.split('=').next().map(str::trim) == Some("url")) {
            out.push(line.to_owned());
        }
    }
    if !found {
        out.push("[remote \"origin\"]".to_owned());
        out.push(format!("\turl = {url}"));
        out.push("\tfetch = +refs/heads/*:refs/remotes/origin/*".to_owned());
    }
    out.join("\n") + "\n"
}

/// Fill the root from the medium: `/usr/local` by its manifest, `/src` once.
pub fn fill(root: &Mounted, medium: &Medium, remote: Option<&str>) -> Result<(), String> {
    if !medium.payload_has(TOOLCHAIN) {
        println!("installer: the medium carries no toolchain; /usr/local stays as it is");
    } else {
        match install_tree(root, medium, TOOLCHAIN)? {
            Some(bytes) => println!(
                "installer: installed the toolchain at /usr/local ({})",
                super::disk::human(bytes)
            ),
            None => println!("installer: /usr/local already holds this medium's toolchain"),
        }
    }
    if !medium.payload_has(SOURCE) {
        return Ok(());
    }
    if fs::symlink_metadata(root.path(SOURCE)).is_ok() {
        println!("installer: the root has a /src already; it stays as it is");
        return Ok(());
    }
    let bytes = seed(root, medium, remote)?;
    println!(
        "installer: seeded /src with the medium's clone ({})",
        super::disk::human(bytes)
    );
    if let Some(url) = remote {
        println!("installer: /src/slopos fetches from and pushes to {url}");
    }
    Ok(())
}
