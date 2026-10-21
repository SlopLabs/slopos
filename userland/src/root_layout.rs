//! The ownership map on a root, as `slopos_abi::fs` states it: the
//! directories every root carries, and the trees the first person holds.

use std::fs::{self, File, Metadata};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
use std::path::{Component, Path, PathBuf};

use slopos_abi::fs::{
    OWNERS_RECORD, PERSON_TREES, ROOT_LAYOUT, RootDir, SESSION_GID, SESSION_HOME, SESSION_UID,
};

/// The prefix everyone's software is installed in.
pub const LOCAL: &str = "/usr/local";

/// Made by init when [`LOCAL`] was the system's before this boot ran
/// anything as a person, so no descriptor the first person opened on it
/// for writing before the hand-over is open still.
pub const SETTLED: &str = "/run/slopos-local";

/// Whether [`LOCAL`] has been the system's since this boot began.
pub fn settled() -> bool {
    fs::symlink_metadata(SETTLED)
        .is_ok_and(|meta| meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

/// Note that [`LOCAL`] is the system's from this boot on.
pub fn settle() -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(SETTLED)
        .map(drop)
}

/// A person's command search path: the system's own tools, then their
/// `~/.local/bin` ahead of what `DEFAULT_PATH` puts after them.
pub fn search_path(home: &str) -> String {
    format!(
        "/bin:/sbin:{}/.local/bin:/usr/local/bin:/opt/bin",
        home.trim_end_matches('/')
    )
}

const PERSON: (u32, u32) = (SESSION_UID, SESSION_GID);
const SYSTEM: (u32, u32) = (0, 0);
/// Links followed before a name counts as naming nothing, as Linux's
/// `MAXSYMLINKS`.
const MAX_LINKS: usize = 40;
/// Written before a hand-over's first change and taken away once its record
/// is, so a boot finishes one cut short.
pub const SHARING: &str = "/var/lib/slopos/owners.sharing";

/// Which of the first person's trees they hold, as the owners record says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owners {
    held: [bool; PERSON_TREES.len()],
}

impl Owners {
    /// A root made for one person: every tree theirs.
    pub const ONE_PERSON: Owners = Owners {
        held: [true; PERSON_TREES.len()],
    };

    const NOBODY: Owners = Owners {
        held: [false; PERSON_TREES.len()],
    };

    /// What a record grants. One written before the map named the first
    /// person's home `/home`; a line naming another owner or another tree
    /// grants nothing.
    pub fn parse(text: &str) -> Owners {
        let mut held = [false; PERSON_TREES.len()];
        let person = format!("{SESSION_UID}:{SESSION_GID}");
        for (owner, tree) in text.lines().filter_map(|line| line.split_once(' ')) {
            let tree = if tree == "/home" { SESSION_HOME } else { tree };
            if let Some(at) = PERSON_TREES.iter().position(|t| *t == tree)
                && owner == person
            {
                held[at] = true;
            }
        }
        Owners { held }
    }

    pub fn render(&self) -> String {
        PERSON_TREES
            .iter()
            .zip(self.held)
            .filter(|(_, held)| *held)
            .map(|(tree, _)| format!("{SESSION_UID}:{SESSION_GID} {tree}\n"))
            .collect()
    }

    pub fn holds(&self, tree: &str) -> bool {
        PERSON_TREES
            .iter()
            .zip(self.held)
            .any(|(t, held)| held && *t == tree)
    }

    /// The first person's home where they hold it: open to others while
    /// they are alone on the root, closed once it is shared.
    pub fn home(&self) -> Option<RootDir> {
        let mode = match self.holds(LOCAL) {
            true => 0o755,
            false => 0o700,
        };
        self.holds(SESSION_HOME).then_some(RootDir {
            path: SESSION_HOME,
            uid: SESSION_UID,
            gid: SESSION_GID,
            mode,
        })
    }

    fn without(mut self, tree: &str) -> Owners {
        for (t, held) in PERSON_TREES.iter().zip(&mut self.held) {
            *held &= *t != tree;
        }
        self
    }

    /// Who `path` belongs to: the first person within a tree they hold, uid
    /// 0 elsewhere.
    pub fn owner(&self, path: &str) -> (u32, u32) {
        let within = |tree: &str| {
            path.strip_prefix(tree)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        };
        let held = PERSON_TREES
            .iter()
            .zip(self.held)
            .any(|(tree, held)| held && within(tree));
        if held { PERSON } else { SYSTEM }
    }

