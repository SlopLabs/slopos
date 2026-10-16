//! A base image: the `newc` archive every slot boots, naming its interface at
//! [`INTERFACE_PATH`] and its records at [`MANIFEST_PATH`]. A kernel boots
//! the first of current and previous whose interface it serves.

use core::fmt::{self, Write};
use core::ops::RangeInclusive;

use slopos_abi::fs::{S_IFREG, USER_PATH_MAX};

use crate::newc;

mod check;
pub use check::{CheckError, Checked, Described, Digest, MAX_RECORDS, check, describe};

pub const INTERFACE_PATH: &str = "/usr/share/slopos/base-interface";
/// One [`ManifestLine`] per record but its own, in the order the archive
/// holds them, so two bases of one manifest hold the same tree.
pub const MANIFEST_PATH: &str = "/usr/share/slopos/base-manifest";
pub const BUILD_TAG_PATH: &str = "/usr/share/slopos/build-tag";

const INTERFACE_MAX: usize = "4294967295\n".len();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Archive(newc::Error),
    NoInterface,
    /// Not a regular file holding a number from 1, alone on a line.
    BadInterface,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Archive(e) => write!(f, "not a newc archive ({e:?})"),
            Error::NoInterface => write!(f, "no {INTERFACE_PATH}"),
            Error::BadInterface => write!(f, "{INTERFACE_PATH} names no interface"),
        }
    }
}

pub fn parse_interface(text: &[u8]) -> Result<u32, Error> {
    let digits = text.strip_suffix(b"\n").unwrap_or(text);
    if text.len() > INTERFACE_MAX
        || digits.first().is_none_or(|&b| b == b'0')
        || !digits.iter().all(u8::is_ascii_digit)
    {
        return Err(Error::BadInterface);
    }
    digits
        .iter()
        .try_fold(0u32, |n, &b| {
            n.checked_mul(10)?.checked_add(u32::from(b - b'0'))
        })
        .ok_or(Error::BadInterface)
}

/// The interface `archive` names, in its last record at [`INTERFACE_PATH`].
pub fn interface(archive: &[u8]) -> Result<u32, Error> {
    let entry = newc::find_last(archive, INTERFACE_PATH)
        .map_err(Error::Archive)?
        .ok_or(Error::NoInterface)?;
    if entry.kind() != S_IFREG {
        return Err(Error::BadInterface);
    }
    parse_interface(entry.data)
}

/// What one of an entry's two bases reads as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    Absent,
    Unreadable(Error),
    Interface(u32),
}

impl Reading {
    pub fn of(archive: Option<&[u8]>) -> Reading {
        match archive.map(interface) {
            None => Reading::Absent,
            Some(Ok(n)) => Reading::Interface(n),
            Some(Err(e)) => Reading::Unreadable(e),
        }
    }

    fn served_by(self, served: &RangeInclusive<u32>) -> bool {
        matches!(self, Reading::Interface(n) if served.contains(&n))
    }
}

impl fmt::Display for Reading {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reading::Absent => write!(f, "absent"),
            Reading::Unreadable(e) => write!(f, "{e}"),
            Reading::Interface(n) => write!(f, "interface {n}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Current,
    Previous,
}

impl Which {
    pub fn name(self) -> &'static str {
        match self {
            Which::Current => "current",
            Which::Previous => "previous",
        }
    }
}

/// The base a kernel serving `served` boots: current, then previous; `None`
/// when it serves neither.
pub fn choose(current: Reading, previous: Reading, served: RangeInclusive<u32>) -> Option<Which> {
    if current.served_by(&served) {
        Some(Which::Current)
    } else if previous.served_by(&served) {
        Some(Which::Previous)
    } else {
        None
    }
}

/// A record of the base: the SHA-256 of its bytes (a symlink's target, a
/// directory's none), its mode as the archive gives it, and its path as the
/// base serves it, from `/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestLine<'a> {
    pub digest: [u8; 32],
    pub mode: u32,
    pub path: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadManifest {
    /// From 1.
    pub line: usize,
}

impl ManifestLine<'_> {
    /// `<sha256 in hex> <mode in octal> <path>` and a line feed; refused for
    /// a path that is relative, too long or holds a control character.
    pub fn write(&self, out: &mut impl Write) -> fmt::Result {
        if !manifest_path_ok(self.path) {
            return Err(fmt::Error);
        }
        for byte in self.digest {
            write!(out, "{byte:02x}")?;
        }
        writeln!(out, " {:06o} {}", self.mode, self.path)
    }
}

fn manifest_path_ok(path: &str) -> bool {
    path.starts_with('/')
        && path.len() < USER_PATH_MAX
        && !path.bytes().any(|b| b.is_ascii_control())
}

fn octal_mode(text: &str) -> Option<u32> {
    let digits = text.as_bytes();
    if digits.len() != 6 || !digits.iter().all(|b| (b'0'..=b'7').contains(b)) {
        return None;
    }
    u32::from_str_radix(text, 8).ok()
}

fn hex_digest(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(bytes.chunks_exact(2)) {
        let nibble = |b: u8| match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        };
        *byte = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(digest)
}

pub fn manifest_lines(text: &str) -> impl Iterator<Item = Result<ManifestLine<'_>, BadManifest>> {
    text.split_terminator('\n').enumerate().map(|(i, line)| {
        let bad = BadManifest { line: i + 1 };
        let (digest, rest) = line.split_once(' ').ok_or(bad)?;
        let (mode, path) = rest.split_once(' ').ok_or(bad)?;
        match (hex_digest(digest), octal_mode(mode)) {
            (Some(digest), Some(mode)) if manifest_path_ok(path) => {
                Ok(ManifestLine { digest, mode, path })
            }
            _ => Err(bad),
        }
    })
}

/// Whether any of `paths` reads differently in manifest `new` than in `old`:
/// a path ending in `/` stands for everything beneath it.
pub fn changed(old: &str, new: &str, paths: &[&str]) -> bool {
    paths.iter().any(|path| {
        let under = |line: &ManifestLine<'_>| match path.strip_suffix('/') {
            Some(_) => line.path.starts_with(path),
            None => line.path == *path,
        };
        let lines = |text| manifest_lines(text).flatten().filter(under);
        lines(old).count() != lines(new).count()
            || lines(old).any(|was| !lines(new).any(|is| is == was))
    })
}

#[cfg(test)]
mod tests;
