//! A package's image: a read-only ext4 volume ([`crate::volume`]) and
//! then its SHA-256 tree ([`slopos_verity_core`]), whose root is the image's
//! digest, so any part of it is read verified on its own. The volume holds a
//! manifest at [`MANIFEST`] of `<sha256> <mode> <path>` for everything else
//! beneath its root but `lost+found`, its paths those they have below
//! `/opt/<name>`, under [`TOP`] alone, and holding [`RELEASE`] and
//! [`DECLARE`]. Every program the declaration names is an executable file of
//! it, a command's perhaps through its links, and every symlink stays inside
//! it.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ops::Bound;

use slopos_abi::fs::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_abi::package::MAX_ENTRIES;
use slopos_boot_core::base::{ManifestLine, manifest_lines};
use slopos_verity_core::sha256::{Compress, Sha256, compress_blocks};
use slopos_verity_core::{self as verity, BLOCK, DIGEST, Digest, Fault, Geometry};

use crate::declare::{Declaration, mount_point};
use crate::release::Release;
use crate::volume::read::{Blocks, Found, Volume, VolumeError};
use crate::volume::write::{self, Entry, What};
use crate::volume::{Kind, LOST_FOUND_NAME};
use crate::words::LineError;
use crate::{sha256, symbols};

pub const DIR: &str = "/.package";
pub const MANIFEST: &str = "/.package/manifest";
pub const RELEASE: &str = "/.package/release";
pub const DECLARE: &str = "/.package/declare";
/// Where an image's records go.
pub const TOP: [&str; 5] = ["/bin", "/libexec", "/lib", "/share", DIR];
const APPS: &str = "/share/slopos/apps";
const MANIFEST_MAX: u64 = 16 << 20;
/// More symlinks than a library or a command is ever reached through.
pub(crate) const LINK_HOPS: usize = 8;
const TEXT_MAX: u64 = 64 << 10;
/// Leaves a reader keeps verified, so a file's reads walk to the root once.
const KEPT: usize = 64;
/// The most of the levels above the leaves a reader holds from its open, as
/// the kernel's device does.
const UPPER_MAX: u64 = 256;
/// What a reader takes of a file at once.
const CHUNK: usize = 1 << 20;
/// Small under test, so a test image spans several.
const COPY_CHUNK: usize = if cfg!(test) { 64 << 10 } else { 4 << 20 };
/// The fewest blocks one core hashes of a chunk, so a small image is hashed
/// on one.
#[cfg(any(test, feature = "std"))]
const SHARE_MIN: usize = 128;
/// A hashing thread's stack: a digest's room, not std's 2 MiB.
#[cfg(any(test, feature = "std"))]
const HASHER_STACK: usize = 64 << 10;

/// Where an image's bytes come from.
pub trait Source {
    type Error: fmt::Display;

    fn len(&mut self) -> Result<u64, Self::Error>;

    /// Fill `out` from `offset`.
    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<(), Self::Error>;

    /// What its blocks are hashed with.
    fn compress(&self) -> Compress {
        compress_blocks
    }

    fn cores(&self) -> usize {
        1
    }
}

fn hash_each(compress: Compress, data: &[u8], out: &mut [Digest]) {
    for (block, digest) in data.chunks_exact(BLOCK).zip(out.iter_mut()) {
        if let Ok(block) = block.try_into() {
            *digest = verity::hash_using(compress, block);
        }
    }
}

fn digests_of(compress: Compress, cores: usize, data: &[u8], out: &mut [Digest]) {
    #[cfg(any(test, feature = "std"))]
    hash_blocks(compress, data, out, cores);
    #[cfg(not(any(test, feature = "std")))]
    {
        let _ = cores;
        hash_each(compress, data, out);
    }
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

    fn compress(&self) -> Compress {
        (**self).compress()
    }

    fn cores(&self) -> usize {
        (**self).cores()
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
        ImageError::Format("no package image: its length fits no SHA-256 tree".into())
    })?;
    let mut top = Box::new([0u8; BLOCK]);
    source
        .read_at(geometry.top() * BLOCK as u64, &mut top[..])
        .map_err(read)?;
    Ok((geometry, verity::hash(&top)))
}

fn damaged<E: fmt::Display>(e: E) -> ImageError {
    ImageError::Damaged(e.to_string())
}

