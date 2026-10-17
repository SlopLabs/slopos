//! An extension's image: a `newc` archive sealed as a base is, by a manifest
//! at [`MANIFEST`] of `<sha256> <mode> <path>` for every other record, its
//! records' paths those they have below `/opt/<name>`, under [`TOP`] alone,
//! and holding [`RELEASE`] and [`DECLARE`]. Every program the declaration
//! names is an executable file of it, and every symlink stays inside it.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFREG};
use slopos_boot_core::base::{self, CheckError, ManifestLine};
use slopos_boot_core::newc::{self, Entry, Record};

use crate::declare::{Declaration, mount_point};
use crate::release::Release;
use crate::sha256;
use crate::words::LineError;

pub const DIR: &str = "/.extension";
pub const MANIFEST: &str = "/.extension/manifest";
pub const RELEASE: &str = "/.extension/release";
pub const DECLARE: &str = "/.extension/declare";
/// Where an image's records go.
pub const TOP: [&str; 5] = ["/bin", "/libexec", "/lib", "/share", DIR];
const APPS: &str = "/share/slopos/apps";

#[derive(Clone, Debug)]
pub struct Image<'a> {
    pub bytes: &'a [u8],
    pub release: Release,
    pub declaration: Declaration,
    pub manifest: &'a [u8],
}

#[derive(Clone, Debug)]
pub enum ImageError<'a> {
    Sealed(CheckError<'a>),
    /// A record outside [`TOP`], or in [`DIR`] but none of its files.
    Placed(&'a [u8]),
    /// A symlink whose target is absolute or climbs through `..`.
    Escapes(&'a [u8]),
    Missing(&'static str),
    Release(LineError),
    Declare(LineError),
    /// A program, file or entry the declaration names that the image holds
    /// no such thing at.
    Undelivered(String),
}

impl fmt::Display for ImageError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Sealed(e) => write!(f, "{}", e.shown(MANIFEST)),
            ImageError::Placed(path) => write!(
                f,
                "{}: outside /bin, /libexec, /lib, /share and {DIR}'s own files",
                String::from_utf8_lossy(path)
            ),
            ImageError::Escapes(path) => write!(
                f,
                "{}: a symlink that may lead out of the image",
                String::from_utf8_lossy(path)
            ),
            ImageError::Missing(path) => write!(f, "no {path}"),
            ImageError::Release(e) => write!(f, "{RELEASE}: {e}"),
            ImageError::Declare(e) => write!(f, "{DECLARE}: {e}"),
            ImageError::Undelivered(what) => {
                write!(f, "{DECLARE} names {what}, which the image does not hold")
            }
        }
    }
}

fn beneath(path: &[u8], dir: &str) -> bool {
    path == dir.as_bytes()
        || path
            .strip_prefix(dir.as_bytes())
            .is_some_and(|rest| rest.first() == Some(&b'/'))
}

/// A symlink's target names a path of the image wherever it is shown:
/// relative, and never through `..`.
fn inside_image(target: &[u8]) -> bool {
    !target.is_empty() && target[0] != b'/' && target.split(|&b| b == b'/').all(|c| c != b"..")
}

fn placed<'a>(entry: &Entry<'a>) -> Result<(), CheckError<'a>> {
    let path = entry.path;
    if !TOP.iter().any(|top| beneath(path, top)) {
        return Err(CheckError::Kind(path));
    }
    if entry.kind() == S_IFLNK && !inside_image(entry.data) {
        return Err(CheckError::Target(path));
    }
    let own = [DIR, MANIFEST, RELEASE, DECLARE];
    if beneath(path, DIR) && !own.iter().any(|p| p.as_bytes() == path) {
        return Err(CheckError::Kind(path));
    }
    Ok(())
}

fn file<'a>(bytes: &'a [u8], path: &'static str) -> Result<&'a str, ImageError<'a>> {
    let entry = newc::find_last(bytes, path)
        .ok()
        .flatten()
        .filter(|entry| entry.kind() == S_IFREG)
        .ok_or(ImageError::Missing(path))?;
    core::str::from_utf8(entry.data).map_err(|_| ImageError::Missing(path))
}

impl<'a> Image<'a> {
    pub fn check(bytes: &'a [u8]) -> Result<Image<'a>, ImageError<'a>> {
        let misplaced = core::cell::Cell::new(None);
        let manifest = base::sealed(bytes, MANIFEST, sha256, |entry| {
            placed(entry).inspect_err(|e| misplaced.set(misplaced.get().or(Some(*e))))
        });
        let manifest = match (manifest, misplaced.get()) {
            (Err(_), Some(CheckError::Target(path))) => return Err(ImageError::Escapes(path)),
            (Err(_), Some(CheckError::Kind(path))) => return Err(ImageError::Placed(path)),
            (result, _) => result.map_err(ImageError::Sealed)?,
        };
        let release = Release::parse(file(bytes, RELEASE)?).map_err(ImageError::Release)?;
        let declaration = Declaration::parse(&release.name, file(bytes, DECLARE)?)
            .map_err(ImageError::Declare)?;
        let image = Image {
            bytes,
            release,
            declaration,
            manifest,
        };
        image.delivers()?;
        Ok(image)
    }

