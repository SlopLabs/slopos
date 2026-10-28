//! An extension's image: a read-only ext4 volume ([`crate::volume`]) and
//! then its SHA-256 tree ([`slopos_verity_core`]), whose root is the image's
//! digest, so any part of it is read verified on its own. The volume holds a
//! manifest at [`MANIFEST`] of `<sha256> <mode> <path>` for everything else
//! beneath its root but `lost+found`, its paths those they have below
//! `/opt/<name>`, under [`TOP`] alone, and holding [`RELEASE`] and
//! [`DECLARE`]. Every program the declaration names is an executable file of
//! it, and every symlink stays inside it.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Bound;

use slopos_abi::extension::MAX_ENTRIES;
use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_boot_core::base::{ManifestLine, manifest_lines};
use slopos_verity_core::sha256::Sha256;
use slopos_verity_core::{self as verity, BLOCK, Digest, Fault, Geometry};

use crate::declare::{Declaration, mount_point};
use crate::release::Release;
use crate::sha256;
use crate::volume::read::{Blocks, Found, Volume, VolumeError};
use crate::volume::write::{self, Entry, What};
use crate::volume::{Kind, LOST_FOUND_NAME};
use crate::words::LineError;

pub const DIR: &str = "/.extension";
pub const MANIFEST: &str = "/.extension/manifest";
pub const RELEASE: &str = "/.extension/release";
pub const DECLARE: &str = "/.extension/declare";
/// Where an image's records go.
pub const TOP: [&str; 5] = ["/bin", "/libexec", "/lib", "/share", DIR];
const APPS: &str = "/share/slopos/apps";
const MANIFEST_MAX: u64 = 16 << 20;
const TEXT_MAX: u64 = 64 << 10;
/// Tree blocks a reader keeps verified, so a file's reads walk to the root
/// once.
const KEPT: usize = 64;
/// What a reader takes of a file at once.
const CHUNK: usize = 64 << 10;

/// Where an image's bytes come from.
pub trait Source {
    type Error: fmt::Display;

    fn len(&mut self) -> Result<u64, Self::Error>;

    /// Fill `out` from `offset`.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Self::Error>;
}

impl Source for &[u8] {
    type Error = &'static str;

    fn len(&mut self) -> Result<u64, &'static str> {
        Ok(<[u8]>::len(self) as u64)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), &'static str> {
        let at = usize::try_from(offset).map_err(|_| "past the end")?;
        let bytes = self
            .get(at..at.saturating_add(out.len()))
            .ok_or("past the end")?;
        out.copy_from_slice(bytes);
        Ok(())
    }
}

impl<S: Source + ?Sized> Source for &mut S {
    type Error = S::Error;

    fn len(&mut self) -> Result<u64, S::Error> {
        (**self).len()
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), S::Error> {
        (**self).read_at(offset, out)
    }
}

#[cfg(any(test, feature = "std"))]
impl Source for std::fs::File {
    type Error = std::io::Error;

    fn len(&mut self) -> std::io::Result<u64> {
        self.metadata().map(|meta| meta.len())
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, out, offset)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// Not an image this system runs: why.
    Format(String),
    /// Bytes that do not hold as its tree or its volume says.
    Damaged(String),
    /// Not the image whose root the caller named.
    Other,
    /// What is not as the manifest says.
    Sealed(String),
    /// A path outside [`TOP`], or in [`DIR`] but none of its files.
    Placed(String),
    /// A symlink whose target is absolute or climbs through `..`.
    Escapes(String),
    Missing(&'static str),
    Release(LineError),
    Declare(LineError),
    /// A program, file or entry the declaration names that the image holds
    /// no such thing at.
    Undelivered(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Format(why) => write!(f, "{why}"),
            ImageError::Damaged(why) => write!(f, "{why}"),
            ImageError::Other => f.write_str("not the image whose root was named"),
            ImageError::Sealed(what) => write!(f, "{what}: not what {MANIFEST} says"),
            ImageError::Placed(path) => write!(
                f,
                "{path}: outside /bin, /libexec, /lib, /share and {DIR}'s own files"
            ),
            ImageError::Escapes(path) => {
                write!(f, "{path}: a symlink that may lead out of the image")
            }
            ImageError::Missing(path) => write!(f, "no {path}"),
            ImageError::Release(e) => write!(f, "{RELEASE}: {e}"),
            ImageError::Declare(e) => write!(f, "{DECLARE}: {e}"),
            ImageError::Undelivered(what) => {
                write!(f, "{DECLARE} names {what}, which the image does not hold")
            }
        }
    }
}

