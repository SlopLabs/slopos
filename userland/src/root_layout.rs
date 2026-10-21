//! The ownership map on a root, as `slopos_abi::fs` states it: the directories
//! every root carries, and the session's trees.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, lchown};
use std::path::{Path, PathBuf};

use slopos_abi::fs::{
    OWNERS_RECORD, ROOT_LAYOUT, RootDir, SESSION_GID, SESSION_TREES, SESSION_UID,
};

/// Make each directory of the map the root at `root` lacks, and hand the
/// session its trees once per root.
pub fn lay_out(root: &Path) {
    for dir in ROOT_LAYOUT {
        let path = under(root, dir.path);
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(e) = own(&path, &dir) {
                    eprintln!("init: {}: {e}", path.display());
                }
            }
            Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => return,
            Err(e) => eprintln!("init: {}: {e}", path.display()),
        }
    }
    let record = under(root, OWNERS_RECORD);
    if fs::symlink_metadata(&record).is_err() {
        migrate(root, &record);
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

/// A root that ran everything as uid 0 holds the session's work as uid 0's.
/// Hand the session what uid 0 holds below its trees, set each directory of
/// the map as it says, and record that it was done.
fn migrate(root: &Path, record: &Path) {
    let mut refused = 0usize;
    for tree in SESSION_TREES {
        let path = under(root, tree);
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        refused += hand_over_below(&path);
        if !ROOT_LAYOUT.iter().any(|dir| dir.path == tree)
            && lchown(&path, Some(SESSION_UID), Some(SESSION_GID)).is_err()
        {
            refused += 1;
        }
    }
    for dir in ROOT_LAYOUT {
        let path = under(root, dir.path);
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        match own(&path, &dir) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::ReadOnlyFilesystem => return,
            Err(_) => refused += 1,
        }
    }
    if refused > 0 {
        eprintln!("init: {refused} entries kept their owner; the next boot tries again");
        return;
    }
    // The record must not outlive the changes it records.
    let _ = crate::syscall::fs::sync();
    let lines: String = SESSION_TREES
        .iter()
        .map(|tree| format!("{SESSION_UID}:{SESSION_GID} {tree}\n"))
        .collect();
    if let Err(e) = write_durably(record, lines.as_bytes()) {
        eprintln!("init: {}: {e}", record.display());
    }
}

/// Every entry uid 0 holds below `top`, itself left alone, links never
/// followed, and nothing in a directory another uid holds: another person's
/// home is theirs. A file with more than one name keeps its owner: its other
/// name may be the system's. A read-only volume mounted there is not this
/// root's to change.
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
