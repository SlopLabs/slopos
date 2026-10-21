//! `/run/sessions`: a file per session, named by its login tag, which init
//! writes through a sibling and a rename and takes away when the session
//! ends, as logind keeps `/run/systemd/sessions`. Anyone reads it: it says who
//! is logged in where, and `getlogin` reads its caller's name there. Nothing
//! decides on it; privd hears the table from init itself.
//!
//! ```text
//! UID=1000
//! USER=user
//! CLASS=user
//! ACTIVE=1
//! LOCKED=0
//! LEADER=42
//! TTY=/dev/pts/3
//! ```

use std::path::{Path, PathBuf};

pub use slopos_priv_core::sessions::Class;

pub const DIR: &str = "/run/sessions";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub tag: u32,
    pub uid: u32,
    pub user: String,
    pub class: Class,
    /// Its desktop is the one in front.
    pub active: bool,
    pub locked: bool,
    /// The process that leads it, while it runs.
    pub leader: Option<u32>,
    pub tty: Option<String>,
}

impl Entry {
    pub fn render(&self) -> String {
        let mut out = format!(
            "UID={}\nUSER={}\nCLASS={}\nACTIVE={}\nLOCKED={}\n",
            self.uid,
            self.user,
            self.class.word(),
            u8::from(self.active),
            u8::from(self.locked)
        );
        if let Some(leader) = self.leader {
            out += &format!("LEADER={leader}\n");
        }
        if let Some(tty) = &self.tty {
            out += &format!("TTY={tty}\n");
        }
        out
    }

    pub fn parse(tag: u32, text: &str) -> Option<Entry> {
        let value = |key: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        };
        let flag = |key: &str| match value(key) {
            Some("1") => Some(true),
            Some("0") => Some(false),
            _ => None,
        };
        Some(Entry {
            tag,
            uid: value("UID")?.parse().ok()?,
            user: value("USER")?.to_owned(),
            class: Class::parse(value("CLASS")?)?,
            active: flag("ACTIVE")?,
            locked: flag("LOCKED")?,
            leader: value("LEADER").and_then(|pid| pid.parse().ok()),
            tty: value("TTY").map(str::to_owned),
        })
    }
}

fn path(tag: u32) -> PathBuf {
    Path::new(DIR).join(tag.to_string())
}

/// Every session as init last wrote it, by tag.
pub fn read() -> Vec<Entry> {
    let Ok(dir) = std::fs::read_dir(DIR) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = dir
        .flatten()
        .filter_map(|file| {
            let tag = file.file_name().to_str()?.parse().ok()?;
            Entry::parse(tag, &std::fs::read_to_string(file.path()).ok()?)
        })
        .collect();
    entries.sort_by_key(|entry| entry.tag);
    entries
}

/// The session the caller's login tag names.
pub fn mine() -> Option<Entry> {
    let tag = crate::syscall::core::login_tag();
    Entry::parse(tag, &std::fs::read_to_string(path(tag)).ok()?)
}

/// Lay out the directory, uid 0's and readable by everyone, keeping only
/// the entries of the sessions `kept` names, which run on across a
/// re-execution and are written again.
pub fn lay_out(kept: &[u32]) {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::create_dir_all(DIR).is_ok() {
        let _ = std::fs::set_permissions(DIR, std::fs::Permissions::from_mode(0o755));
    }
    let Ok(dir) = std::fs::read_dir(DIR) else {
        return;
    };
    for file in dir.flatten() {
        let tag = file.file_name().to_str().and_then(|name| name.parse().ok());
        if !tag.is_some_and(|tag| kept.contains(&tag)) {
            let _ = std::fs::remove_file(file.path());
        }
    }
}

pub fn write(entry: &Entry) {
    let temp = Path::new(DIR).join(format!(".{}", entry.tag));
    if std::fs::write(&temp, entry.render()).is_ok() {
        let _ = std::fs::rename(&temp, path(entry.tag));
    }
}

pub fn remove(tag: u32) {
    let _ = std::fs::remove_file(path(tag));
}