/// Room for `blocks` blocks of an image's tree, refused rather than
/// aborting when this process cannot have it.
fn room(blocks: u64) -> Result<Vec<u8>, ImageError> {
    let refused = || ImageError::Format("a tree larger than this can hold".into());
    let bytes = usize::try_from(blocks)
        .ok()
        .and_then(|blocks| blocks.checked_mul(BLOCK))
        .ok_or_else(refused)?;
    let mut room = Vec::new();
    room.try_reserve_exact(bytes).map_err(|_| refused())?;
    room.resize(bytes, 0);
    Ok(room)
}

/// The image's blocks from its tree's top to the end of level `lowest`, read
/// a chunk at a time: what [`verity::verify_tree`] holds to the root.
fn tree_of<S: Source>(
    source: &mut S,
    geometry: &Geometry,
    lowest: usize,
) -> Result<Vec<u8>, ImageError> {
    let end = geometry
        .level(lowest)
        .map_or(geometry.top() + 1, |(start, count)| start + count);
    let mut tree = room(end - geometry.top())?;
    let from = geometry.top() * BLOCK as u64;
    for (k, room) in tree.chunks_mut(CHUNK).enumerate() {
        source
            .read_at(from + (k * CHUNK) as u64, room)
            .map_err(damaged)?;
    }
    Ok(tree)
}

fn untrue(fault: Fault<()>) -> ImageError {
    match fault {
        Fault::Mismatch(block) => {
            ImageError::Damaged(format!("tree block {block} is not what the root says"))
        }
        Fault::Read(()) | Fault::OutOfRange => ImageError::Damaged("its tree is cut short".into()),
    }
}

/// Only the tree is read: every reader holds each data block to it.
pub fn verify_tree<S: Source>(source: &mut S, digest: &Digest) -> Result<(), ImageError> {
    let (geometry, root) = root_of(source)?;
    if root != *digest {
        return Err(ImageError::Other);
    }
    let compress = source.compress();
    let Some((start, count)) = geometry.level(0).filter(|_| geometry.levels() >= 2) else {
        let tree = tree_of(source, &geometry, 0)?;
        return verity::verify_tree(&geometry, &root, compress, &tree, 0).map_err(untrue);
    };
    let upper = tree_of(source, &geometry, 1)?;
    verity::verify_tree(&geometry, &root, compress, &upper, 1).map_err(untrue)?;
    let per = (COPY_CHUNK / BLOCK) as u64;
    let mut leaves = vec![0; count.min(per) as usize * BLOCK];
    let mut first = 0;
    while first < count {
        let n = (count - first).min(per) as usize;
        source
            .read_at((start + first) * BLOCK as u64, &mut leaves[..n * BLOCK])
            .map_err(damaged)?;
        verity::verify_leaves(&geometry, compress, &upper, first, &leaves[..n * BLOCK])
            .map_err(untrue)?;
        first += n as u64;
    }
    Ok(())
}

pub trait Sink {
    fn buffer(&mut self) -> Result<Vec<u8>, String>;

    /// All-zero blocks of `bytes` are holes.
    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), String>;
}

/// What reaches `sink` is the image `digest` names once this answers `Ok`:
/// the bytes handed over are the bytes hashed. So is what `seen` derives.
fn copy<S: Source + Send>(
    source: &mut S,
    len: u64,
    digest: &Digest,
    sink: &mut (dyn Sink + Send),
    seen: &mut (dyn FnMut(u64, &[u8]) + Send),
) -> Result<(), ImageError> {
    let geometry = Geometry::of_image(len).ok_or_else(|| {
        ImageError::Format("no package image: its length fits no SHA-256 tree".into())
    })?;
    let top = geometry.top();
    let leaves_at = geometry.level(0).map_or(top, |(start, _)| start);
    let mut upper = room(leaves_at - top)?;
    let mut leaf = Box::new([0; BLOCK]);
    let compress = source.compress();
    let tree = verity::Streamed::new(geometry, compress, &mut upper, &mut leaf)
        .ok_or_else(|| untrue(Fault::OutOfRange))?;
    let stage = Stage {
        out: Out {
            sink,
            leaves: None,
            leaves_at: leaves_at * BLOCK as u64,
        },
        tree,
        digests: vec![[0; DIGEST]; COPY_CHUNK / BLOCK],
        seen,
        compress,
        cores: source.cores(),
    };
    let data = geometry.data_blocks() * BLOCK as u64;
    let mut out = read_through(source, data, stage)?.finish(digest)?;
    if !upper.is_empty() {
        out.put(top * BLOCK as u64, upper)?;
    }
    Ok(())
}