    /// The record of the root at `root`, `None` where it has none or it
    /// cannot be read.
    pub fn read(root: &Path) -> Option<Owners> {
        recorded(root)
            .ok()
            .flatten()
            .map(|text| Owners::parse(&text))
    }

    /// The record of the root at `root`; a recordless root's what init lays
    /// out at its next boot, and nothing where either cannot be read.
    pub fn of(root: &Path) -> Owners {
        match recorded(root) {
            Ok(Some(text)) => Owners::parse(&text),
            Ok(None) => Owners::unrecorded(root).unwrap_or(Owners::NOBODY),
            Err(_) => Owners::NOBODY,
        }
    }

    /// What a root without a record is laid out as: every tree the one
    /// person's, `/usr/local` the system's once the accounts name two.
    fn unrecorded(root: &Path) -> Option<Owners> {
        Some(match people(root)? > 1 {
            true => Owners::ONE_PERSON.without(LOCAL),
            false => Owners::ONE_PERSON,
        })
    }

    pub fn write(&self, root: &Path) -> std::io::Result<()> {
        write_durably(&under(root, OWNERS_RECORD), self.render().as_bytes())
    }
}

/// The text of the root's record, `None` where there is none.
fn recorded(root: &Path) -> std::io::Result<Option<String>> {
    let path = under(root, OWNERS_RECORD);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(meta) if !meta.is_file() => Err(ErrorKind::InvalidData.into()),
        Ok(_) => fs::read_to_string(&path).map(Some),
    }
}

/// How many people the root's accounts name, `None` when they cannot be
/// read.
fn people(root: &Path) -> Option<usize> {
    crate::accounts::Store::read(root)
        .accounts()
        .ok()
        .map(|accounts| accounts.people().len())
}

/// Make each directory of the map the root at `root` lacks, hand the first
/// person their trees once per root, and `/usr/local` to the system once a
/// second person has an account or a hand-over was begun. What the root's
/// record says now; a record that cannot be read, or accounts on a root
/// without one, grant nothing and change nothing.
pub fn lay_out(root: &Path) -> Owners {
    let record = under(root, OWNERS_RECORD);
    let (owners, recorded) = match recorded(root) {
        Ok(Some(text)) => (Owners::parse(&text), Some(text)),
        Ok(None) => match Owners::unrecorded(root) {
            Some(owners) => (owners, None),
            None => {
                eprintln!("init: the accounts cannot be read; the next boot lays out this root");
                return Owners::NOBODY;
            }
        },
        Err(e) => {
            eprintln!(
                "init: {}: {e}; nothing is the first person's",
                record.display()
            );
            return Owners::NOBODY;
        }
    };
    for dir in ROOT_LAYOUT.into_iter().chain(owners.home()) {
        let path = under(root, dir.path);
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        let (uid, gid) = owners.owner(dir.path);
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(e) = own(&path, &RootDir { uid, gid, ..dir }) {
                    eprintln!("init: {}: {e}", path.display());
                }
            }
            Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => return owners,
            Err(e) => eprintln!("init: {}: {e}", path.display()),
        }
    }
    match recorded {
        None => migrate(root, &owners),
        Some(text) if text != owners.render() => {
            if let Err(e) = owners.write(root) {
                eprintln!("init: {}: {e}", record.display());
            }
        }
        Some(_) => {}
    }
    let begun = sharing(root);
    if !owners.holds(LOCAL) {
        if begun {
            let _ = fs::remove_file(under(root, SHARING));
        }
        return owners;
    }
    if !begun && people(root).is_none_or(|n| n < 2) {
        return owners;
    }
    match share(root) {
        Ok(()) => owners.without(LOCAL),
        Err(e) => {
            eprintln!("init: {e}; the next boot tries again");
            owners
        }
    }
}

/// `path`, a directory only the session may use, for what it runs this boot.
pub fn make_runtime_dir(path: &str, uid: u32) {
    let Some(parent) = Path::new(path).parent() else {
        return;
    };
    let made = fs::create_dir_all(parent).and_then(|()| match fs::create_dir(path) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        other => other,
    });
    let owned = made.and_then(|()| {
        lchown(path, Some(uid), Some(uid))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    });
    if let Err(e) = owned {
        eprintln!("init: {path}: {e}");
    }
}

