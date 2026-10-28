//! App entries, which the dock launches: `share/slopos/apps/<id>.app`, as
//! the XDG data directories rank them, an extension's replacing none.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use slopos_abi::fs::O_NOFOLLOW;
use slopos_abi::syscall::posix::{O_NOCTTY, O_NONBLOCK};
use slopos_extension_core::declare::mount_point;

#[derive(Debug)]
pub struct Share {
    pub dir: PathBuf,
    /// An extension's declared apps, the only ids it shows.
    pub only: Option<Vec<String>>,
}

impl Share {
    fn all(dir: impl Into<PathBuf>) -> Share {
        Share {
            dir: dir.into(),
            only: None,
        }
    }
}

/// Each extension's share by name, the base's, `/usr/local`'s, then the
/// person's: a later one's entry replaces an earlier's with its id.
pub fn shares(home: Option<&str>) -> Vec<Share> {
    let mut shares: Vec<Share> = crate::activation::apps()
        .into_iter()
        .map(|(name, apps)| Share {
            dir: Path::new(&mount_point(&name)).join("share"),
            only: Some(apps),
        })
        .collect();
    shares.push(Share::all("/usr/share"));
    shares.push(Share::all("/usr/local/share"));
    shares.extend(home.map(|home| Share::all(Path::new(home).join(".local/share"))));
    shares
}

/// Larger than any entry, so a stray file is not read whole.
const ENTRY_MAX: u64 = 4096;
/// More than a share ever holds, so a directory of millions is not listed
/// whole.
const ENTRIES_PER_SHARE_MAX: usize = 64;
/// Well past the size an icon is drawn at, so a huge one is refused before
/// it is decoded.
const ICON_BYTES_MAX: u64 = 1 << 20;
const ICON_PIXELS_MAX: usize = 256 * 256;

/// The bytes of the regular file at `path`, if it holds at most `max`. The
/// session writes these files and the compositor reads them, so a link, a
/// FIFO or a huge file there must neither redirect, block nor exhaust it.
pub fn read_bounded(path: &Path, max: u64) -> Option<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((O_NONBLOCK | O_NOCTTY) as i32 | O_NOFOLLOW as i32)
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

/// The PNG at `path`, read as an entry is.
pub fn read_icon(path: &Path) -> Option<slopos_image::Image> {
    let bytes = read_bounded(path, ICON_BYTES_MAX)?;
    let options = slopos_image::DecodeOptions {
        max_pixels: ICON_PIXELS_MAX,
        ..slopos_image::DecodeOptions::default()
    };
    slopos_image::decode(&bytes, options).ok()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: Option<PathBuf>,
    pub medium: bool,
}

/// Lines of `key=value`: `id`, `name`, the program it `exec`s, an `icon` in
/// `icons`, and `boot=medium` for an app of the install medium alone. A key
/// not known is passed over, but a `boot` not known makes no entry.
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
    let medium = match fields.get("boot") {
        None => false,
        Some(&"medium") => true,
        Some(_) => return None,
    };
    (fields.get("id") == Some(&id)).then(|| AppEntry {
        id: id.to_string(),
        name: name.to_string(),
        exec: exec.to_string(),
        icon,
        medium,
    })
}

/// The entry of the base's own share that runs `path`, whatever boot it is
/// shown on. The base is sealed, so no process can give a program another's
/// name here.
pub fn of_base(path: &str) -> Option<AppEntry> {
    read(&[Share::all("/usr/share")], true)
        .into_iter()
        .find(|entry| entry.exec == path)
}

/// The entries the dock pins on this boot for the person whose home is
/// `home`.
pub fn pinned(home: Option<&str>) -> Vec<AppEntry> {
    read(&shares(home), crate::privd::on_the_medium())
}

/// Every entry under `shares` shown on a boot, of the install medium when
/// `medium`, ordered by name.
pub fn read(shares: &[Share], medium: bool) -> Vec<AppEntry> {
    let mut entries = BTreeMap::new();
    for share in shares {
        let dir = share.dir.join("slopos/apps");
        let icons = share.dir.join("slopos/icons");
        let Ok(listing) = fs::read_dir(&dir) else {
            continue;
        };
        for file in listing.flatten().take(ENTRIES_PER_SHARE_MAX) {
            let path = file.path();
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".app"))
            else {
                continue;
            };
            if share
                .only
                .as_ref()
                .is_some_and(|only| !only.iter().any(|app| app == id) || entries.contains_key(id))
            {
                continue;
            }
            let entry = read_bounded(&path, ENTRY_MAX)
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| parse(id, &text, &icons))
                .filter(|entry| medium || !entry.medium);
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
