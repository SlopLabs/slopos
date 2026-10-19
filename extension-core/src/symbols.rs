//! An extension's programs reach SlopOS through `libc.so` alone, as every
//! program outside the base does: each is dynamically linked, through the
//! base's interpreter, against `libc.so` and libraries in its own `/lib`,
//! and every name it binds is one `libc.so` or a library it needs exports.
//! `libc.so` only grows, so what binds against one binds against every later
//! one. Every ELF file outside `/lib` is a program, so a static PIE, which
//! names no interpreter, is refused wherever it could be run from.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use crate::elf::{self, ElfError, Kind};
use crate::image::{Image, Source};
use crate::{INTERPRETER, LIBC_SONAME};

const LIB: &str = "/lib/";
/// More symlinks than a library's soname is ever reached through.
const LINK_HOPS: usize = 8;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SymbolError {
    Libc(ElfError),
    /// The image's files could not be read whole.
    Read(String),
    Elf(String, ElfError),
    /// Linked statically: it carries its own C library.
    Static(String),
    /// A program the base's interpreter does not load.
    Interpreter(String),
    /// A program that does not name `libc.so`.
    NoLibc(String),
    /// A library neither `libc.so` nor in the image's `/lib`.
    Needs(String, String),
    /// A name nothing it needs exports.
    Unbound(String, String),
    /// More names than its bytes hold, or more libraries than a program
    /// needs.
    TooMany(String),
}

impl fmt::Display for SymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolError::Libc(e) => write!(f, "{}: {e:?}", crate::LIBC),
            SymbolError::Read(why) => write!(f, "{why}"),
            SymbolError::Elf(path, e) => write!(f, "{path}: an ELF file this cannot read ({e:?})"),
            SymbolError::Static(path) => {
                write!(f, "{path}: statically linked, not against {LIBC_SONAME}")
            }
            SymbolError::Interpreter(path) => {
                write!(f, "{path}: a program {INTERPRETER} does not load")
            }
            SymbolError::NoLibc(path) => write!(f, "{path}: does not need {LIBC_SONAME}"),
            SymbolError::Needs(path, lib) => {
                write!(
                    f,
                    "{path}: needs {lib}, which is neither {LIBC_SONAME} nor in its /lib"
                )
            }
            SymbolError::Unbound(path, name) => {
                write!(f, "{path}: binds {name}, which nothing it needs exports")
            }
            SymbolError::TooMany(path) => write!(
                f,
                "{path}: names more than it holds, or needs more than {NEEDED_MAX} libraries or a closure of {CLOSURE_MAX}"
            ),
        }
    }
}

fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The largest ELF file of an image this reads whole.
const ELF_MAX: u64 = 1 << 30;

/// Libraries one file may name, and one program's closure reach, at most:
/// more than any system links, few enough that checking an image stays
/// proportional to it.
const NEEDED_MAX: usize = 64;
const CLOSURE_MAX: usize = 256;

/// What a file binds and offers, its own copy of each name once, so a
/// file's bytes go once it is read.
struct Linked {
    kind: Kind,
    dynamic: bool,
    interpreter: Option<Vec<u8>>,
    needed: BTreeSet<Vec<u8>>,
    imports: BTreeSet<Vec<u8>>,
    exports: BTreeSet<Vec<u8>>,
}

impl Linked {
    /// What `bytes` binds and offers; refused when its names, each once,
    /// come to more than the file holds.
    fn read(path: &str, bytes: &[u8]) -> Result<Self, SymbolError> {
        let linking = elf::read(bytes).map_err(|e| SymbolError::Elf(path.into(), e))?;
        let mut room = bytes.len();
        let mut owned = |names: &[&[u8]]| {
            let mut set = BTreeSet::new();
            for &name in names {
                if !set.contains(name) {
                    room = room
                        .checked_sub(name.len())
                        .ok_or_else(|| SymbolError::TooMany(path.into()))?;
                    set.insert(name.to_vec());
                }
            }
            Ok::<_, SymbolError>(set)
        };
        let linked = Self {
            kind: linking.kind,
            dynamic: linking.dynamic,
            interpreter: linking.interpreter.map(<[u8]>::to_vec),
            needed: owned(&linking.needed)?,
            imports: owned(&linking.imports)?,
            exports: owned(&linking.exports)?,
        };
        if linked.needed.len() > NEEDED_MAX {
            return Err(SymbolError::TooMany(path.into()));
        }
        Ok(linked)
    }
}

