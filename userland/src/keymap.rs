//! Keyboard-layout helpers: the file → parse → serialise → upload pipeline,
//! and which layout is wanted. The computer's layout, in [`PERSIST_PATH`],
//! serves the login screen, the console and everyone who chose none; a
//! person's own, in their preferences, serves while their desktop is in
//! front. Init loads whichever is wanted as that changes.
//!
//! Layout files are parsed in userland into a binary `LayoutTable` and uploaded
//! via `SYSCALL_KEYMAP_LOAD`; the kernel never parses layout text.

use std::ffi::CString;
use std::fs;

use slopos_keymap_core::{LayoutTable, SERIALIZED_LEN, parse, serialize};
use slopos_settings_core::prefs::layout_name;

use crate::syscall::keymap::{keymap_get_name, keymap_load};

/// Where the installed `*.layout` files live.
pub const KEYMAP_DIR: &str = "/usr/share/keymaps";

/// The computer's layout, by its short name.
pub const PERSIST_PATH: &str = "/etc/keymap";

/// The kernel's own layout, which no file is needed for.
pub const BUILT_IN: &str = "us";

pub fn current_name() -> Option<String> {
    let mut buf = [0u8; 16];
    let n = keymap_get_name(&mut buf);
    if n < 0 {
        return None;
    }
    let n = (n as usize).min(buf.len());
    core::str::from_utf8(&buf[..n]).ok().map(String::from)
}

pub fn layout_path(name: &str) -> Option<String> {
    layout_name(name).then(|| format!("{KEYMAP_DIR}/{name}.layout"))
}

pub fn exists(name: &str) -> bool {
    layout_path(name).is_some_and(|path| std::path::Path::new(&path).is_file())
}

/// An installed layout, and what a person is shown for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub title: String,
}

/// Every installed layout, by title.
pub fn layouts() -> Vec<Layout> {
    let Ok(entries) = fs::read_dir(KEYMAP_DIR) else {
        return Vec::new();
    };
    let mut found: Vec<Layout> = entries
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name();
            let name = file.to_str()?.strip_suffix(".layout")?.to_string();
            let src = fs::read(entry.path()).ok()?;
            let title = slopos_keymap_core::title(&src).unwrap_or(&name).to_string();
            layout_name(&name).then_some(Layout { name, title })
        })
        .collect();
    found.sort_by(|a, b| a.title.cmp(&b.title));
    found
}

/// Load `/usr/share/keymaps/<name>.layout`, parse and validate it, and upload
/// the binary table to the kernel. The `Err` names the failing stage.
pub fn load_layout_by_name(name: &str) -> Result<(), &'static str> {
    let path = layout_path(name).ok_or("no such layout")?;
    let src = fs::read(&path).map_err(|_| "no such layout")?;

    let mut table = Box::new(LayoutTable::empty());
    parse(&src, &mut table).map_err(|_| "failed to parse layout")?;
    let mut blob = vec![0u8; SERIALIZED_LEN];
    serialize(&table, &mut blob).map_err(|_| "failed to serialise layout")?;

    if keymap_load(&blob) < 0 {
        return Err("kernel rejected layout");
    }
    Ok(())
}

/// The computer's layout, when uid 0 keeps one that is installed.
pub fn computer_layout() -> Option<String> {
    let bytes = crate::regular_file::read(PERSIST_PATH, 64, Some(0))?;
    let name = std::str::from_utf8(&bytes).ok()?.trim();
    exists(name).then(|| name.to_string())
}

/// Keep `name` as the computer's layout, durably, since the next boot is
/// what it is for.
pub fn keep_computer_layout(name: &str) -> Result<(), String> {
    let path = CString::new(PERSIST_PATH).map_err(|e| e.to_string())?;
    crate::syscall::fs::write_durable(&path, name.as_bytes())
        .map_err(|e| format!("{PERSIST_PATH}: {e}"))
}

/// Why a person's own layout did not take.
#[derive(Debug)]
pub enum Own {
    NoSuchLayout,
    NoHome,
    /// Their preferences could not be written, for this reason.
    NotKept(String),
    /// It is kept, but init did not load it, for this reason.
    NotLoaded(String),
}

/// Keep `name`, or the computer's for `None`, as this process's person's
/// layout, and have init load what is wanted now.
pub fn keep_own(name: Option<&str>) -> Result<(), Own> {
    if name.is_some_and(|name| !exists(name)) {
        return Err(Own::NoSuchLayout);
    }
    let path = crate::prefs::own_path().ok_or(Own::NoHome)?;
    let mut prefs = crate::prefs::read(&path);
    match name {
        Some(name) => prefs
            .set(slopos_settings_core::prefs::Key::KeyboardLayout, name)
            .map_err(|e| Own::NotKept(e.to_string()))?,
        None => prefs.reset(slopos_settings_core::prefs::Key::KeyboardLayout),
    }
    crate::prefs::write(&path, &prefs).map_err(Own::NotKept)?;
    crate::init_control::ask(crate::init_control::Command::Keymap).map_err(Own::NotLoaded)
}

/// The layout wanted while `person` — their home and uid — is in front, or
/// the computer's while nobody is.
pub fn wanted(person: Option<(&str, u32)>) -> String {
    let own = person.and_then(|(home, uid)| {
        let prefs = crate::prefs::of_person(home, uid);
        prefs
            .keyboard_layout()
            .filter(|name| exists(name))
            .map(str::to_string)
    });
    own.or_else(computer_layout)
        .unwrap_or_else(|| BUILT_IN.to_string())
}

/// Load the layout `person` wants, unless it is the one in use.
pub fn follow(person: Option<(&str, u32)>) -> Result<(), &'static str> {
    let wanted = wanted(person);
    if current_name().as_deref() == Some(wanted.as_str()) {
        return Ok(());
    }
    load_layout_by_name(&wanted)
}