fn under(root: &Path, path: &str) -> PathBuf {
    root.join(path.trim_start_matches('/'))
}

fn own(path: &Path, dir: &RootDir) -> std::io::Result<()> {
    lchown(path, Some(dir.uid), Some(dir.gid))?;
    fs::set_permissions(path, fs::Permissions::from_mode(dir.mode.into()))
}

/// A root that ran everything as uid 0 holds the first person's work as uid
/// 0's. Hand them what uid 0 holds below their trees, set each directory of
/// the map as it says, and record that it was done.
fn migrate(root: &Path, owners: &Owners) {
    let mut refused = 0usize;
    for tree in PERSON_TREES.into_iter().filter(|tree| owners.holds(tree)) {
        let path = under(root, tree);
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        refused += hand_over_below(&path);
        if lchown(&path, Some(SESSION_UID), Some(SESSION_GID)).is_err() {
            refused += 1;
        }
    }
    for dir in ROOT_LAYOUT {
        let path = under(root, dir.path);
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        let (uid, gid) = owners.owner(dir.path);
        match own(&path, &RootDir { uid, gid, ..dir }) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => return,
            Err(_) => refused += 1,
        }
    }
    if !owners.holds(LOCAL) {
        refused += home_to_themselves(root) + take_local(root);
    }
    if refused > 0 {
        eprintln!("init: {refused} entries kept their owner; the next boot tries again");
        return;
    }
    // The record must not outlive the changes it records.
    let _ = crate::syscall::fs::sync();
    if let Err(e) = owners.write(root) {
        eprintln!("init: {}: {e}", under(root, OWNERS_RECORD).display());
    }
}

/// Every entry uid 0 holds below `top`, itself left alone, links never
/// followed, and nothing in a directory another uid holds. A file with more
/// than one name keeps its owner: its other name may be the system's. A
/// read-only volume mounted there is not this root's to change.
fn hand_over_below(top: &Path) -> usize {
    let mut refused = 0usize;
    let mut dirs: Vec<PathBuf> = vec![top.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            refused += 1;
            continue;
        };
        for entry in entries {
            let Ok(path) = entry.map(|entry| entry.path()) else {
                refused += 1;
                continue;
            };
            let Ok(meta) = fs::symlink_metadata(&path) else {
                refused += 1;
                continue;
            };
            if ![0, SESSION_UID].contains(&meta.uid()) || !meta.is_dir() && meta.nlink() > 1 {
                continue;
            }
            let handed = match meta.uid() {
                0 => lchown(&path, Some(SESSION_UID), Some(SESSION_GID)),
                _ => Ok(()),
            };
            match handed {
                Ok(()) if meta.is_dir() => dirs.push(path),
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => {}
                Err(_) => refused += 1,
            }
        }
    }
    refused
}

/// Ready the root at `root` for a second person: the first person's home
/// theirs alone, `/usr/local` the system's whatever lies below it, and the
/// record saying the first person no longer holds it. Takes `DacOverride`.
pub fn share(root: &Path) -> Result<(), String> {
    let begun = under(root, SHARING);
    write_durably(&begun, b"").map_err(|e| format!("{}: {e}", begun.display()))?;
    let refused = home_to_themselves(root) + take_local(root);
    if refused > 0 {
        return Err(format!(
            "{LOCAL}: {refused} entries could not be made the system's"
        ));
    }
    let _ = crate::syscall::fs::sync();
    Owners::of(root)
        .without(LOCAL)
        .write(root)
        .map_err(|e| format!("{}: {e}", under(root, OWNERS_RECORD).display()))?;
    let _ = fs::remove_file(begun);
    Ok(())
}

/// Whether a hand-over was begun on the root at `root` and its record not
/// yet written.
pub fn sharing(root: &Path) -> bool {
    fs::symlink_metadata(under(root, SHARING)).is_ok_and(|meta| meta.is_file())
}

/// The first person's home closed to everyone else, as every later
/// person's is made.
fn home_to_themselves(root: &Path) -> usize {
    let home = under(root, SESSION_HOME);
    let Ok(meta) = fs::symlink_metadata(&home) else {
        return 0;
    };
    let mode = meta.mode() & 0o7777;
    if !meta.is_dir() || meta.uid() != SESSION_UID || mode & 0o077 == 0 {
        return 0;
    }
    usize::from(fs::set_permissions(&home, fs::Permissions::from_mode(mode & !0o077)).is_err())
}

