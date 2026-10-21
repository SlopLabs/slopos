//! The new root, mounted while it is filled: the directories a SlopOS root
//! has with the owners the ownership map gives them, its base mount points
//! sealed, the medium's toolchain and apps installed at `/usr/local` by the
//! rule host trees follow, and its clone seeded at `/src`, the first
//! person's.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::Path;

use slopos_abi::fs::inode_flags::FS_IMMUTABLE_FL;
use slopos_abi::fs::{ROOT_LAYOUT, SESSION_GID, SESSION_UID};
use slopos_boot_core::layout;
use slopos_tree_core::{Kind, Manifest};

use super::Medium;
use crate::root_layout::Owners;
use crate::syscall::fs::{inode_flags, mount, set_inode_flags, sync, umount2};
use crate::tree::{self, create_new, give, make_dir, write_durably};

/// Where the new root's mount point is made: memory, which the next boot does
/// not see.
const MOUNT_PARENT: &str = "/tmp";
pub(super) const EXT4_ROOT_INODE: u64 = 2;
const SLOT_STATE: &str = "/var/lib/slopos/slots";
pub const LOCAL: &str = "/usr/local";
pub const SOURCE: &str = "/src";
const CLONE_CONFIG: &str = "slopos/.git/config";
const COPY_CHUNK: usize = 1 << 20;
const PROGRESS_EVERY: u64 = 256 << 20;

/// The new root, mounted in a directory made under a name nobody could
/// foresee, so no planted link redirects the mount, and that only uid 0 may
/// search, so the session, which owns a kept root's `/usr/local`, cannot turn
/// a directory the install writes through into a link. Unmounted on drop.
pub struct Mounted {
    private: String,
    dir: String,
}