impl<E: fmt::Display> From<VolumeError<E>> for ImageError {
    fn from(e: VolumeError<E>) -> Self {
        match e {
            VolumeError::Read(e) => ImageError::Damaged(e.to_string()),
            VolumeError::TooMany => ImageError::Format(format!(
                "more than {MAX_ENTRIES} entries, or a file larger than it may be read whole"
            )),
            e => ImageError::Format(e.to_string()),
        }
    }
}

/// The geometry of the image `source` holds and its root, the digest of its
/// tree's top block.
pub fn root_of<S: Source>(source: &mut S) -> Result<(Geometry, Digest), ImageError> {
    let read = |e: S::Error| ImageError::Damaged(e.to_string());
    let len = source.len().map_err(read)?;
    let geometry = Geometry::of_image(len).ok_or_else(|| {
        ImageError::Format("no extension image: its length fits no SHA-256 tree".into())
    })?;
    let mut top = Box::new([0u8; BLOCK]);
    source
        .read_at(geometry.top() * BLOCK as u64, &mut top[..])
        .map_err(read)?;
    Ok((geometry, verity::hash(&top)))
}

/// Hold every block of the image `source` holds to `digest`, a level of its
/// tree at a time, in room that does not grow with the image. What a reader
/// verifies a block at a time is then whole.
pub fn verify_whole<S: Source>(source: &mut S, digest: &Digest) -> Result<(), ImageError> {
    let (geometry, root) = root_of(source)?;
    if root != *digest {
        return Err(ImageError::Other);
    }
    let mut ahead = Whole(Ahead::new(source, geometry.data_blocks()));
    let (mut read, mut built) = (Box::new([0; BLOCK]), Box::new([0; BLOCK]));
    verity::verify_all(&geometry, &root, &mut ahead, &mut read, &mut built).map_err(|fault| {
        match fault {
            Fault::Read(e) => ImageError::Damaged(e.to_string()),
            Fault::Mismatch(_) | Fault::OutOfRange => {
                ImageError::Damaged("its tree is not the one its data makes".into())
            }
        }
    })
}

/// An image's data blocks read a chunk at a time, as a reader takes them in
/// order; tree blocks bypass the chunk.
struct Ahead<S> {
    source: S,
    data_blocks: u64,
    chunk: Vec<u8>,
    first: u64,
    held: u64,
}

impl<S: Source> Ahead<S> {
    fn new(source: S, data_blocks: u64) -> Self {
        Self {
            source,
            data_blocks,
            chunk: vec![0; CHUNK],
            first: 0,
            held: 0,
        }
    }

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), S::Error> {
        if block >= self.data_blocks {
            return self.source.read_at(block * BLOCK as u64, out);
        }
        if !(self.first..self.first + self.held).contains(&block) {
            let held = (self.data_blocks - block).min((CHUNK / BLOCK) as u64);
            self.source.read_at(
                block * BLOCK as u64,
                &mut self.chunk[..held as usize * BLOCK],
            )?;
            self.first = block;
            self.held = held;
        }
        let at = (block - self.first) as usize * BLOCK;
        out.copy_from_slice(&self.chunk[at..at + BLOCK]);
        Ok(())
    }
}

struct Whole<'s, S>(Ahead<&'s mut S>);

impl<S: Source> verity::Source for Whole<'_, S> {
    type Error = S::Error;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), S::Error> {
        self.0.read(block, out)
    }

    fn verified(&mut self, _: u64, _: &mut [u8; BLOCK]) -> bool {
        false
    }

    fn keep(&mut self, _: u64, _: &[u8; BLOCK]) {}
}