struct Stage<'s, 't> {
    out: Out<'s>,
    tree: verity::Streamed<'t>,
    digests: Vec<Digest>,
    seen: &'s mut (dyn FnMut(u64, &[u8]) + Send),
    compress: Compress,
    cores: usize,
}

impl<'s> Stage<'s, '_> {
    fn chunk(&mut self, at: u64, chunk: Vec<u8>) -> Result<(), ImageError> {
        let blocks = chunk.len() / BLOCK;
        let digests = &mut self.digests[..blocks];
        digests_of(self.compress, self.cores, &chunk, digests);
        let out = &mut self.out;
        for digest in digests.iter() {
            self.tree
                .push_digest(*digest, &mut |_, leaf| out.leaf(leaf))?;
        }
        (self.seen)(at, &chunk);
        self.out.put(at, chunk)
    }

    fn finish(self, digest: &Digest) -> Result<Out<'s>, ImageError> {
        let mut out = self.out;
        if self.tree.finish(&mut |_, leaf| out.leaf(leaf))? != Some(*digest) {
            return Err(ImageError::Damaged(
                "its data is not the data its tree is of".into(),
            ));
        }
        out.flush()?;
        Ok(out)
    }
}

#[cfg(any(test, feature = "std"))]
const READ_AHEAD: usize = 2;

#[cfg(any(test, feature = "std"))]
fn read_through<'s, 't, S: Source + Send>(
    source: &mut S,
    data: u64,
    stage: Stage<'s, 't>,
) -> Result<Stage<'s, 't>, ImageError> {
    use std::sync::{Mutex, mpsc};
    let held = Mutex::new(Some(stage));
    let (chunks, read) = mpsc::sync_channel::<(u64, Vec<u8>)>(1);
    let (buffers, empty) = mpsc::sync_channel::<Vec<u8>>(READ_AHEAD);
    std::thread::scope(|scope| {
        let hashing = std::thread::Builder::new().spawn_scoped(scope, || {
            // Owned, so the hasher's end wakes a reader waiting for a buffer.
            let buffers = buffers;
            let taken = held.lock().ok().and_then(|mut held| held.take());
            let mut stage = taken.ok_or_else(|| ImageError::Damaged("no stage".into()))?;
            for _ in 0..READ_AHEAD {
                let _ = buffers.send(stage.out.buffer()?);
            }
            for (at, chunk) in read {
                stage.chunk(at, chunk)?;
                let _ = buffers.send(stage.out.buffer()?);
            }
            Ok(stage)
        });
        let Ok(hashing) = hashing else {
            drop(chunks);
            let stage = held.lock().ok().and_then(|mut held| held.take());
            let stage = stage.ok_or_else(|| ImageError::Damaged("no stage".into()))?;
            return read_here(source, data, stage);
        };
        let mut at = 0;
        let mut fault = Ok(());
        while at < data {
            let Ok(mut chunk) = empty.recv() else {
                break;
            };
            let n = (data - at).min(COPY_CHUNK as u64) as usize;
            chunk.resize(n, 0);
            fault = source.read_at(at, &mut chunk).map_err(damaged);
            if fault.is_err() || chunks.send((at, chunk)).is_err() {
                break;
            }
            at += n as u64;
        }
        drop(chunks);
        drop(empty);
        let hashed = hashing
            .join()
            .unwrap_or_else(|_| Err(ImageError::Damaged("its hashing stopped".into())));
        fault.and(hashed)
    })
}

#[cfg(not(any(test, feature = "std")))]
fn read_through<'s, 't, S: Source>(
    source: &mut S,
    data: u64,
    stage: Stage<'s, 't>,
) -> Result<Stage<'s, 't>, ImageError> {
    read_here(source, data, stage)
}

fn read_here<'s, 't, S: Source>(
    source: &mut S,
    data: u64,
    mut stage: Stage<'s, 't>,
) -> Result<Stage<'s, 't>, ImageError> {
    let mut at = 0;
    while at < data {
        let n = (data - at).min(COPY_CHUNK as u64) as usize;
        let mut chunk = stage.out.buffer()?;
        chunk.resize(n, 0);
        source.read_at(at, &mut chunk).map_err(damaged)?;
        stage.chunk(at, chunk)?;
        at += n as u64;
    }
    Ok(stage)
}

