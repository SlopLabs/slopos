//! A tree installed onto a root — the toolchain at `/usr/local` — and the rule
//! that replaces it with a newer one, which `scripts/fs_tree.py` follows on
//! the host and the installer in the guest.
//!
//! What an install put down is recorded in a manifest under
//! [`MANIFEST_DIR`]: an `identity` line, then `<kind> <size or -> <path>` per
//! entry, parents first. A reinstall removes exactly what the old manifest
//! names and keeps whatever the root's user put beside it; anything in the way
//! of the new tree that the old manifest does not name, or that is not the
//! kind it names, refuses the install before anything is written. While an
//! install is under way the manifest names the old entries and the new under
//! [`UNFINISHED`], so one cut short is redone, never trusted.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Where a root keeps the manifest of each tree installed onto it.
pub const MANIFEST_DIR: &str = "/var/lib/slopos/trees";
/// The identity a manifest carries while its install is under way.
pub const UNFINISHED: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The manifest name of the tree installed at `guest`: its path with `/`
/// written `_`, `usr_local` for `/usr/local`.
pub fn manifest_name(guest: &str) -> String {
    guest.trim_start_matches('/').replace('/', "_")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Dir,
    File,
    Link,
}

impl Kind {
    fn letter(self) -> char {
        match self {
            Kind::Dir => 'd',
            Kind::File => 'f',
            Kind::Link => 'l',
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Kind::Dir => "directory",
            Kind::File => "file",
            Kind::Link => "symlink",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub kind: Kind,
    /// A file's size; `None` for the rest, and for every old entry an
    /// unfinished manifest names.
    pub size: Option<u64>,
    /// Below the tree's root, `/`-separated, never empty, `.` or `..`.
    pub rel: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub identity: String,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
}

fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && rel
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, ParseError> {
        let mut lines = text.lines().enumerate();
        let identity = lines
            .next()
            .and_then(|(_, line)| line.strip_prefix("identity "))
            .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or(ParseError { line: 1 })?
            .to_string();
        let mut items = Vec::new();
        for (at, line) in lines {
            let bad = ParseError { line: at + 1 };
            let mut fields = line.splitn(3, ' ');
            let kind = match fields.next() {
                Some("d") => Kind::Dir,
                Some("f") => Kind::File,
                Some("l") => Kind::Link,
                _ => return Err(bad),
            };
            let size = match fields.next() {
                Some("-") => None,
                Some(n) if kind == Kind::File => Some(n.parse().map_err(|_| bad.clone())?),
                _ => return Err(bad),
            };
            let rel = fields.next().filter(|r| safe_rel(r)).ok_or(bad)?;
            items.push(Item {
                kind,
                size,
                rel: rel.to_string(),
            });
        }
        Ok(Manifest { identity, items })
    }

    pub fn render(&self) -> String {
        let mut out = format!("identity {}\n", self.identity);
        for item in &self.items {
            let size = item.size.map_or_else(|| "-".to_string(), |n| n.to_string());
            out.push_str(&format!("{} {size} {}\n", item.kind.letter(), item.rel));
        }
        out
    }

    pub fn is_finished(&self) -> bool {
        self.identity != UNFINISHED
    }
}

/// What a path on the root is, as `lstat` says, without following a symlink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Absent,
    Dir,
    File,
    Link,
    /// A device, a FIFO or a socket.
    Other,
}

impl State {
    fn noun(self) -> &'static str {
        match self {
            State::Absent => "nothing",
            State::Dir => "directory",
            State::File => "file",
            State::Link => "symlink",
            State::Other => "special file",
        }
    }

    fn is(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (State::Dir, Kind::Dir) | (State::File, Kind::File) | (State::Link, Kind::Link)
        )
    }
}

/// The root a tree is installed onto, read through absolute paths.
pub trait Root {
    fn state(&mut self, path: &str) -> State;
    /// The names a directory holds; none when it cannot be listed.
    fn children(&mut self, dir: &str) -> Vec<String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Remove(String),
    RemoveDir(String),
    /// A directory above the tree's root.
    MakeDir(String),
    /// Put the new tree's `item` at `path`: a directory may already be there
    /// and takes the item's mode, anything else is not.
    Install {
        item: Item,
        path: String,
    },
}

/// An install the root can take: the steps in order, the manifest to record
/// before the first and the one to record after the last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    pub during: Manifest,
    pub done: Manifest,
}

/// What on the root is in the way of the new tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflicts(pub Vec<String>);

fn join(dir: &str, rel: &str) -> String {
    if dir == "/" {
        format!("/{rel}")
    } else {
        format!("{dir}/{rel}")
    }
}

fn ancestors(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(next) = path[at + 1..].find('/') {
        at += next + 1;
        out.push(path[..at].to_string());
    }
    out
}