/// Every entry below and at `/usr/local` uid 0's and writable by uid 0
/// alone, links never followed: each directory before what is in it is
/// listed, so nothing the person runs puts a name there behind the walk. A
/// file with another name its holder is not the first person's or the
/// system's is someone else's, and only its name here goes, as does a link
/// that leads through what another uid may change. What could not be changed.
fn take_local(root: &Path) -> usize {
    let top = under(root, LOCAL);
    let mut refused = match fs::symlink_metadata(&top) {
        Ok(meta) if meta.is_dir() => usize::from(to_system(&top, &meta).is_none()),
        Ok(_) => 1,
        Err(e) if e.kind() == ErrorKind::NotFound => return 0,
        Err(_) => 1,
    };
    if refused > 0 {
        return refused;
    }
    let mut dirs = vec![top];
    let mut links = Vec::new();
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            refused += 1;
            continue;
        };
        for entry in entries {
            let Ok(path) = entry.map(|entry| entry.path()) else {
                refused += 1;
                continue;
            };
            let Ok(found) = fs::symlink_metadata(&path) else {
                refused += 1;
                continue;
            };
            let foreign =
                !found.is_dir() && found.nlink() > 1 && ![0, SESSION_UID].contains(&found.uid());
            if foreign {
                refused += usize::from(fs::remove_file(&path).is_err());
                continue;
            }
            match to_system(&path, &found) {
                Some(meta) if meta.is_dir() => dirs.push(path),
                Some(meta) if meta.file_type().is_symlink() => links.push(path),
                Some(_) => {}
                None => refused += 1,
            }
        }
    }
    for link in links.iter().filter(|link| !names_the_system_s(root, link)) {
        refused += usize::from(fs::remove_file(link).is_err());
    }
    refused
}

/// Whether `link` on the root at `root`, followed as the kernel follows it,
/// finds every name in a directory only uid 0 may change and ends at what
/// only uid 0 may change, a directory only within `/usr/local`, so nobody
/// else can make it lead anywhere else or put names beneath it.
fn names_the_system_s(root: &Path, link: &Path) -> bool {
    let (Some(dir), Some(name)) = (link.parent(), link.file_name()) else {
        return false;
    };
    let mut at = dir.to_path_buf();
    let mut rest = vec![PathBuf::from(name)];
    let mut followed = 0;
    while let Some(next) = rest.pop() {
        match next.components().next() {
            Some(Component::RootDir) => at = root.to_path_buf(),
            Some(Component::ParentDir) if at != root => {
                at.pop();
            }
            Some(Component::Normal(name)) => {
                if !only_uid_0_changes(&at) {
                    return false;
                }
                at.push(name);
                let Ok(meta) = fs::symlink_metadata(&at) else {
                    return false;
                };
                if meta.file_type().is_symlink() {
                    followed += 1;
                    let Ok(target) = fs::read_link(&at) else {
                        return false;
                    };
                    if followed > MAX_LINKS {
                        return false;
                    }
                    at.pop();
                    rest.extend(
                        target
                            .components()
                            .rev()
                            .map(|part| PathBuf::from(part.as_os_str())),
                    );
                }
            }
            _ => {}
        }
    }
    only_uid_0_changes(&at) && (!at.is_dir() || at.starts_with(under(root, LOCAL)))
}

fn only_uid_0_changes(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

/// `path` uid 0's and, unless a link, writable by uid 0 alone; what it is
/// then, since until it was uid 0's its holder could change its mode.
fn to_system(path: &Path, found: &Metadata) -> Option<Metadata> {
    let settled = |done: std::io::Result<()>| match done {
        Err(e) if e.kind() != ErrorKind::ReadOnlyFilesystem => None,
        _ => Some(()),
    };
    if (found.uid(), found.gid()) != SYSTEM {
        settled(lchown(path, Some(0), Some(0)))?;
    }
    let meta = fs::symlink_metadata(path).ok()?;
    let mode = meta.mode() & 0o7777;
    if !meta.file_type().is_symlink() && mode & 0o022 != 0 {
        settled(fs::set_permissions(
            path,
            fs::Permissions::from_mode(mode & !0o022),
        ))?;
    }
    Some(meta)
}

fn write_durably(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".part");
    let mut file = File::create(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    if let Some(dir) = path.parent() {
        File::open(dir)?.sync_all()?;
    }
    Ok(())
}
