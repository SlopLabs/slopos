//! The ownership map on a root, as `slopos_abi::fs` states it: the
//! directories every root carries, and the trees the first person holds.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
use std::path::{Path, PathBuf};

use slopos_abi::fs::{
    OWNERS_RECORD, PERSON_TREES, ROOT_LAYOUT, RootDir, SESSION_GID, SESSION_HOME, SESSION_UID,
};

/// A person's command search path: the system's own tools, then their
/// `~/.local/bin` ahead of what `DEFAULT_PATH` puts after them.
pub fn search_path(home: &str) -> String {
    format!(
        "/bin:/sbin:{}/.local/bin:/opt/bin",
        home.trim_end_matches('/')
    )
}

const PERSON: (u32, u32) = (SESSION_UID, SESSION_GID);
const SYSTEM: (u32, u32) = (0, 0);

/// Which of the first person's trees they hold, as the owners record says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owners {
    held: [bool; PERSON_TREES.len()],
}

impl Owners {
    pub const ONE_PERSON: Owners = Owners {
        held: [true; PERSON_TREES.len()],
    };

    /// What a record grants the first person; a line naming another owner or
    /// tree grants nothing.
    pub fn parse(text: &str) -> Owners {
        let mut held = [false; PERSON_TREES.len()];
        let person = format!("{SESSION_UID}:{SESSION_GID}");
        for (owner, tree) in text.lines().filter_map(|line| line.split_once(' ')) {
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
    /// they are `alone` on the root, closed once it is shared.
    pub fn home(&self, alone: bool) -> Option<RootDir> {
        let mode = match alone {
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

    pub fn write(&self, root: &Path) -> std::io::Result<()> {
        write_durably(&under(root, OWNERS_RECORD), self.render().as_bytes())
    }
}

fn recorded(root: &Path) -> std::io::Result<Option<String>> {
    let path = under(root, OWNERS_RECORD);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(meta) if !meta.is_file() => Err(ErrorKind::InvalidData.into()),
        Ok(_) => fs::read_to_string(&path).map(Some),
    }
}

/// Make each directory of the map the root at `root` lacks, the first
/// person's home as their record and [`alone`] have it; a record that is
/// absent or cannot be read grants nothing and changes nothing.
pub fn lay_out(root: &Path) {
    let record = under(root, OWNERS_RECORD);
    let owners = match recorded(root) {
        Ok(Some(text)) => Owners::parse(&text),
        Ok(None) => {
            eprintln!(
                "init: {}: absent; nothing is the first person's",
                record.display()
            );
            return;
        }
        Err(e) => {
            eprintln!(
                "init: {}: {e}; nothing is the first person's",
                record.display()
            );
            return;
        }
    };
    let home = owners
        .home(true)
        .filter(|home| fs::symlink_metadata(under(root, home.path)).is_err())
        .and_then(|_| owners.home(alone(root)));
    for dir in ROOT_LAYOUT.into_iter().chain(home) {
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
            Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => return,
            Err(e) => eprintln!("init: {}: {e}", path.display()),
        }
    }
}

/// Whether the root at `root` knows one person at most, as its accounts
/// record says; one it cannot read shares the root.
pub fn alone(root: &Path) -> bool {
    crate::accounts::Store::read(root)
        .accounts()
        .is_ok_and(|accounts| accounts.people().len() <= 1)
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

/// The first person's home on the root at `root` closed to others, for a
/// second person: whether this changed it. Takes `DacOverride`.
pub fn close_home(root: &Path) -> Result<bool, String> {
    let home = under(root, SESSION_HOME);
    let meta = match fs::symlink_metadata(&home) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        other => other.map_err(|e| format!("{}: {e}", home.display()))?,
    };
    let mode = meta.mode() & 0o7777;
    if !meta.is_dir() || meta.uid() != SESSION_UID || mode & 0o077 == 0 {
        return Ok(false);
    }
    fs::set_permissions(&home, fs::Permissions::from_mode(mode & !0o077))
        .map_err(|e| format!("{}: {e}", home.display()))?;
    let _ = crate::syscall::fs::sync();
    Ok(true)
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
