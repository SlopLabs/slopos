//! A person's preferences, [`slopos_settings_core::prefs`], as their file
//! holds them: written by the person, through `prefs` or Settings, and read
//! by their desktop and, for the keyboard layout, by init.

use std::ffi::CString;
use std::os::unix::fs::MetadataExt;

use slopos_settings_core::prefs::{self, FILE_MAX, Prefs};

/// This process's home: `HOME` when it names an absolute path, else the one
/// the user database gives its uid.
pub fn own_home() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .filter(|home| home.starts_with('/'))
        .or_else(|| crate::accounts::home_of(crate::syscall::process::geteuid()))
}

pub fn own_path() -> Option<String> {
    own_home().map(|home| prefs::path(&home))
}

/// What `path` holds, every default where it holds nothing readable and a
/// line passed over where a byte is not text.
pub fn read(path: &str) -> Prefs {
    parsed(crate::regular_file::read(path, FILE_MAX, None))
}

fn parsed(bytes: Option<Vec<u8>>) -> Prefs {
    bytes.map_or_else(Prefs::default, |bytes| {
        Prefs::parse(&String::from_utf8_lossy(&bytes)).0
    })
}

pub fn read_own() -> Prefs {
    own_path().map_or_else(Prefs::default, |path| read(&path))
}

pub fn write_own(prefs: &Prefs) -> Result<(), String> {
    let path = own_path().ok_or("this process has no home to keep preferences in")?;
    write(&path, prefs)
}

pub fn write(path: &str, prefs: &Prefs) -> Result<(), String> {
    if let Some((dir, _)) = path.rsplit_once('/') {
        std::fs::create_dir_all(dir).map_err(|e| format!("{dir}: {e}"))?;
    }
    let c_path = CString::new(path).map_err(|e| e.to_string())?;
    crate::syscall::fs::write_durable(&c_path, prefs.render().as_bytes())
        .map_err(|e| format!("{path}: {e}"))
}

/// The preferences of the person `uid` at home in `home`, as one who is not
/// them reads them: only from a regular file of theirs.
pub fn of_person(home: &str, uid: u32) -> Prefs {
    parsed(crate::regular_file::read(
        &prefs::path(home),
        FILE_MAX,
        Some(uid),
    ))
}

/// Tells when a preferences file has changed by what `stat` says of it, for
/// readers that keep what they read: there is no change notification, and a
/// file edited by hand changes as surely as one Settings wrote.
pub struct Watch {
    path: String,
    seen: Option<Stamp>,
}

type Stamp = (u64, u64, i64, i64, i64);

fn stamp(path: &str) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((
        meta.dev(),
        meta.ino(),
        meta.size() as i64,
        meta.mtime(),
        meta.mtime_nsec(),
    ))
}

impl Watch {
    pub fn new(path: String) -> Watch {
        let seen = stamp(&path);
        Watch { path, seen }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether the file differs from when this last looked.
    pub fn changed(&mut self) -> bool {
        let now = stamp(&self.path);
        let changed = now != self.seen;
        self.seen = now;
        changed
    }
}