struct Out<'s> {
    sink: &'s mut (dyn Sink + Send),
    leaves: Option<Vec<u8>>,
    leaves_at: u64,
}

impl Out<'_> {
    fn buffer(&mut self) -> Result<Vec<u8>, ImageError> {
        let mut buffer = self.sink.buffer().map_err(ImageError::Damaged)?;
        buffer.clear();
        Ok(buffer)
    }

    fn put(&mut self, at: u64, bytes: Vec<u8>) -> Result<(), ImageError> {
        self.sink.put(at, bytes).map_err(ImageError::Damaged)
    }

    fn leaf(&mut self, leaf: &[u8; BLOCK]) -> Result<(), ImageError> {
        let mut leaves = match self.leaves.take() {
            Some(leaves) => leaves,
            None => self.buffer()?,
        };
        leaves.extend_from_slice(leaf);
        if leaves.len() < COPY_CHUNK {
            self.leaves = Some(leaves);
            return Ok(());
        }
        let at = self.leaves_at;
        self.leaves_at += leaves.len() as u64;
        self.put(at, leaves)
    }

    fn flush(&mut self) -> Result<(), ImageError> {
        match self.leaves.take() {
            Some(leaves) => {
                let at = self.leaves_at;
                self.leaves_at += leaves.len() as u64;
                self.put(at, leaves)
            }
            None => Ok(()),
        }
    }
}

/// The runs of `bytes` that are not all zero blocks.
pub fn runs(bytes: &[u8]) -> impl Iterator<Item = core::ops::Range<usize>> + '_ {
    let blocks = bytes.len() / BLOCK;
    let zero = move |k: usize| bytes[k * BLOCK..(k + 1) * BLOCK].iter().all(|&b| b == 0);
    let mut k = 0;
    core::iter::from_fn(move || {
        while k < blocks && zero(k) {
            k += 1;
        }
        if k == blocks {
            return None;
        }
        let first = k;
        while k < blocks && !zero(k) {
            k += 1;
        }
        Some(first * BLOCK..k * BLOCK)
    })
}

/// A share no thread could be started for is hashed here.
#[cfg(any(test, feature = "std"))]
fn hash_blocks(compress: Compress, data: &[u8], out: &mut [Digest], threads: usize) {
    let each = |data: &[u8], out: &mut [Digest]| hash_each(compress, data, out);
    let share = out.len().div_ceil(threads.max(1)).max(SHARE_MIN);
    if share >= out.len() {
        return each(data, out);
    }
    let refused = std::thread::scope(|scope| {
        let mut parts = data.chunks(share * BLOCK).zip(out.chunks_mut(share));
        let mine = parts.next();
        let mut refused = Vec::new();
        for (k, (data, out)) in parts.enumerate() {
            let spawned = std::thread::Builder::new()
                .stack_size(HASHER_STACK)
                .spawn_scoped(scope, || each(data, out));
            if spawned.is_err() {
                refused.push(k + 1);
            }
        }
        if let Some((data, out)) = mine {
            each(data, out);
        }
        refused
    });
    for k in refused {
        let out = out.chunks_mut(share).nth(k).unwrap_or_default();
        each(data.chunks(share * BLOCK).nth(k).unwrap_or_default(), out);
    }
}

/// An image's data as its tree vouches for it.
pub struct Verified<S> {
    source: S,
    geometry: Geometry,
    root: Digest,
    /// As many top levels as fit [`UPPER_MAX`], as the kernel's device keeps.
    upper: Vec<u8>,
    /// Tree blocks verified, the most recent last.
    kept: Vec<(u64, Box<[u8; BLOCK]>)>,
    scratch: Box<[u8; BLOCK]>,
    /// A volume walk moves between inode tables and directories, and small
    /// files lie side by side.
    windows: [Window; WINDOWS],
    digests: Vec<Digest>,
}

const WINDOWS: usize = 2;

/// Each block checked the first time it is handed out.
#[derive(Default)]
struct Window {
    bytes: Vec<u8>,
    first: u64,
    checked: Vec<bool>,
}

impl Window {
    fn holds(&self, first: u64, blocks: usize) -> bool {
        first >= self.first && first + blocks as u64 <= self.first + self.checked.len() as u64
    }
}

