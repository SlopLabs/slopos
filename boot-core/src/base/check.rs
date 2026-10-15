//! A base held to what `tools/initramfs` packs and the kernel indexes,
//! mounts and unpacks. The kernel frees the base it does not choose before it
//! indexes the one it does, so neither the packer nor `bootctl` takes any
//! other.

use core::fmt;

use slopos_abi::fs::{BASE_DIRS, S_IFDIR, S_IFLNK, S_IFREG, USER_NAME_MAX, USER_PATH_MAX};

use super::{BUILD_TAG_PATH, Error, MANIFEST_PATH, interface, manifest_lines};
use crate::newc::{self, Entry};

/// The most records a base holds: each is compared with every other.
pub const MAX_RECORDS: usize = 2048;

/// What the kernel mounts before it unpacks a RAM root: a base may name these
/// as directories, and holds nothing in devfs.
const MOUNTED_DIRS: [&str; 3] = ["/tmp", "/run", "/dev"];
const DEVFS: &str = "/dev";

/// A SHA-256 of a record's bytes, which the caller computes.
pub type Digest = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checked<'a> {
    pub interface: u32,
    pub manifest: &'a [u8],
    pub build_tag: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError<'a> {
    Archive(newc::Error),
    TooMany,
    /// A device, FIFO or socket, which the base does not serve.
    Kind(&'a [u8]),
    /// A path that is not absolute and in its one spelling.
    Name(&'a [u8]),
    /// A symlink to nothing, or to a path too long to be one.
    Target(&'a [u8]),
    Twice(&'a [u8]),
    BeneathFile(&'a [u8]),
    /// No directory where the kernel mounts the base, or its own
    /// filesystems, over one.
    NotDirectory(&'a [u8]),
    /// A record in the devfs the kernel mounts over `/dev`.
    InDevfs(&'a [u8]),
    Interface(Error),
    NoManifest,
    /// The manifest's line, from 1, that does not parse.
    ManifestLine(usize),
    /// A line naming other than the record the archive holds there.
    Unknown(&'a str),
    /// A record the manifest leaves out.
    Unlisted(&'a [u8]),
    /// A record not of the mode or the digest its line gives.
    Misstated(&'a str),
}

struct Shown<'a>(&'a [u8]);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match core::str::from_utf8(self.0) {
            Ok(text) => f.write_str(text),
            Err(_) => write!(f, "{:?}", self.0),
        }
    }
}

impl fmt::Display for CheckError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CheckError::Archive(e) => write!(f, "not a newc archive ({e:?})"),
            CheckError::TooMany => write!(f, "more than {MAX_RECORDS} records"),
            CheckError::Kind(path) => write!(f, "{}: no file, directory or symlink", Shown(path)),
            CheckError::Name(path) => write!(f, "{}: a name the kernel refuses", Shown(path)),
            CheckError::Target(path) => {
                write!(f, "{}: a symlink the kernel refuses", Shown(path))
            }
            CheckError::Twice(path) => write!(f, "{}: packed twice", Shown(path)),
            CheckError::BeneathFile(path) => {
                write!(f, "{}: beneath what is no directory", Shown(path))
            }
            CheckError::NotDirectory(path) => {
                write!(f, "{}: the kernel mounts a directory there", Shown(path))
            }
            CheckError::InDevfs(path) => write!(f, "{}: in the kernel's devfs", Shown(path)),
            CheckError::Interface(e) => write!(f, "{e}"),
            CheckError::NoManifest => write!(f, "no {MANIFEST_PATH}"),
            CheckError::ManifestLine(line) => {
                write!(f, "{MANIFEST_PATH}: line {line} is malformed")
            }
            CheckError::Unknown(path) => {
                write!(
                    f,
                    "{MANIFEST_PATH} names {path} where the archive holds another"
                )
            }
            CheckError::Unlisted(path) => {
                write!(f, "{MANIFEST_PATH} leaves out {}", Shown(path))
            }
            CheckError::Misstated(path) => write!(f, "{path}: not what {MANIFEST_PATH} says"),
        }
    }
}

/// Absolute, every component a name the kernel takes, and shorter than a
/// path: one spelling, so the index, the unpack and this read one path.
fn canonical(path: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(b"/") else {
        return false;
    };
    path.len() < USER_PATH_MAX
        && core::str::from_utf8(path).is_ok()
        && rest.split(|&b| b == b'/').all(|part| {
            !part.is_empty() && part != b"." && part != b".." && part.len() <= USER_NAME_MAX
        })
}

/// A symlink's target as the kernel reads it, up to a NUL some writers add.
fn target_ok(data: &[u8]) -> bool {
    let target = data.split(|&b| b == 0).next().unwrap_or_default();
    !target.is_empty() && target.len() < USER_PATH_MAX
}

/// Whether `inner` lies below `outer`, both canonical.
fn beneath(inner: &[u8], outer: &[u8]) -> bool {
    inner.len() > outer.len() && inner.starts_with(outer) && inner[outer.len()] == b'/'
}

/// Where the kernel needs a directory: each base directory and each on the
/// way to one, and where it mounts its own filesystems.
fn wants_directory(path: &[u8]) -> bool {
    BASE_DIRS
        .iter()
        .chain(&MOUNTED_DIRS)
        .any(|dir| dir.as_bytes() == path || beneath(dir.as_bytes(), path))
}

fn is_manifest(entry: &Entry<'_>) -> bool {
    entry.path == MANIFEST_PATH.as_bytes()
}

/// The base `image` is, or the first thing that makes it none; `digest` is
/// SHA-256.
pub fn check<'a>(
    image: &'a [u8],
    digest: impl Fn(&[u8]) -> Digest,
) -> Result<Checked<'a>, CheckError<'a>> {
    let mut records = 0;
    let mut manifest = None;
    for entry in newc::entries(image) {
        let entry = entry.map_err(CheckError::Archive)?;
        records += 1;
        if records > MAX_RECORDS {
            return Err(CheckError::TooMany);
        }
        let path = entry.path;
        if ![S_IFDIR, S_IFREG, S_IFLNK].contains(&entry.kind()) {
            return Err(CheckError::Kind(path));
        }
        if !canonical(path) {
            return Err(CheckError::Name(path));
        }
        if entry.kind() == S_IFLNK && !target_ok(entry.data) {
            return Err(CheckError::Target(path));
        }
        if entry.kind() != S_IFDIR && wants_directory(path) {
            return Err(CheckError::NotDirectory(path));
        }
        if beneath(path, DEVFS.as_bytes()) {
            return Err(CheckError::InDevfs(path));
        }
        for earlier in newc::entries(image).take(records - 1).flatten() {
            if earlier.path == path {
                return Err(CheckError::Twice(path));
            }
            let below_file =
                |a: &Entry<'_>, b: &Entry<'_>| b.kind() != S_IFDIR && beneath(a.path, b.path);
            if below_file(&entry, &earlier) || below_file(&earlier, &entry) {
                return Err(CheckError::BeneathFile(path));
            }
        }
        if is_manifest(&entry) && entry.kind() == S_IFREG {
            manifest = Some(entry.data);
        }
    }
    let interface = interface(image).map_err(CheckError::Interface)?;
    let manifest = manifest.ok_or(CheckError::NoManifest)?;
    let text = core::str::from_utf8(manifest).map_err(|_| CheckError::ManifestLine(1))?;
    let mut lines = manifest_lines(text);
    for entry in newc::entries(image).flatten().filter(|e| !is_manifest(e)) {
        let line = lines
            .next()
            .ok_or(CheckError::Unlisted(entry.path))?
            .map_err(|bad| CheckError::ManifestLine(bad.line))?;
        if line.path.as_bytes() != entry.path {
            return Err(CheckError::Unknown(line.path));
        }
        if line.mode != entry.mode || line.digest != digest(entry.data) {
            return Err(CheckError::Misstated(line.path));
        }
    }
    if let Some(line) = lines.next() {
        let line = line.map_err(|bad| CheckError::ManifestLine(bad.line))?;
        return Err(CheckError::Unknown(line.path));
    }
    Ok(Checked {
        interface,
        manifest,
        build_tag: build_tag(image),
    })
}

fn build_tag(image: &[u8]) -> Option<&str> {
    newc::find_last(image, BUILD_TAG_PATH)
        .ok()
        .flatten()
        .filter(|entry| entry.kind() == S_IFREG)
        .and_then(|entry| core::str::from_utf8(entry.data).ok())
        .map(str::trim)
}

/// What an image on the boot partition says of itself, read as the kernel
/// reads it and not checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Described<'a> {
    pub interface: Result<u32, Error>,
    pub manifest: Option<&'a [u8]>,
    pub build_tag: Option<&'a str>,
}

pub fn describe(image: &[u8]) -> Described<'_> {
    Described {
        interface: interface(image),
        manifest: newc::find_last(image, MANIFEST_PATH)
            .ok()
            .flatten()
            .filter(|entry| entry.kind() == S_IFREG)
            .map(|entry| entry.data),
        build_tag: build_tag(image),
    }
}

#[cfg(test)]
mod tests;