/// An image's data as its tree vouches for it.
pub struct Verified<S> {
    source: Ahead<S>,
    geometry: Geometry,
    root: Digest,
    /// Tree blocks verified, the most recent last.
    kept: Vec<(u64, Box<[u8; BLOCK]>)>,
    scratch: Box<[u8; BLOCK]>,
}

struct Tree<'a, S> {
    source: &'a mut Ahead<S>,
    kept: &'a mut Vec<(u64, Box<[u8; BLOCK]>)>,
}

impl<S: Source> verity::Source for Tree<'_, S> {
    type Error = S::Error;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), S::Error> {
        self.source.read(block, out)
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        let Some(at) = self.kept.iter().position(|(b, _)| *b == block) else {
            return false;
        };
        let entry = self.kept.remove(at);
        out.copy_from_slice(&entry.1[..]);
        self.kept.push(entry);
        true
    }

    fn keep(&mut self, block: u64, verified: &[u8; BLOCK]) {
        let mut copy = if self.kept.len() == KEPT {
            self.kept.remove(0).1
        } else {
            Box::new([0; BLOCK])
        };
        copy.copy_from_slice(verified);
        self.kept.push((block, copy));
    }
}

impl<S: Source> Blocks for Verified<S> {
    type Error = String;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), String> {
        let mut tree = Tree {
            source: &mut self.source,
            kept: &mut self.kept,
        };
        verity::read(
            &self.geometry,
            &self.root,
            &mut tree,
            block,
            out,
            &mut self.scratch,
        )
        .map_err(|fault| match fault {
            verity::Fault::Read(e) => e.to_string(),
            verity::Fault::Mismatch(block) => {
                format!("block {block} does not hold what the image's tree says")
            }
            verity::Fault::OutOfRange => format!("block {block} is past the image's data"),
        })
    }
}

pub struct Image<S> {
    pub release: Release,
    pub declaration: Declaration,
    pub manifest: Vec<u8>,
    /// The root of its tree.
    pub digest: Digest,
    entries: BTreeMap<String, Found>,
    links: BTreeMap<String, String>,
    volume: Volume<Verified<S>>,
}

impl<S> fmt::Debug for Image<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("release", &self.release)
            .field("digest", &crate::hex(&self.digest))
            .finish_non_exhaustive()
    }
}