fn kept_levels(geometry: &Geometry) -> Option<usize> {
    let mut lowest = None;
    let mut blocks = 0;
    for level in (1..geometry.levels()).rev() {
        blocks += geometry.level(level)?.1;
        if blocks > UPPER_MAX {
            break;
        }
        lowest = Some(level);
    }
    lowest
}

impl<S: Source> Verified<S> {
    fn new(mut source: S, geometry: Geometry, root: Digest) -> Result<Self, ImageError> {
        let upper = match kept_levels(&geometry) {
            Some(lowest) => {
                let upper = tree_of(&mut source, &geometry, lowest)?;
                verity::verify_tree(&geometry, &root, source.compress(), &upper, lowest)
                    .map_err(untrue)?;
                upper
            }
            None => Vec::new(),
        };
        Ok(Self {
            source,
            geometry,
            root,
            upper,
            kept: Vec::new(),
            scratch: Box::new([0; BLOCK]),
            windows: Default::default(),
            digests: Vec::new(),
        })
    }

    fn check(&mut self, first: u64, data: &[u8]) -> Result<(), String> {
        let (geometry, root) = (self.geometry, self.root);
        let mut tree = self.tree();
        let checked = verity::check(&geometry, &root, &mut tree.0, first, data, tree.1);
        refused(checked, first)
    }

    fn check_digests(&mut self, first: u64, digests: &[Digest]) -> Result<(), String> {
        let (geometry, root) = (self.geometry, self.root);
        let mut tree = self.tree();
        let checked = verity::check_digests(&geometry, &root, &mut tree.0, first, digests, tree.1);
        refused(checked, first)
    }

    fn tree(&mut self) -> (Tree<'_, S>, &mut [u8; BLOCK]) {
        let tree = Tree {
            source: &mut self.source,
            top: self.geometry.top(),
            upper: &self.upper,
            kept: &mut self.kept,
        };
        (tree, &mut self.scratch)
    }

    fn read_direct(&mut self, first: u64, out: &mut [u8]) -> Result<(), String> {
        self.source
            .read_at(first * BLOCK as u64, out)
            .map_err(|e| e.to_string())?;
        let blocks = out.len() / BLOCK;
        let mut digests = core::mem::take(&mut self.digests);
        digests.resize(blocks, [0; DIGEST]);
        let (compress, cores) = (self.source.compress(), self.source.cores());
        digests_of(compress, cores, out, &mut digests);
        let checked = self.check_digests(first, &digests);
        self.digests = digests;
        checked
    }

    /// Replaces the least recently used window.
    fn fill_window(&mut self, first: u64) -> Result<(), String> {
        self.windows.rotate_left(1);
        let blocks = (self.geometry.data_blocks() - first).min((CHUNK / BLOCK) as u64) as usize;
        let window = &mut self.windows[WINDOWS - 1];
        window.checked.clear();
        window.bytes.clear();
        window.bytes.resize(blocks * BLOCK, 0);
        if let Err(e) = self.source.read_at(first * BLOCK as u64, &mut window.bytes) {
            window.bytes.clear();
            return Err(e.to_string());
        }
        window.first = first;
        window.checked.resize(blocks, false);
        Ok(())
    }

    fn check_window(&mut self, from: usize, to: usize) -> Result<(), String> {
        let mut window = core::mem::take(&mut self.windows[WINDOWS - 1]);
        let mut k = from;
        let mut checked = Ok(());
        while k < to && checked.is_ok() {
            if window.checked[k] {
                k += 1;
                continue;
            }
            let end = (k..to).find(|&j| window.checked[j]).unwrap_or(to);
            checked = self.check(
                window.first + k as u64,
                &window.bytes[k * BLOCK..end * BLOCK],
            );
            if checked.is_ok() {
                window.checked[k..end].fill(true);
            }
            k = end;
        }
        self.windows[WINDOWS - 1] = window;
        checked
    }
}

fn refused<E: fmt::Display>(checked: Result<(), Fault<E>>, first: u64) -> Result<(), String> {
    checked.map_err(|fault| match fault {
        Fault::Read(e) => e.to_string(),
        Fault::Mismatch(block) => {
            format!("block {block} does not hold what the image's tree says")
        }
        Fault::OutOfRange => format!("block {first} is past the image's data"),
    })
}

