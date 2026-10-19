//! dm-verity's hash tree as data: an image is its data blocks followed by a
//! tree of SHA-256 digests, in the layout `veritysetup format` writes with
//! `--no-superblock --salt=-` and 4 KiB data and hash blocks, and the digest
//! of the tree's top block is the image's root. Free of `alloc`, `unsafe` and
//! I/O, so the kernel, which reads an extension's image verified block by
//! block, and the tools that build and check one share it.
//!
//! The levels lie after the data, the top first; level `i` holds the
//! digests of the blocks of the level below, 128 to a block, the rest of a
//! block zero. With one data block there are no levels, and the root is that
//! block's digest. The layout follows the kernel's dm-verity documentation,
//! format version 1 with an empty salt, taken as interface facts.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod sha256;

pub const BLOCK: usize = 4096;
pub const DIGEST: usize = sha256::OUTPUT;

pub type Digest = [u8; DIGEST];

const FANOUT_BITS: u32 = 7;
const FANOUT: u64 = 1 << FANOUT_BITS;

/// Data blocks one tree covers at most: the kernel addresses a volume in
/// 32-bit blocks.
pub const MAX_DATA_BLOCKS: u64 = 1 << 32;

const MAX_LEVELS: usize = 32_u32.div_ceil(FANOUT_BITS) as usize;

/// The digest of a block of zeros, which an image of mostly free space
/// holds most of.
const ZERO: Digest = [
    0xad, 0x7f, 0xac, 0xb2, 0x58, 0x6f, 0xc6, 0xe9, 0x66, 0xc0, 0x04, 0xd7, 0xd1, 0xd1, 0x6b, 0x02,
    0x4f, 0x58, 0x05, 0xff, 0x7c, 0xb4, 0x7c, 0x7a, 0x85, 0xda, 0xbd, 0x8b, 0x48, 0x89, 0x2c, 0xa7,
];

/// A block's digest.
pub fn hash(block: &[u8; BLOCK]) -> Digest {
    if block.iter().all(|&b| b == 0) {
        ZERO
    } else {
        sha256::digest(block)
    }
}

/// Where an image's levels lie, from how many data blocks it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    data_blocks: u64,
    levels: usize,
    /// Each level's first block, counted from the image's start.
    start: [u64; MAX_LEVELS],
    tree_blocks: u64,
}

const fn blocks_at(data_blocks: u64, level: usize) -> u64 {
    data_blocks.div_ceil(1 << (FANOUT_BITS as u64 * (level as u64 + 1)))
}

impl Geometry {
    pub const fn new(data_blocks: u64) -> Option<Self> {
        if data_blocks == 0 || data_blocks > MAX_DATA_BLOCKS {
            return None;
        }
        let mut levels = 0;
        while (data_blocks - 1) >> (FANOUT_BITS as usize * levels) != 0 {
            levels += 1;
        }
        let mut start = [0; MAX_LEVELS];
        let mut at = data_blocks;
        let mut level = levels;
        while level > 0 {
            level -= 1;
            start[level] = at;
            at += blocks_at(data_blocks, level);
        }
        Some(Self {
            data_blocks,
            levels,
            start,
            tree_blocks: at - data_blocks,
        })
    }

    /// The geometry of an image `bytes` long: the one count of data blocks
    /// whose tree fills the rest of it.
    pub fn of_image(bytes: u64) -> Option<Self> {
        if !bytes.is_multiple_of(BLOCK as u64) {
            return None;
        }
        let total = bytes / BLOCK as u64;
        let (mut lo, mut hi) = (1, total.min(MAX_DATA_BLOCKS));
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let geometry = Self::new(mid)?;
            match geometry.image_blocks().cmp(&total) {
                core::cmp::Ordering::Equal => return Some(geometry),
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid - 1,
            }
        }
        None
    }

    pub fn data_blocks(&self) -> u64 {
        self.data_blocks
    }

    pub fn tree_blocks(&self) -> u64 {
        self.tree_blocks
    }

    pub fn image_blocks(&self) -> u64 {
        self.data_blocks + self.tree_blocks
    }

    pub fn image_bytes(&self) -> u64 {
        self.image_blocks() * BLOCK as u64
    }

    /// The image block holding the digest of block `index` of `layer` — the
    /// data when 0, level `layer - 1` of the tree otherwise — and the byte
    /// it starts at; `None` for the top's one block, whose digest is the root.
    pub fn digest_at(&self, layer: usize, index: u64) -> Option<(u64, usize)> {
        (layer < self.levels).then(|| {
            (
                self.start[layer] + (index >> FANOUT_BITS),
                (index & (FANOUT - 1)) as usize * DIGEST,
            )
        })
    }

    /// The image block holding the top of the tree, whose digest is the
    /// root: the one data block when there are no levels.
    pub fn top(&self) -> u64 {
        match self.levels {
            0 => 0,
            levels => self.start[levels - 1],
        }
    }
}

/// Where the blocks of an image come from, and which of its tree's blocks
/// were verified already.
pub trait Source {
    type Error;