/// The first directory between `guest` and `path` that is not one, which a
/// path walk would follow or fail at: a manifest the root's user can edit
/// must not reach past the tree through a symlink.
fn blocked_above(
    root: &mut impl Root,
    guest: &str,
    path: &str,
    removed: &BTreeSet<String>,
) -> Option<(String, State)> {
    ancestors(path)
        .into_iter()
        .filter(|dir| dir.len() > guest.len())
        .find_map(|dir| {
            let state = if removed.contains(&dir) {
                State::Absent
            } else {
                root.state(&dir)
            };
            (!matches!(state, State::Dir | State::Absent)).then_some((dir, state))
        })
}

/// The steps that install the tree `new` describes at `guest` on `root`, where
/// `old` is the manifest an earlier install there recorded, if any.
pub fn plan(
    root: &mut impl Root,
    guest: &str,
    old: Option<&Manifest>,
    new: &Manifest,
) -> Result<Plan, Conflicts> {
    let mut conflicts = Vec::new();
    let mut steps = Vec::new();
    for dir in ancestors(guest).into_iter().chain([guest.to_string()]) {
        match root.state(&dir) {
            State::Absent => steps.push(Step::MakeDir(dir)),
            State::Dir => {}
            state => conflicts.push(format!(
                "{dir} is a {}, where {guest} needs a directory",
                state.noun()
            )),
        }
    }

    let old_items = old.map_or(&[][..], |m| &m.items[..]);
    // An unfinished install lists a path whose kind it was changing under
    // both kinds, and left it as either.
    let recorded =
        |rel: &str, state: State| old_items.iter().any(|i| i.rel == rel && state.is(i.kind));
    let mut removed = BTreeSet::new();
    for item in old_items {
        let path = join(guest, &item.rel);
        if removed.contains(&path) {
            continue;
        }
        if let Some((dir, state)) = blocked_above(root, guest, &path, &removed) {
            conflicts.push(format!(
                "{dir} is a {}; the last install put a directory there",
                state.noun()
            ));
            continue;
        }
        match root.state(&path) {
            State::Absent => {}
            state if !state.is(item.kind) && recorded(&item.rel, state) => {}
            state if !state.is(item.kind) => conflicts.push(format!(
                "{path} is a {}; the last install put a {} there",
                state.noun(),
                item.kind.noun()
            )),
            _ if item.kind != Kind::Dir => {
                removed.insert(path.clone());
                steps.push(Step::Remove(path));
            }
            _ => {}
        }
    }
    let kept: BTreeSet<String> = new
        .items
        .iter()
        .filter(|item| item.kind == Kind::Dir)
        .map(|item| join(guest, &item.rel))
        .collect();
    let mut old_dirs: Vec<&Item> = old_items
        .iter()
        .filter(|item| item.kind == Kind::Dir)
        .collect();
    old_dirs.sort_by_key(|item| core::cmp::Reverse(item.rel.matches('/').count()));
    for item in old_dirs {
        let path = join(guest, &item.rel);
        if kept.contains(&path) || root.state(&path) != State::Dir {
            continue;
        }
        let children = root.children(&path);
        if children.iter().all(|c| removed.contains(&join(&path, c))) {
            removed.insert(path.clone());
            steps.push(Step::RemoveDir(path));
        }
    }

    for item in &new.items {
        let path = join(guest, &item.rel);
        if let Some((dir, state)) = blocked_above(root, guest, &path, &removed) {
            conflicts.push(format!(
                "{dir} is a {}, where the new tree has a directory",
                state.noun()
            ));
            continue;
        }
        let state = if removed.contains(&path) {
            State::Absent
        } else {
            root.state(&path)
        };
        match (item.kind, state) {
            (Kind::Dir, State::Dir) | (_, State::Absent) => steps.push(Step::Install {
                item: item.clone(),
                path,
            }),
            (Kind::Dir, state) => conflicts.push(format!(
                "{path} is a {}, where the new tree has a directory",
                state.noun()
            )),
            (_, _) => conflicts.push(format!("{path} is already there, and the new tree has one")),
        }
    }
    if !conflicts.is_empty() {
        conflicts.dedup();
        return Err(Conflicts(conflicts));
    }

    let listed: BTreeSet<(Kind, &str)> = new
        .items
        .iter()
        .map(|item| (item.kind, item.rel.as_str()))
        .collect();
    let leftover = old_items
        .iter()
        .filter(|item| !listed.contains(&(item.kind, item.rel.as_str())))
        .map(|item| Item {
            size: None,
            ..item.clone()
        });
    Ok(Plan {
        steps,
        during: Manifest {
            identity: UNFINISHED.to_string(),
            items: new.items.iter().cloned().chain(leftover).collect(),
        },
        done: new.clone(),
    })
}

#[cfg(test)]
mod tests;