impl Mounted {
    pub fn at(node: &str) -> Result<Mounted, String> {
        let name: String = super::disk::random_bytes()?[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let private = format!("{MOUNT_PARENT}/installer-{name}");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&private)
            .map_err(|e| format!("{private}: {e}"))?;
        let dir = format!("{private}/root");
        let mounted = fs::create_dir(&dir)
            .map_err(|e| format!("{dir}: {e}"))
            .and_then(|()| {
                mount(node.as_bytes(), dir.as_bytes(), b"ext4", 0)
                    .map_err(|e| format!("mounting {node} at {dir}: {e:?}"))
            });
        if let Err(e) = mounted {
            let _ = fs::remove_dir(&dir);
            let _ = fs::remove_dir(&private);
            return Err(e);
        }
        let mounted = Mounted { private, dir };
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

    /// The directory `path`, made where the root lacks it as `owner`'s with
    /// `mode`. One the root holds keeps its owner and mode.
    fn owned_dir(&self, path: &str, owner: (u32, u32), mode: u16) -> Result<(), String> {
        let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        if !parent.is_empty() {
            self.dir(parent)?;
        }
        if self.walk_dirs(path, false)? {
            return Ok(());
        }
        make_dir(&self.path(path))?;
        give(&self.path(path), owner)?;
        fs::set_permissions(self.path(path), fs::Permissions::from_mode(mode.into()))
            .map_err(|e| format!("{path}: {e}"))
    }

    fn unmount(&self) -> Result<(), String> {
        sync().map_err(|e| format!("sync: {e:?}"))?;
        let path = CString::new(self.dir.as_str()).map_err(|_| "a NUL in the mount point")?;
        umount2(path.as_ptr(), 0).map_err(|e| format!("unmounting {}: {e:?}", self.dir))?;
        let _ = fs::remove_dir(&self.dir);
        let _ = fs::remove_dir(&self.private);
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

/// The directories every root has, owned as the root's record has them: on a
/// new root, which it records, every tree the first person's. The base's
/// mount points sealed. What the record says.
pub fn lay_out(root: &Mounted, new: bool) -> Result<Owners, String> {
    let top = Path::new(&root.dir);
    let owners = match new {
        true => Owners::ONE_PERSON,
        false => Owners::of(top),
    };
    for dir in ROOT_LAYOUT.iter().filter(|dir| dir.path != "/") {
        root.owned_dir(dir.path, owners.owner(dir.path), dir.mode)?;
    }
    if let Some(home) = owners.home() {
        root.owned_dir(home.path, (home.uid, home.gid), home.mode)?;
    }
    for dir in slopos_abi::fs::BASE_DIRS {
        root.owned_dir(dir, (0, 0), 0o755)?;
        seal(root, dir)?;
    }
    if new {
        owners
            .write(top)
            .map_err(|e| format!("the owners record: {e}"))?;
    }
    Ok(owners)
}

/// A new root's accounts: `name`, its first administrator, whom the desktop
/// logs in, with the password `password` is the hash of, and the user
/// database written from them.
pub fn make_people(root: &Mounted, name: &str, password: Option<String>) -> Result<(), String> {
    let accounts = slopos_accounts_core::Accounts::first(name, password)
        .map_err(|e| format!("{name}: {e}"))?;
    let store = crate::accounts::Store::write(Path::new(&root.dir))?;
    store.record(&accounts)?;
    store.sync()
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

/// Install `source` at `path`, the session's.
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
                .map_err(|e| format!("{dest}: {e}"))?;
        }
        Kind::File => copy_file(source, &dest, progress)?,
        Kind::Link => {
            let link = fs::read_link(source).map_err(|e| format!("{}: {e}", source.display()))?;
            symlink(link, &dest).map_err(|e| format!("{dest}: {e}"))?;
        }
    }
    give(&dest, (SESSION_UID, SESSION_GID))
}

/// Install the medium's tree `name`, the `what` it says it installs, from
/// `source` at `/usr/local` as `holder`'s, keeping whatever the root's user
/// put beside what an earlier install left, its manifest `keeper`'s.
fn install_tree(
    root: &Mounted,
    name: &str,
    what: &str,
    source: &Path,
    new: &Manifest,
    (holder, keeper): ((u32, u32), (u32, u32)),
) -> Result<(), String> {
    let mut progress = Progress {
        copied: 0,
        reported: 0,
    };
    let installed = tree::Install {
        root: &root.dir,
        name,
        guest: LOCAL,
        manifests: slopos_tree_core::MANIFEST_DIR,
        source,
        owner: Some(holder),
        keeper: Some(keeper),
        remedy: "move it aside, or format the root",
    }
    .run(new, &mut |n| progress.add(n))?;
    match installed {
        Some(bytes) => println!(
            "installer: installed {what} at {LOCAL} ({})",
            super::disk::human(bytes)
        ),
        None => println!("installer: {LOCAL} already holds this medium's {what}"),
    }
    Ok(())
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
        give(&path, (SESSION_UID, SESSION_GID))?;
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

/// Fill the root from the medium: `/usr/local` with the toolchain and the
/// apps by their manifests, as whoever `owners` says holds it, `/src` once.
pub fn fill(
    root: &Mounted,
    medium: &Medium,
    remote: Option<&str>,
    owners: &Owners,
) -> Result<(), String> {
    let holder = owners.owner(LOCAL);
    if !medium.payload_has(LOCAL) {
        println!("installer: the medium carries no toolchain");
    } else {
        let recorded = slopos_tree_core::manifest_path(slopos_tree_core::TOOLCHAIN);
        let text = fs::read_to_string(medium.payload_path(&recorded))
            .map_err(|e| format!("the medium's manifest of {LOCAL}: {e}"))?;
        let new = Manifest::parse(&text)
            .map_err(|e| format!("the medium's manifest of {LOCAL}, line {}", e.line))?;
        let source = medium.payload_path(LOCAL);
        install_tree(
            root,
            slopos_tree_core::TOOLCHAIN,
            "the toolchain",
            &source,
            &new,
            (holder, (0, 0)),
        )?;
    }
    let apps = medium.path(layout::MEDIUM_APPS);
    let new = tree::scan(&apps)?;
    install_tree(
        root,
        slopos_tree_core::APPS,
        "the apps",
        &apps,
        &new,
        (holder, holder),
    )?;
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
