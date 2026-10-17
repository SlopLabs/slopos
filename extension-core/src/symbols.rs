//! An extension's programs reach SlopOS through `libc.so` alone, as every
//! program outside the base does: each is dynamically linked, through the
//! base's interpreter, against `libc.so` and libraries in its own `/lib`,
//! and every name it binds is one `libc.so` or a library it needs exports.
//! `libc.so` only grows, so what binds against one binds against every later
//! one. Every ELF file outside `/lib` is a program, so a static PIE, which
//! names no interpreter, is refused wherever it could be run from.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::elf::{self, ElfError, Kind, Linking};
use crate::image::Image;
use crate::{INTERPRETER, LIBC_SONAME};

const LIB: &str = "/lib/";
/// More symlinks than a library's soname is ever reached through.
const LINK_HOPS: usize = 8;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SymbolError {
    Libc(ElfError),
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
}

impl fmt::Display for SymbolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SymbolError::Libc(e) => write!(f, "{}: {e:?}", crate::LIBC),
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
        }
    }
}

fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// [`check`] over `image`'s files and symlinks.
pub fn check_image(image: &Image<'_>, libc: &[u8]) -> Result<(), SymbolError> {
    let files: Vec<(&str, &[u8])> = image.files().collect();
    let links: Vec<(&str, &str)> = image.links().collect();
    check(&files, &links, libc)
}

/// Hold `files`, an image's `(path, bytes)`, to binding only what `libc`
/// and the image's own libraries export; `links` are its symlinks, `(path,
/// target)`, through which a library may be named.
pub fn check(
    files: &[(&str, &[u8])],
    links: &[(&str, &str)],
    libc: &[u8],
) -> Result<(), SymbolError> {
    let libc = elf::read(libc).map_err(SymbolError::Libc)?;
    let libc: BTreeSet<&[u8]> = libc.exports.iter().copied().collect();
    let mut read: Vec<(&str, Linking<'_>)> = Vec::new();
    for &(path, bytes) in files.iter().filter(|(_, bytes)| elf::is_elf(bytes)) {
        let linking = elf::read(bytes).map_err(|e| SymbolError::Elf(path.into(), e))?;
        if linking.kind != Kind::Other {
            read.push((path, linking));
        }
    }
    let library = |name: &[u8]| {
        let mut path = format!("{LIB}{}", core::str::from_utf8(name).ok()?);
        for _ in 0..LINK_HOPS {
            if let Some(found) = read
                .iter()
                .find(|(at, linking)| *at == path && linking.kind == Kind::Shared)
            {
                return Some(found);
            }
            let (at, target) = links.iter().find(|(at, _)| *at == path)?;
            let dir = at.rsplit_once('/').map_or("", |(dir, _)| dir);
            path = format!("{dir}/{target}");
        }
        None
    };
    for (path, linking) in &read {
        if !linking.dynamic {
            return Err(SymbolError::Static((*path).into()));
        }
        let program = !path.starts_with(LIB) || linking.kind == Kind::Executable;
        if program || linking.interpreter.is_some() {
            if linking.interpreter != Some(INTERPRETER.as_bytes()) {
                return Err(SymbolError::Interpreter((*path).into()));
            }
            if !linking.needed.contains(&LIBC_SONAME.as_bytes()) {
                return Err(SymbolError::NoLibc((*path).into()));
            }
        }
        let mut bound: BTreeSet<&[u8]> = BTreeSet::new();
        let mut seen: Vec<&[u8]> = Vec::new();
        let mut frontier: Vec<&[u8]> = linking.needed.clone();
        while let Some(name) = frontier.pop() {
            if seen.contains(&name) || name == LIBC_SONAME.as_bytes() {
                continue;
            }
            seen.push(name);
            let (_, needed) =
                library(name).ok_or_else(|| SymbolError::Needs((*path).into(), shown(name)))?;
            bound.extend(needed.exports.iter().copied());
            frontier.extend(needed.needed.iter().copied());
        }
        if let Some(name) = linking
            .imports
            .iter()
            .find(|name| !libc.contains(*name) && !bound.contains(*name))
        {
            return Err(SymbolError::Unbound((*path).into(), shown(name)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