fn beneath(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// A symlink's target names a path of the image wherever it is shown:
/// relative, holding no NUL, and never through `..`.
fn inside_image(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('/')
        && !target.contains('\0')
        && target.split('/').all(|c| c != "..")
}

fn placed(found: &Found) -> Result<(), ImageError> {
    let path = found.path.as_str();
    let own = [DIR, MANIFEST, RELEASE, DECLARE];
    if !TOP.iter().any(|top| beneath(path, top)) || (beneath(path, DIR) && !own.contains(&path)) {
        return Err(ImageError::Placed(path.into()));
    }
    Ok(())
}

fn type_bits(kind: Kind) -> u32 {
    match kind {
        Kind::Directory => S_IFDIR,
        Kind::File => S_IFREG,
        Kind::Symlink => S_IFLNK,
    }
}

/// The bytes of the regular file at `path`, refused past `max`.
fn text<S: Source>(
    volume: &mut Volume<Verified<S>>,
    entries: &BTreeMap<String, Found>,
    path: &'static str,
    max: u64,
) -> Result<Vec<u8>, ImageError> {
    let found = entries
        .get(path)
        .filter(|f| f.kind == Kind::File)
        .ok_or(ImageError::Missing(path))?;
    if found.size > max {
        return Err(ImageError::Format(format!(
            "{path}: larger than {max} bytes"
        )));
    }
    let inode = volume.inode(found.ino)?;
    Ok(volume.read_all(&inode, max)?)
}

/// The manifest names everything but itself once, in order, as what it is.
fn sealed(entries: &BTreeMap<String, Found>, manifest: &[u8]) -> Result<(), ImageError> {
    let text = core::str::from_utf8(manifest).map_err(|_| ImageError::Sealed(MANIFEST.into()))?;
    let mut named = 0;
    let mut last: Option<&str> = None;
    for line in manifest_lines(text) {
        let line = line.map_err(|bad| ImageError::Sealed(format!("line {}", bad.line)))?;
        let fits = entries.get(line.path).is_some_and(|found| {
            line.path != MANIFEST
                && line.mode == type_bits(found.kind) | u32::from(found.mode & 0o7777)
        });
        if !fits || last.is_some_and(|last| last >= line.path) {
            return Err(ImageError::Sealed(line.path.into()));
        }
        last = Some(line.path);
        named += 1;
    }
    if named + 1 != entries.len() {
        let mut lines = manifest_lines(text).flatten().map(|line| line.path);
        let unnamed = entries
            .keys()
            .filter(|path| *path != MANIFEST)
            .find(|path| lines.next() != Some(path.as_str()))
            .cloned()
            .unwrap_or_default();
        return Err(ImageError::Sealed(unnamed));
    }
    Ok(())
}

impl<S: Source> Image<S> {
    /// The image `source` holds, every file of it held to the manifest.
    pub fn check(source: S) -> Result<Self, ImageError> {
        let mut image = Self::open(source, None)?;
        image.check_contents()?;
        Ok(image)
    }

    /// The image `source` holds, under `digest` when one is named, its tree
    /// held to the manifest; [`Self::check_contents`] holds the files' bytes
    /// to it, and every read verifies them against the root.
    pub fn open(mut source: S, digest: Option<Digest>) -> Result<Self, ImageError> {
        let (geometry, root) = root_of(&mut source)?;
        if digest.is_some_and(|want| want != root) {
            return Err(ImageError::Other);
        }
        let verified = Verified {
            source: Ahead::new(source, geometry.data_blocks()),
            geometry,
            root,
            kept: Vec::new(),
            scratch: Box::new([0; BLOCK]),
        };
        let mut volume = Volume::open(verified, geometry.data_blocks())?;
        let lost_found = format!("/{}", String::from_utf8_lossy(LOST_FOUND_NAME));
        let mut entries = BTreeMap::new();
        let mut links = BTreeMap::new();
        for found in volume.walk(MAX_ENTRIES, MANIFEST_MAX as usize)? {
            if found.path == lost_found && found.kind == Kind::Directory {
                continue;
            }
            placed(&found)?;
            if found.kind == Kind::Symlink {
                let inode = volume.inode(found.ino)?;
                let target = volume.read_all(&inode, BLOCK as u64)?;
                let target = String::from_utf8(target)
                    .ok()
                    .filter(|t| inside_image(t))
                    .ok_or_else(|| ImageError::Escapes(found.path.clone()))?;
                links.insert(found.path.clone(), target);
            }
            entries.insert(found.path.clone(), found);
        }
        let manifest = text(&mut volume, &entries, MANIFEST, MANIFEST_MAX)?;
        sealed(&entries, &manifest)?;
        let release = text(&mut volume, &entries, RELEASE, TEXT_MAX)?;
        let release = core::str::from_utf8(&release).map_err(|_| ImageError::Missing(RELEASE))?;
        let release = Release::parse(release).map_err(ImageError::Release)?;
        let declare = text(&mut volume, &entries, DECLARE, TEXT_MAX)?;
        let declare = core::str::from_utf8(&declare).map_err(|_| ImageError::Missing(DECLARE))?;
        let declaration =
            Declaration::parse(&release.name, declare).map_err(ImageError::Declare)?;
        let image = Image {
            release,
            declaration,
            manifest,
            digest: root,
            entries,
            links,
            volume,
        };
        image.delivers()?;
        Ok(image)
    }

    /// Hold every file's bytes and every symlink's target to the digest the
    /// manifest records for it.
    pub fn check_contents(&mut self) -> Result<(), ImageError> {
        let text = String::from_utf8(self.manifest.clone()).unwrap_or_default();
        for line in manifest_lines(&text).flatten() {
            let digest = match self.entries.get(line.path).map(|f| (f.kind, f.ino)) {
                Some((Kind::File, ino)) => self.hash(ino)?,
                Some((Kind::Symlink, _)) => sha256(self.links[line.path].as_bytes()),
                _ => sha256(b""),
            };
            if digest != line.digest {
                return Err(ImageError::Sealed(line.path.into()));
            }
        }
        Ok(())
    }

    fn hash(&mut self, ino: u32) -> Result<Digest, ImageError> {
        let inode = self.volume.inode(ino)?;
        let mut hash = Sha256::new();
        let mut chunk = vec![0; CHUNK];
        let mut at = 0;
        while at < inode.size {
            let n = self.volume.read(&inode, at, &mut chunk)?;
            hash.update(&chunk[..n]);
            at += n as u64;
        }
        Ok(hash.finish())
    }

    /// How long it is: its volume and its tree.
    pub fn bytes(&self) -> u64 {
        self.volume.blocks().geometry.image_bytes()
    }

    /// Where its bytes come from.
    pub fn source(&self) -> &S {
        &self.volume.blocks().source.source
    }

    /// What is at `path`, a path below the mount point.
    pub fn entry(&self, path: &str) -> Option<&Found> {
        self.entries.get(path)
    }

    /// Every regular file.
    pub fn files(&self) -> impl Iterator<Item = &Found> {
        self.entries.values().filter(|f| f.kind == Kind::File)
    }

    /// Every symlink, by its path below the mount point, and its target.
    pub fn links(&self) -> impl Iterator<Item = (&str, &str)> {
        self.links
            .iter()
            .map(|(path, target)| (path.as_str(), target.as_str()))
    }

    /// The bytes of the regular file at `path`, refused past `max`.
    pub fn read(&mut self, path: &str, max: u64) -> Result<Vec<u8>, ImageError> {
        let found = self
            .entries
            .get(path)
            .filter(|f| f.kind == Kind::File)
            .ok_or_else(|| ImageError::Undelivered(path.into()))?;
        let inode = self.volume.inode(found.ino)?;
        Ok(self.volume.read_all(&inode, max)?)
    }

    /// Hand `each` every file that starts as an ELF file does, with its
    /// bytes, one at a time, each refused past `max`; an error of `each` is
    /// answered as `Err(Err(_))`.
    pub fn each_elf<E>(
        &mut self,
        max: u64,
        each: &mut dyn FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), Result<ImageError, E>> {
        let files: Vec<(String, u32)> = self.files().map(|f| (f.path.clone(), f.ino)).collect();
        for (path, ino) in files {
            let inode = self.volume.inode(ino).map_err(|e| Ok(e.into()))?;
            let mut magic = [0u8; 4];
            let n = self
                .volume
                .read(&inode, 0, &mut magic)
                .map_err(|e| Ok(e.into()))?;
            if n == 4 && crate::elf::is_elf(&magic) {
                let bytes = self
                    .volume
                    .read_all(&inode, max)
                    .map_err(|e| Ok(e.into()))?;
                each(&path, &bytes).map_err(Err)?;
            }
        }
        Ok(())
    }

    /// What `/opt/<name>/<rest>` is in the image: `/<rest>`.
    fn inside<'p>(&self, path: &'p str) -> Option<&'p str> {
        path.strip_prefix(&mount_point(&self.release.name))
            .filter(|rest| rest.starts_with('/'))
    }

    fn delivers(&self) -> Result<(), ImageError> {
        let undelivered = |what: &str| ImageError::Undelivered(what.to_string());
        let declaration = &self.declaration;
        for program in declaration.programs(&self.release.name) {
            let executable = self
                .inside(&program)
                .and_then(|path| self.entry(path))
                .is_some_and(|e| e.kind == Kind::File && e.mode & 0o111 != 0);
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
                    self.entry(dir).is_some_and(|e| e.kind == Kind::Directory)
                        || self
                            .entries
                            .range::<str, _>((Bound::Included(path), Bound::Unbounded))
                            .next()
                            .is_some_and(|(p, _)| p.starts_with(path))
                }
                None => self.entry(path).is_some_and(|e| e.kind != Kind::Directory),
            };
            if !found {
                return Err(undelivered(read));
            }
        }
        for app in &declaration.apps {
            let entry = format!("{APPS}/{app}.app");
            if !self.entry(&entry).is_some_and(|e| e.kind == Kind::File) {
                return Err(undelivered(&entry));
            }
        }
        Ok(())
    }
}