    /// Image block `block`, read into `out`.
    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), Self::Error>;

    /// The tree block `block`, copied into `out` when it was verified before.
    fn verified(&mut self, block: u64, out: &mut [u8; BLOCK]) -> bool;

    /// Keep tree block `block`, just verified.
    fn keep(&mut self, block: u64, verified: &[u8; BLOCK]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault<E> {
    Read(E),
    /// The image block named does not hash to what the tree says.
    Mismatch(u64),
    /// Past the image's data.
    OutOfRange,
}

/// Data block `index`, read into `out` once it and every tree block on the
/// way to `root` check out; `scratch` is a block of room the walk uses.
pub fn read<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    index: u64,
    out: &mut [u8; BLOCK],
    scratch: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    if index >= geometry.data_blocks {
        return Err(Fault::OutOfRange);
    }
    source.read(index, out).map_err(Fault::Read)?;
    if hash(out) != expected(geometry, root, source, 0, index, scratch)? {
        return Err(Fault::Mismatch(index));
    }
    Ok(())
}

/// What block `index` of `layer` must hash to.
fn expected<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    layer: usize,
    index: u64,
    scratch: &mut [u8; BLOCK],
) -> Result<Digest, Fault<S::Error>> {
    let Some((block, at)) = geometry.digest_at(layer, index) else {
        return Ok(*root);
    };
    if !source.verified(block, scratch) {
        let want = expected(
            geometry,
            root,
            source,
            layer + 1,
            index >> FANOUT_BITS,
            scratch,
        )?;
        source.read(block, scratch).map_err(Fault::Read)?;
        if hash(scratch) != want {
            return Err(Fault::Mismatch(block));
        }
        source.keep(block, scratch);
    }
    let mut digest = [0; DIGEST];
    digest.copy_from_slice(&scratch[at..at + DIGEST]);
    Ok(digest)
}

/// Hold every block of the image `source` holds to `root`, a level at a
/// time from the data up: each group of 128 blocks of a level hashes to the
/// block of the level above that holds their digests, and the top hashes to
/// the root. `read` and `built` are the room it takes, whatever the image's
/// size. A tree block is read twice, so `source` must not change meanwhile.
pub fn verify_all<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    read: &mut [u8; BLOCK],
    built: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    for layer in 0..geometry.levels {
        let (first, count) = match layer {
            0 => (0, geometry.data_blocks),
            _ => (
                geometry.start[layer - 1],
                blocks_at(geometry.data_blocks, layer - 1),
            ),
        };
        let mut index = 0;
        while index < count {
            built.fill(0);
            let group = (count - index).min(FANOUT);
            for k in 0..group {
                source.read(first + index + k, read).map_err(Fault::Read)?;
                built[k as usize * DIGEST..][..DIGEST].copy_from_slice(&hash(read));
            }
            let (stored, _) = geometry.digest_at(layer, index).ok_or(Fault::OutOfRange)?;
            source.read(stored, read).map_err(Fault::Read)?;
            if read != built {
                return Err(Fault::Mismatch(stored));
            }
            index += group;
        }
    }
    source.read(geometry.top(), read).map_err(Fault::Read)?;
    if hash(read) != *root {
        return Err(Fault::Mismatch(geometry.top()));
    }
    Ok(())
}

/// A tree built from an image's data blocks, pushed in order.
pub struct Builder<'t> {
    geometry: Geometry,
    /// The tree's blocks, as they follow the data in the image.
    tree: &'t mut [u8],
    pushed: u64,
    only: Digest,
}

impl<'t> Builder<'t> {
    /// A builder writing into `tree`, which is `geometry`'s tree blocks long.
    pub fn new(geometry: Geometry, tree: &'t mut [u8]) -> Option<Self> {
        if tree.len() as u64 != geometry.tree_blocks * BLOCK as u64 {
            return None;
        }
        tree.fill(0);
        Some(Self {
            geometry,
            tree,
            pushed: 0,
            only: ZERO,
        })
    }

    pub fn push(&mut self, block: &[u8; BLOCK]) {
        self.push_digest(hash(block));
    }

    /// The next data block, by its digest.
    pub fn push_digest(&mut self, digest: Digest) {
        let index = self.pushed;
        self.pushed += 1;
        self.place(0, index, digest);
    }

    fn place(&mut self, layer: usize, index: u64, digest: Digest) {
        match self.geometry.digest_at(layer, index) {
            Some((block, at)) => {
                let offset = (block - self.geometry.data_blocks) as usize * BLOCK + at;
                if let Some(slot) = self.tree.get_mut(offset..offset + DIGEST) {
                    slot.copy_from_slice(&digest);
                }
            }
            None => self.only = digest,
        }
    }

    /// The root, once every data block is pushed; `None` before.
    pub fn finish(mut self) -> Option<Digest> {
        if self.pushed != self.geometry.data_blocks {
            return None;
        }
        for level in 0..self.geometry.levels {
            let first = self.geometry.start[level] - self.geometry.data_blocks;
            for index in 0..blocks_at(self.geometry.data_blocks, level) {
                let at = (first + index) as usize * BLOCK;
                let digest = match self.tree[at..at + BLOCK].try_into() {
                    Ok(block) => hash(block),
                    Err(_) => return None,
                };
                self.place(level + 1, index, digest);
            }
        }
        Some(self.only)
    }
}

#[cfg(test)]
mod tests;