struct Tree<'a, S> {
    source: &'a mut S,
    top: u64,
    upper: &'a [u8],
    kept: &'a mut Vec<(u64, Box<[u8; BLOCK]>)>,
}

impl<S: Source> verity::Source for Tree<'_, S> {
    type Error = S::Error;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), S::Error> {
        self.source.read_at(block * BLOCK as u64, out)
    }

    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool {
        let from = block.wrapping_sub(self.top) as usize;
        if let Some(held) = from
            .checked_mul(BLOCK)
            .and_then(|at| self.upper.get(at..at + BLOCK))
        {
            out.copy_from_slice(held);
            return true;
        }
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

    fn compress(&self) -> Compress {
        self.source.compress()
    }
}

impl<S: Source> Blocks for Verified<S> {
    type Error = String;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), String> {
        self.read_run(block, &mut out[..])
    }

    fn read_run(&mut self, first: u64, out: &mut [u8]) -> Result<(), String> {
        let blocks = out.len() / BLOCK;
        let end = first.checked_add(blocks as u64);
        if !end.is_some_and(|end| end <= self.geometry.data_blocks()) {
            return Err(format!("block {first} is past the image's data"));
        }
        if out.len() >= CHUNK {
            return self.read_direct(first, out);
        }
        match self.windows.iter().position(|w| w.holds(first, blocks)) {
            Some(at) => self.windows[at..].rotate_left(1),
            None => self.fill_window(first)?,
        }
        let window = &self.windows[WINDOWS - 1];
        let from = (first - window.first) as usize;
        self.check_window(from, from + blocks)?;
        let window = &self.windows[WINDOWS - 1];
        out.copy_from_slice(&window.bytes[from * BLOCK..(from + blocks) * BLOCK]);
        Ok(())
    }
}

/// `len` bytes at image byte `from` are file bytes `at..`.
struct Piece {
    from: u64,
    len: u64,
    file: usize,
    at: u64,
}

static ZEROS: [u8; BLOCK] = [0; BLOCK];

/// A file hashed as the copy passes it; bytes out of order force a re-read.
struct Passing {
    path: String,
    ino: u32,
    want: Digest,
    size: u64,
    hash: Sha256,
    fed: u64,
    /// `Err` past [`symbols::ELF_MAX`].
    elf: Option<Result<Vec<u8>, ()>>,
    in_order: bool,
    left: usize,
    done: Option<Option<Digest>>,
}

impl Passing {
    fn new(path: String, ino: u32, want: Digest, size: u64, compress: Compress) -> Self {
        Self {
            path,
            ino,
            want,
            size,
            hash: Sha256::using(compress),
            fed: 0,
            elf: None,
            in_order: true,
            left: 0,
            done: None,
        }
    }

    fn feed(&mut self, at: u64, bytes: &[u8]) {
        if !self.in_order || at >= self.size {
            return;
        }
        if at < self.fed {
            self.in_order = false;
            self.elf = None;
            return;
        }
        self.zeros(at);
        let n = (self.size - at).min(bytes.len() as u64) as usize;
        self.take(&bytes[..n]);
    }

    fn zeros(&mut self, to: u64) {
        while self.fed < to {
            let n = (to - self.fed).min(BLOCK as u64) as usize;
            self.take(&ZEROS[..n]);
        }
    }

    fn take(&mut self, bytes: &[u8]) {
        if self.fed == 0 && crate::elf::is_elf(bytes) {
            self.elf = Some(if self.size > symbols::ELF_MAX {
                Err(())
            } else {
                Ok(Vec::with_capacity(self.size as usize))
            });
        }
        if let Some(Ok(elf)) = &mut self.elf {
            elf.extend_from_slice(bytes);
        }
        self.hash.update(bytes);
        self.fed += bytes.len() as u64;
    }