enum Content {
    Bytes(Vec<u8>),
    #[cfg(any(test, feature = "std"))]
    File(std::path::PathBuf, u64),
}

impl Content {
    fn len(&self) -> u64 {
        match self {
            Content::Bytes(bytes) => bytes.len() as u64,
            #[cfg(any(test, feature = "std"))]
            Content::File(_, len) => *len,
        }
    }
}

/// An image under construction: what it holds, then the release, the
/// declaration and the manifest.
#[derive(Default)]
pub struct Packer {
    records: Vec<(String, u32, Content)>,
}

/// Room for one file's bytes as [`write::Layout::write`] asks for them.
struct Reader<'p> {
    records: &'p [(String, u32, Content)],
    #[cfg(any(test, feature = "std"))]
    open: Option<Opened>,
}

/// A record's file, open, and the piece of it read last.
#[cfg(any(test, feature = "std"))]
struct Opened {
    record: usize,
    file: std::fs::File,
    held: Vec<u8>,
    at: u64,
}

impl Reader<'_> {
    fn fill(&mut self, record: usize, offset: u64, room: &mut [u8]) -> Result<(), String> {
        match &self.records[record].2 {
            Content::Bytes(bytes) => {
                let at = offset as usize;
                room.copy_from_slice(&bytes[at..at + room.len()]);
                Ok(())
            }
            #[cfg(any(test, feature = "std"))]
            Content::File(path, len) => {
                let shown = |e: std::io::Error| format!("{}: {e}", path.display());
                if self.open.as_ref().is_none_or(|o| o.record != record) {
                    let file = std::fs::File::open(path).map_err(shown)?;
                    self.open = Some(Opened {
                        record,
                        file,
                        held: Vec::new(),
                        at: 0,
                    });
                }
                let Some(opened) = self.open.as_mut() else {
                    return Err(format!("{}: not open", path.display()));
                };
                let end = offset + room.len() as u64;
                let held_end = opened.at + opened.held.len() as u64;
                if offset < opened.at || end > held_end {
                    let take = (len - offset).min(CHUNK as u64).max(room.len() as u64);
                    opened.held.resize(take as usize, 0);
                    Source::read_at(&mut opened.file, offset, &mut opened.held).map_err(shown)?;
                    opened.at = offset;
                }
                let from = (offset - opened.at) as usize;
                room.copy_from_slice(&opened.held[from..from + room.len()]);
                Ok(())
            }
        }
    }

    fn digest(&mut self, record: usize) -> Result<Digest, String> {
        let len = self.records[record].2.len();
        let mut hash = Sha256::new();
        let mut chunk = vec![0; CHUNK];
        let mut at = 0;
        while at < len {
            let n = (len - at).min(CHUNK as u64) as usize;
            self.fill(record, at, &mut chunk[..n])?;
            hash.update(&chunk[..n]);
            at += n as u64;
        }
        Ok(hash.finish())
    }
}