/// [`check`] over `image`'s files and symlinks, each file read alone.
pub fn check_image<S: Source>(image: &mut Image<S>, libc: &[u8]) -> Result<(), SymbolError> {
    let mut read = Vec::new();
    image
        .each_elf(ELF_MAX, &mut |path, bytes| {
            let linked = Linked::read(path, bytes)?;
            if linked.kind != Kind::Other {
                read.push((path.to_string(), linked));
            }
            Ok(())
        })
        .map_err(|e| match e {
            Ok(e) => SymbolError::Read(e.to_string()),
            Err(e) => e,
        })?;
    let links: Vec<(&str, &str)> = image.links().collect();
    bind(&read, &links, libc)
}

/// Hold `files`, an image's `(path, bytes)`, to binding only what `libc`
/// and the image's own libraries export; `links` are its symlinks, `(path,
/// target)`, through which a library may be named.
pub fn check(
    files: &[(&str, &[u8])],
    links: &[(&str, &str)],
    libc: &[u8],
) -> Result<(), SymbolError> {
    let mut read = Vec::new();
    for &(path, bytes) in files.iter().filter(|(_, bytes)| elf::is_elf(bytes)) {
        let linked = Linked::read(path, bytes)?;
        if linked.kind != Kind::Other {
            read.push((path.to_string(), linked));
        }
    }
    bind(&read, links, libc)
}

fn bind(read: &[(String, Linked)], links: &[(&str, &str)], libc: &[u8]) -> Result<(), SymbolError> {
    let libc = elf::read(libc).map_err(SymbolError::Libc)?;
    let libc: BTreeSet<&[u8]> = libc.exports.iter().copied().collect();
    let shared: BTreeMap<&str, usize> = read
        .iter()
        .enumerate()
        .filter(|(_, (_, linked))| linked.kind == Kind::Shared)
        .map(|(at, (path, _))| (path.as_str(), at))
        .collect();
    let links: BTreeMap<&str, &str> = links.iter().copied().collect();
    let mut found: BTreeMap<&[u8], Option<usize>> = BTreeMap::new();
    for (path, linked) in read {
        if !linked.dynamic {
            return Err(SymbolError::Static(path.clone()));
        }
        let program = !path.starts_with(LIB) || linked.kind == Kind::Executable;
        if program || linked.interpreter.is_some() {
            if linked.interpreter.as_deref() != Some(INTERPRETER.as_bytes()) {
                return Err(SymbolError::Interpreter(path.clone()));
            }
            if !linked.needed.contains(LIBC_SONAME.as_bytes()) {
                return Err(SymbolError::NoLibc(path.clone()));
            }
        }
        let mut reached: BTreeSet<usize> = BTreeSet::new();
        let mut frontier: Vec<&[u8]> = linked.needed.iter().map(Vec::as_slice).collect();
        while let Some(name) = frontier.pop() {
            if name == LIBC_SONAME.as_bytes() {
                continue;
            }
            let library = *found
                .entry(name)
                .or_insert_with(|| library(name, &shared, &links));
            let library = library.ok_or_else(|| SymbolError::Needs(path.clone(), shown(name)))?;
            if !reached.insert(library) {
                continue;
            }
            if reached.len() > CLOSURE_MAX {
                return Err(SymbolError::TooMany(path.clone()));
            }
            frontier.extend(read[library].1.needed.iter().map(Vec::as_slice));
        }
        let bound = |name: &[u8]| {
            libc.contains(name) || reached.iter().any(|&at| read[at].1.exports.contains(name))
        };
        if let Some(name) = linked.imports.iter().find(|name| !bound(name)) {
            return Err(SymbolError::Unbound(path.clone(), shown(name)));
        }
    }
    Ok(())
}

/// The shared library of the image `name` names in `/lib`, through its
/// symlinks.
fn library(
    name: &[u8],
    shared: &BTreeMap<&str, usize>,
    links: &BTreeMap<&str, &str>,
) -> Option<usize> {
    let mut path = format!("{LIB}{}", core::str::from_utf8(name).ok()?);
    for _ in 0..LINK_HOPS {
        if let Some(&at) = shared.get(path.as_str()) {
            return Some(at);
        }
        let target = links.get(path.as_str())?;
        let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        path = format!("{dir}/{target}");
    }
    None
}

#[cfg(test)]
mod tests;