    /// `None` when its bytes came out of order.
    fn finish(&mut self, gathered: &mut symbols::Gathered) -> Option<Digest> {
        if let Some(done) = self.done {
            return done;
        }
        let done = self.in_order.then(|| {
            self.zeros(self.size);
            match self.elf.take() {
                Some(Ok(elf)) => symbols::gather(gathered, &self.path, &elf),
                Some(Err(())) => {
                    *gathered = Err(symbols::SymbolError::Read(format!(
                        "{}: an ELF file larger than {} bytes",
                        self.path,
                        symbols::ELF_MAX
                    )))
                }
                None => {}
            }
            core::mem::replace(&mut self.hash, Sha256::new()).finish()
        });
        self.done = Some(done);
        done
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
    /// What [`Self::check_contents`] read of its ELF files, until the symbol
    /// check takes it.
    gathered: Option<symbols::Gathered>,
    contents: Option<Result<(), ImageError>>,
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
        let verified = Verified::new(source, geometry, root)?;
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
            gathered: None,
            contents: None,
        };
        image.delivers()?;
        Ok(image)
    }

    /// Hold every file and symlink to the manifest, or answer what
    /// [`Self::copy`] found. Inode order is the writer's layout.
    pub fn check_contents(&mut self) -> Result<(), ImageError> {
        if let Some(found) = &self.contents {
            return found.clone();
        }
        let mut gathered = Ok(Vec::new());
        for (ino, path, want) in self.files_named()? {
            if self.hash(&path, ino, &mut gathered)? != want {
                return Err(ImageError::Sealed(path));
            }
        }
        self.settle(gathered, Ok(()))
    }

    /// In inode order, once every other entry is held to its line.
    fn files_named(&self) -> Result<Vec<(u32, String, Digest)>, ImageError> {
        let text = core::str::from_utf8(&self.manifest).unwrap_or_default();
        let mut files = Vec::new();
        for line in manifest_lines(text).flatten() {
            let digest = match self.entries.get(line.path).map(|f| (f.kind, f.ino)) {
                Some((Kind::File, ino)) => {
                    files.push((ino, line.path.to_string(), line.digest));
                    continue;
                }
                Some((Kind::Symlink, _)) => sha256(self.links[line.path].as_bytes()),
                _ => sha256(b""),
            };
            if digest != line.digest {
                return Err(ImageError::Sealed(line.path.into()));
            }
        }
        files.sort_unstable_by_key(|&(ino, _, _)| ino);
        Ok(files)
    }

    fn settle(
        &mut self,
        mut gathered: symbols::Gathered,
        found: Result<(), ImageError>,
    ) -> Result<(), ImageError> {
        if let Ok(read) = &mut gathered {
            read.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        }
        self.gathered = Some(gathered);
        self.contents = Some(found.clone());
        found
    }

    /// One read: each file is hashed as its bytes pass and held to the
    /// manifest once the root holds; a file out of order is read again.
    pub fn copy(&mut self, sink: &mut (dyn Sink + Send)) -> Result<(), ImageError>
    where
        S: Send,
    {
        let files = match self.files_named() {
            Ok(files) => files,
            Err(e) => {
                self.contents = Some(Err(e));
                Vec::new()
            }
        };
        let compress = self.volume.blocks().source.compress();
        let mut passing = Vec::with_capacity(files.len());
        let mut pieces = Vec::new();
        for (k, (ino, path, want)) in files.into_iter().enumerate() {
            let inode = self.volume.inode(ino)?;
            let mut file = Passing::new(path, ino, want, inode.size, compress);
            for (lblk, pblk, len) in self.volume.runs(&inode)? {
                pieces.push(Piece {
                    from: pblk * BLOCK as u64,
                    len: u64::from(len) * BLOCK as u64,
                    file: k,
                    at: u64::from(lblk) * BLOCK as u64,
                });
                file.left += 1;
            }
            passing.push(file);
        }
        pieces.sort_unstable_by_key(|p| p.from);
        let mut gathered = Ok(Vec::new());
        let mut next = 0;
        // Finished as its last piece passes, so an ELF file's bytes go then.
        let mut seen = |at: u64, chunk: &[u8]| {
            let end = at + chunk.len() as u64;
            let mut k = next;
            while let Some(piece) = pieces.get(k).filter(|p| p.from < end) {
                let from = piece.from.max(at);
                let to = (piece.from + piece.len).min(end);
                let file = &mut passing[piece.file];
                if from < to {
                    let bytes = &chunk[(from - at) as usize..(to - at) as usize];
                    file.feed(piece.at + (from - piece.from), bytes);
                    if to == piece.from + piece.len {
                        file.left -= 1;
                        if file.left == 0 {
                            file.finish(&mut gathered);
                        }
                    }
                }
                k += 1;
            }
            while pieces.get(next).is_some_and(|p| p.from + p.len <= end) {
                next += 1;
            }
        };
        let len = self.bytes();
        if self.volume.blocks_mut().source.len().map_err(damaged)? != len {
            return Err(ImageError::Damaged("it changed since it was opened".into()));
        }
        let digest = self.digest;
        let source = &mut self.volume.blocks_mut().source;
        copy(source, len, &digest, sink, &mut seen)?;
        if self.contents.is_some() {
            return Ok(());
        }
        let mut found = Ok(());
        for mut file in passing {
            let digest = match file.finish(&mut gathered) {
                Some(digest) => Ok(digest),
                None => self.hash(&file.path, file.ino, &mut gathered),
            };
            match digest {
                Ok(digest) if digest == file.want => {}
                Ok(_) => found = Err(ImageError::Sealed(file.path)),
                Err(e) => found = Err(e),
            }
            if found.is_err() {
                break;
            }
        }
        let _ = self.settle(gathered, found);
        Ok(())
    }

    /// The digest of the file at `path`, read in runs; an ELF file is read
    /// whole and added to `gathered`.
    fn hash(
        &mut self,
        path: &str,
        ino: u32,
        gathered: &mut symbols::Gathered,
    ) -> Result<Digest, ImageError> {
        let inode = self.volume.inode(ino)?;
        let mut hash = Sha256::using(self.volume.blocks().source.compress());
        let first = inode.size.min(CHUNK as u64) as usize;
        let mut chunk = vec![0; first];
        self.volume.read(&inode, 0, &mut chunk)?;
        if crate::elf::is_elf(&chunk) {
            if inode.size > symbols::ELF_MAX {
                *gathered = Err(symbols::SymbolError::Read(format!(
                    "{path}: an ELF file larger than {} bytes",
                    symbols::ELF_MAX
                )));
            } else {
                chunk.resize(inode.size as usize, 0);
                self.volume
                    .read(&inode, first as u64, &mut chunk[first..])?;
                hash.update(&chunk);
                symbols::gather(gathered, path, &chunk);
                return Ok(hash.finish());
            }
        }
        hash.update(&chunk);
        chunk.resize(CHUNK, 0);
        let mut at = first as u64;
        while at < inode.size {
            let n = self.volume.read(&inode, at, &mut chunk)?;
            hash.update(&chunk[..n]);
            at += n as u64;
        }
        Ok(hash.finish())
    }

    /// What [`Self::check_contents`] read of its ELF files, once.
    pub(crate) fn take_gathered(&mut self) -> Option<symbols::Gathered> {
        self.gathered.take()
    }

    /// How long it is: its volume and its tree.
    pub fn bytes(&self) -> u64 {
        self.volume.blocks().geometry.image_bytes()
    }

    /// Where its bytes come from.
    pub fn source(&self) -> &S {
        &self.volume.blocks().source
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

    /// What `path` reaches once each symlink on the way is followed as the
    /// kernel follows it, through at most [`LINK_HOPS`] of them. A target
    /// never climbs (`inside_image`), so a link's target continues from the
    /// directory holding it.
    fn resolved(&self, path: &str) -> Option<&Found> {
        let mut left: Vec<String> = path.split('/').rev().map(String::from).collect();
        let mut at = String::new();
        let mut hops = 0;
        while let Some(part) = left.pop() {
            if part.is_empty() || part == "." {
                continue;
            }
            let next = format!("{at}/{part}");
            if let Some(target) = self.links.get(&next) {
                hops += 1;
                if hops >= LINK_HOPS {
                    return None;
                }
                left.extend(target.split('/').rev().map(String::from));
                continue;
            }
            self.entries.get(&next)?;
            at = next;
        }
        self.entries.get(&at)
    }

    fn delivers(&self) -> Result<(), ImageError> {
        let undelivered = |what: &str| ImageError::Undelivered(what.to_string());
        let declaration = &self.declaration;
        let executable = |e: &Found| e.kind == Kind::File && e.mode & 0o111 != 0;
        for program in declaration.programs(&self.release.name) {
            if !self
                .inside(&program)
                .and_then(|path| self.entry(path))
                .is_some_and(executable)
            {
                return Err(undelivered(&program));
            }
        }
        for command in &declaration.commands {
            if !self
                .resolved(&format!("/bin/{command}"))
                .is_some_and(executable)
            {
                let program = format!("{}/bin/{command}", mount_point(&self.release.name));
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
