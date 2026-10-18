//! The activations shown at `/opt/<name>`, read as init and privd hold an
//! extension to its record: the one shown now, by the generation its inodes
//! name, and its release, declaration and manifest, read from that
//! generation alone.

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use slopos_abi::extension::Label;
use slopos_abi::fs::{BASEFS_MAGIC, O_DIRECTORY, O_NOFOLLOW};
use slopos_abi::syscall::O_CLOEXEC;
use slopos_extension_core::ceiling::{self, NEVER};
use slopos_extension_core::declare::{Declaration, mount_point};
use slopos_extension_core::image::{DECLARE, MANIFEST, RELEASE};
use slopos_extension_core::record::Record;
use slopos_extension_core::release::Release;
use slopos_priv_core::Action;
use slopos_priv_core::DELEGABLE;

use crate::apps::extctl::store::Store;
use crate::syscall::fs as sys_fs;

fn open_dir(path: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags((O_DIRECTORY | O_NOFOLLOW) as i32 | O_CLOEXEC as i32)
        .open(path)
}

/// `/opt/<name>` opened, once it is an activation's mount rather than a
/// directory someone made there.
pub fn open(name: &str) -> Result<File, String> {
    let mount = mount_point(name);
    let dir = open_dir(&mount).map_err(|e| format!("{mount}: {e}"))?;
    match sys_fs::fstatfs(dir.as_raw_fd()) {
        Ok(stats) if stats.f_type == BASEFS_MAGIC => Ok(dir),
        _ => Err(format!("nothing is shown at {mount}")),
    }
}

/// The generation of a file of a basefs mount, the high half of its inode.
pub fn generation(file: &File) -> Option<u64> {
    let on_basefs = sys_fs::fstatfs(file.as_raw_fd()).is_ok_and(|s| s.f_type == BASEFS_MAGIC);
    on_basefs
        .then(|| file.metadata().ok().map(|meta| meta.ino() >> 32))
        .flatten()
}

/// `path`, below the mount point, of the activation of `generation` shown at
/// `/opt/<name>`.
pub fn read(name: &str, generation: u64, path: &str) -> Result<String, String> {
    let at = format!("{}{path}", mount_point(name));
    let mut file = File::open(&at).map_err(|e| format!("{at}: {e}"))?;
    if self::generation(&file) != Some(generation) {
        return Err(format!(
            "{} shows another activation than it did",
            mount_point(name)
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("{at}: {e}"))?;
    Ok(text)
}

/// Whether `path` is a file of the activation of `generation`, and that
/// activation is still the one shown at `/opt/<name>`.
pub fn holds(name: &str, generation: u64, path: &str) -> Result<(), String> {
    let root = open(name)?;
    let file = File::open(path).map_err(|e| format!("{path}: {e}"))?;
    let shown = Some(generation);
    if self::generation(&root) != shown || self::generation(&file) != shown {
        return Err(format!(
            "{path} is not a file of the activation it was declared by"
        ));
    }
    Ok(())
}

/// The extensions the store keeps that are shown now, by name: a mount at
/// `/opt/<name>` lists in no directory.
pub fn names() -> Vec<String> {
    let names = Store::open(false).and_then(|store| store.names());
    names
        .unwrap_or_default()
        .into_iter()
        .filter(|name| open(name).is_ok())
        .collect()
}

/// An extension's current image as the activation shown for it holds it.
pub struct Shown {
    pub generation: u64,
    pub declaration: Declaration,
    pub manifest: String,
}

/// `record`'s current image as shown at `/opt/<name>`, once its release
/// names the record's extension and version and its declaration asks for
/// what the record grants.
pub fn shown(record: &Record) -> Result<Shown, String> {
    let name = &record.name;
    let current = record.current.as_ref().ok_or("nothing is installed")?;
    let generation = self::generation(&open(name)?).ok_or("its activation names no generation")?;
    let read = |path| read(name, generation, path);
    let release = Release::parse(&read(RELEASE)?).map_err(|e| e.to_string())?;
    if release.name != *name || release.version != current.version {
        return Err(format!(
            "{} shows another image than the record's",
            mount_point(name)
        ));
    }
    let declaration = Declaration::parse(name, &read(DECLARE)?).map_err(|e| e.to_string())?;
    if !declaration.grant().same(&current.grant) {
        return Err("the image asks for other than it was granted".into());
    }
    Ok(Shown {
        generation,
        declaration,
        manifest: read(MANIFEST)?,
    })
}

/// The base's ceiling.
pub fn ceiling() -> Result<u64, String> {
    std::fs::read_to_string(slopos_extension_core::CEILING)
        .map_err(|e| e.to_string())
        .and_then(|text| ceiling::parse(&text).map_err(|e| e.to_string()))
        .map_err(|why| format!("{}: {why}", slopos_extension_core::CEILING))
}

/// An action of an extension's, as privd runs it.
pub struct Loaded {
    pub action: Action,
    pub extension: String,
    /// The activation it was read from, which its program must be a file of.
    pub generation: u64,
    /// The login tag its mechanism runs under.
    pub login: u32,
}

/// Every running extension's actions as privd runs them.
#[derive(Default)]
pub struct Actions {
    pub loaded: Vec<Loaded>,
    /// What they were read from, a `<name> <sha256> <generation>` line for
    /// each extension with actions: while it stands, so do they.
    pub from: String,
    /// The extensions left out, and why.
    pub skipped: Vec<String>,
}

pub fn actions() -> Result<Actions, String> {
    let store = Store::open(false).map_err(|why| format!("the extension store: {why}"))?;
    let names = store
        .names()
        .map_err(|why| format!("the extension store: {why}"))?;
    let mut out = Actions::default();
    if names.is_empty() {
        return Ok(out);
    }
    let ceiling = ceiling()?;
    let mut records = Vec::new();
    for name in names {
        match store.record(&name) {
            Ok(Some(record)) => records.push(record),
            Ok(None) => {}
            Err(why) => out.skipped.push(why),
        }
    }
    records.sort_by_key(Record::number);
    let mut taken: Vec<u32> = Vec::new();
    for record in records {
        let name = record.name.clone();
        let uids: Vec<u32> = record.uids.iter().map(|&(_, uid)| uid).collect();
        if uids.iter().any(|uid| taken.contains(uid)) {
            out.skipped
                .push(format!("{name}: it holds a uid another extension holds"));
            continue;
        }
        taken.extend(uids);
        let Some(current) = record.current.as_ref().filter(|_| record.runs()) else {
            continue;
        };
        let shown = match shown(&record) {
            Ok(shown) if shown.declaration.actions.is_empty() => continue,
            Ok(shown) => shown,
            Err(why) => {
                out.skipped.push(format!("{name}: {why}"));
                continue;
            }
        };
        let first = shown.declaration.users.first();
        let (Some(uid), Some(login), Some(label)) = (
            first.and_then(|user| record.uid(user)),
            record.number().and_then(crate::init_control::action_tag),
            Label::new(name.as_bytes(), current.sha256),
        ) else {
            out.skipped.push(format!(
                "{name}: its record gives it or its first user no uid"
            ));
            continue;
        };
        let held = current.grant.caps & ceiling & DELEGABLE & !NEVER;
        for action in &shown.declaration.actions {
            out.loaded.push(Loaded {
                action: action.for_privd(uid, held, label),
                extension: name.clone(),
                generation: shown.generation,
                login,
            });
        }
        out.from.push_str(&format!(
            "{name} {} {}\n",
            slopos_extension_core::hex(&current.sha256),
            shown.generation
        ));
    }
    Ok(out)
}
