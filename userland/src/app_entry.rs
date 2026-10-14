//! App entries, what the dock launches: `share/slopos/apps/<id>.app`, lines of
//! `key=value` naming the app's `id`, its `name`, the program it `exec`s and
//! an `icon`, a PNG in the same tree's `share/slopos/icons/`.
//!
//! Entries are read from the base's `/usr/share`, then `/usr/local/share`,
//! then the person's `~/.local/share`; a later entry replaces an earlier one
//! with the same id, as the XDG data directories rank.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use slopos_abi::syscall::posix::O_NONBLOCK;

/// The directories holding entries, as `<share>/slopos/apps`, in the order a
/// later one replaces an earlier one's entry.
pub fn shares(home: Option<&str>) -> Vec<PathBuf> {
    let mut shares = vec![
        PathBuf::from("/usr/share"),
        PathBuf::from("/usr/local/share"),
    ];
    shares.extend(home.map(|home| Path::new(home).join(".local/share")));
    shares
}

/// Larger than any entry, so a stray file is not read whole.
const ENTRY_MAX: u64 = 4096;

/// The bytes of the regular file at `path`, if it holds at most `max`. The
/// session writes these files and the compositor reads them, so a FIFO or a
/// huge file there must neither block nor exhaust it.
pub fn read_bounded(path: &Path, max: u64) -> Option<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK as i32)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > max {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: Option<PathBuf>,
}

/// The entry `text` states as `id`, whose icon is in `icons`. Every line is
/// `key=value`; a key this reader does not know is passed over, so an entry
/// can say more than an older dock reads.
pub fn parse(id: &str, text: &str, icons: &Path) -> Option<AppEntry> {
    let mut fields = BTreeMap::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once('=')?;
        if fields.insert(key, value).is_some() {
            return None;
        }
    }
    let name = fields.get("name").filter(|name| !name.is_empty())?;
    let exec = fields.get("exec").filter(|exec| exec.starts_with('/'))?;
    let icon = match fields.get("icon") {
        None => None,
        Some(file) if !file.is_empty() && !file.contains('/') => Some(icons.join(file)),
        Some(_) => return None,
    };
    (fields.get("id") == Some(&id)).then(|| AppEntry {
        id: id.to_string(),
        name: name.to_string(),
        exec: exec.to_string(),
        icon,
    })
}

/// Every entry under `shares`, a later one replacing an earlier with its id,
/// ordered by name.
pub fn read(shares: &[PathBuf]) -> Vec<AppEntry> {
    let mut entries = BTreeMap::new();
    for share in shares {
        let dir = share.join("slopos/apps");
        let icons = share.join("slopos/icons");
        let Ok(listing) = fs::read_dir(&dir) else {
            continue;
        };
        for file in listing.flatten() {
            let path = file.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".app"))
            else {
                continue;
            };
            let entry = read_bounded(&path, ENTRY_MAX)
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse(id, &text, &icons));
            if let Some(entry) = entry {
                entries.insert(entry.id.clone(), entry);
            }
        }
    }
    let mut entries: Vec<AppEntry> = entries.into_values().collect();
    entries.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    entries
}