    /// The entry at `path`, a path below the mount point.
    pub fn entry(&self, path: &str) -> Option<Entry<'a>> {
        newc::find_last(self.bytes, path).ok().flatten()
    }

    /// Every regular file, by its path below the mount point.
    pub fn files(&self) -> impl Iterator<Item = (&'a str, &'a [u8])> + 'a {
        newc::entries(self.bytes)
            .flatten()
            .filter(|entry| entry.kind() == S_IFREG)
            .filter_map(|entry| Some((core::str::from_utf8(entry.path).ok()?, entry.data)))
    }

    /// Every symlink, by its path below the mount point, and its target.
    pub fn links(&self) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        newc::entries(self.bytes)
            .flatten()
            .filter(|entry| entry.kind() == S_IFLNK)
            .filter_map(|entry| {
                Some((
                    core::str::from_utf8(entry.path).ok()?,
                    core::str::from_utf8(entry.data).ok()?,
                ))
            })
    }

    /// What `/opt/<name>/<rest>` is in the image: `/<rest>`.
    fn inside<'p>(&self, path: &'p str) -> Option<&'p str> {
        path.strip_prefix(&mount_point(&self.release.name))
            .filter(|rest| rest.starts_with('/'))
    }

    fn delivers(&self) -> Result<(), ImageError<'a>> {
        let undelivered = |what: &str| ImageError::Undelivered(what.to_string());
        let declaration = &self.declaration;
        for program in declaration.programs(&self.release.name) {
            let executable = self
                .inside(&program)
                .and_then(|path| self.entry(path))
                .is_some_and(|e| e.kind() == S_IFREG && e.mode & 0o111 != 0);
            if !executable {
                return Err(undelivered(&program));
            }
        }
        for read in declaration.services.iter().flat_map(|s| &s.reads) {
            let Some(path) = self.inside(read) else {
                continue;
            };
            let found = match path.strip_suffix('/') {
                Some(dir) => {
                    self.entry(dir).is_some_and(|e| e.kind() == S_IFDIR)
                        || newc::entries(self.bytes)
                            .flatten()
                            .any(|e| e.path.starts_with(path.as_bytes()))
                }
                None => self.entry(path).is_some_and(|e| e.kind() != S_IFDIR),
            };
            if !found {
                return Err(undelivered(read));
            }
        }
        for app in &declaration.apps {
            let entry = alloc::format!("{APPS}/{app}.app");
            if !self.entry(&entry).is_some_and(|e| e.kind() == S_IFREG) {
                return Err(undelivered(&entry));
            }
        }
        Ok(())
    }
}

/// An image under construction: records added in the order they are to be
/// packed, then the release, the declaration and the manifest last.
#[derive(Default)]
pub struct Packer {
    records: Vec<(String, u32, Vec<u8>)>,
}

impl Packer {
    /// `path` below the mount point, a directory, file or symlink by `mode`;
    /// a symlink's data is its target.
    pub fn add(&mut self, path: &str, mode: u32, data: Vec<u8>) {
        self.records.push((path.to_string(), mode, data));
    }

    pub fn finish(mut self, release: &str, declare: &str) -> Result<Vec<u8>, String> {
        let data = S_IFREG | 0o644;
        self.records
            .retain(|(path, _, _)| ![RELEASE, DECLARE, MANIFEST].contains(&path.as_str()));
        self.records
            .push((RELEASE.into(), data, release.as_bytes().to_vec()));
        self.records
            .push((DECLARE.into(), data, declare.as_bytes().to_vec()));
        let mut manifest = String::new();
        for (path, mode, bytes) in &self.records {
            ManifestLine {
                digest: sha256(bytes),
                mode: *mode,
                path,
            }
            .write(&mut manifest)
            .map_err(|_| alloc::format!("{path}: no manifest line takes it"))?;
        }
        self.records
            .push((MANIFEST.into(), data, manifest.into_bytes()));
        let mut out = Vec::new();
        for (path, mode, bytes) in &self.records {
            let record = Record::new(path.as_bytes(), *mode, bytes)
                .ok_or_else(|| alloc::format!("{path}: too large for an archive"))?;
            record
                .pieces()
                .iter()
                .for_each(|piece| out.extend_from_slice(piece));
        }
        Record::trailer()
            .pieces()
            .iter()
            .for_each(|piece| out.extend_from_slice(piece));
        Image::check(&out).map_err(|e| e.to_string())?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