impl Packer {
    /// `path` below the mount point, a directory, file or symlink by `mode`;
    /// a symlink's data is its target.
    pub fn add(&mut self, path: &str, mode: u32, data: Vec<u8>) {
        self.records
            .push((path.to_string(), mode, Content::Bytes(data)));
    }

    /// The regular file at `path` below the mount point, its bytes read from
    /// `from` as the image is written.
    #[cfg(any(test, feature = "std"))]
    pub fn add_file(
        &mut self,
        path: &str,
        mode: u32,
        from: &std::path::Path,
    ) -> std::io::Result<()> {
        let len = std::fs::metadata(from)?.len();
        self.records.push((
            path.to_string(),
            mode,
            Content::File(from.to_path_buf(), len),
        ));
        Ok(())
    }

    /// The image, in memory, once it checks out.
    pub fn finish(self, release: &str, declare: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        self.write(release, declare, &mut |block| {
            out.extend_from_slice(block);
            Ok(())
        })?;
        Image::check(&out[..]).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Hand the image to `sink` a block at a time, the volume and then its
    /// tree; answers its root.
    pub fn write(
        mut self,
        release: &str,
        declare: &str,
        sink: &mut dyn FnMut(&[u8; BLOCK]) -> Result<(), String>,
    ) -> Result<Digest, String> {
        let file = S_IFREG | 0o644;
        self.records
            .retain(|(path, _, _)| ![RELEASE, DECLARE, MANIFEST].contains(&path.as_str()));
        self.records.push((
            RELEASE.into(),
            file,
            Content::Bytes(release.as_bytes().to_vec()),
        ));
        self.records.push((
            DECLARE.into(),
            file,
            Content::Bytes(declare.as_bytes().to_vec()),
        ));
        let mut dirs: Vec<String> = Vec::new();
        for (path, _, _) in &self.records {
            let mut at = 0;
            while let Some(next) = path[at + 1..].find('/') {
                at += next + 1;
                dirs.push(path[..at].to_string());
            }
        }
        for dir in dirs {
            if !self.records.iter().any(|(path, _, _)| *path == dir) {
                self.records
                    .push((dir, S_IFDIR | 0o755, Content::Bytes(Vec::new())));
            }
        }
        self.records.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some(pair) = self.records.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(format!("{}: packed twice", pair[0].0));
        }
        let mut reader = Reader {
            records: &self.records,
            #[cfg(any(test, feature = "std"))]
            open: None,
        };
        let mut manifest = String::new();
        for (i, (path, mode, content)) in self.records.iter().enumerate() {
            let digest = match (mode & S_IFMT, content) {
                (S_IFREG, _) => reader.digest(i)?,
                (_, Content::Bytes(bytes)) => sha256(bytes),
                #[cfg(any(test, feature = "std"))]
                _ => return Err(format!("{path}: no regular file")),
            };
            ManifestLine {
                digest,
                mode: *mode,
                path,
            }
            .write(&mut manifest)
            .map_err(|_| format!("{path}: no manifest line takes it"))?;
        }
        let manifest = manifest.into_bytes();
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&sha256(&manifest)[..16]);
        let mut entries = Vec::with_capacity(self.records.len() + 1);
        for (path, mode, content) in &self.records {
            let what = match (mode & S_IFMT, content) {
                (S_IFDIR, _) => What::Directory,
                (S_IFREG, content) => What::File(content.len()),
                (S_IFLNK, Content::Bytes(target)) => What::Symlink(target.clone()),
                _ => return Err(format!("{path}: no directory, file or symlink")),
            };
            entries.push(Entry {
                path: path.clone(),
                perm: (mode & 0o7777) as u16,
                what,
            });
        }
        entries.push(Entry {
            path: MANIFEST.into(),
            perm: 0o644,
            what: What::File(manifest.len() as u64),
        });
        let manifest_entry = entries.len() - 1;
        let layout = write::layout(&entries, uuid).map_err(|e| e.to_string())?;
        let geometry = Geometry::new(layout.blocks()).ok_or("larger than an image can be")?;
        let mut tree = vec![0; geometry.tree_blocks() as usize * BLOCK];
        let mut builder =
            verity::Builder::new(geometry, &mut tree).ok_or("no room for the image's tree")?;
        layout.write(
            &mut |entry, offset, room| {
                if entry == manifest_entry {
                    let at = offset as usize;
                    room.copy_from_slice(&manifest[at..at + room.len()]);
                    Ok(())
                } else {
                    reader.fill(entry, offset, room)
                }
            },
            &mut |block| {
                builder.push(block);
                sink(block)
            },
        )?;
        let root = builder
            .finish()
            .ok_or("the volume wrote fewer blocks than it has")?;
        for block in tree.chunks_exact(BLOCK) {
            if let Ok(block) = block.try_into() {
                sink(block)?;
            }
        }
        Ok(root)
    }
}

#[cfg(test)]
mod tests;
