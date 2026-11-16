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
    hash_using(sha256::compress_blocks, block)
}

/// [`hash`] through `compress`.
pub fn hash_using(compress: sha256::Compress, block: &[u8; BLOCK]) -> Digest {
    if block.iter().all(|&b| b == 0) {
        ZERO
    } else {
        sha256::digest_using(compress, block)
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

    /// How many levels the tree has, 0 the leaves; none over one data block.
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// The image block level `level` starts at, and how many blocks it has.
    pub fn level(&self, level: usize) -> Option<(u64, u64)> {
        (level < self.levels).then(|| (self.start[level], blocks_at(self.data_blocks, level)))
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

    /// What its blocks are hashed with.
    fn compress(&self) -> sha256::Compress {
        sha256::compress_blocks
    }
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
    check(geometry, root, source, index, out, scratch)
}

/// Data blocks `first..`, already read whole into `data`, held to the tree;
/// the leaf over each 128 of them is looked up once. `scratch` is a block of
/// room the walk uses.
pub fn check<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    first: u64,
    data: &[u8],
    scratch: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    if !data.len().is_multiple_of(BLOCK) {
        return Err(Fault::OutOfRange);
    }
    let compress = source.compress();
    let digest = |k: usize| match data[k * BLOCK..(k + 1) * BLOCK].try_into() {
        Ok(block) => hash_using(compress, block),
        Err(_) => [0; DIGEST],
    };
    held(
        geometry,
        root,
        source,
        first,
        data.len() / BLOCK,
        digest,
        scratch,
    )
}

/// [`check`] of the data blocks `first..` whose digests, hashed already,
/// are `digests`.
pub fn check_digests<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    first: u64,
    digests: &[Digest],
    scratch: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    held(
        geometry,
        root,
        source,
        first,
        digests.len(),
        |k| digests[k],
        scratch,
    )
}

fn held<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    first: u64,
    count: usize,
    digest: impl Fn(usize) -> Digest,
    scratch: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    let in_range = first
        .checked_add(count as u64)
        .is_some_and(|end| end <= geometry.data_blocks);
    if !in_range {
        return Err(Fault::OutOfRange);
    }
    let mut leaf = None;
    for (k, index) in (first..first + count as u64).enumerate() {
        let want = match geometry.digest_at(0, index) {
            None => *root,
            Some((at_leaf, at)) => {
                if leaf != Some(at_leaf) {
                    expected(geometry, root, source, 0, index, scratch)?;
                    leaf = Some(at_leaf);
                }
                let mut digest = [0; DIGEST];
                digest.copy_from_slice(&scratch[at..at + DIGEST]);
                digest
            }
        };
        if digest(k) != want {
            return Err(Fault::Mismatch(index));
        }
    }
    Ok(())
}

/// Block `index` of tree level `level`, 0 the leaves, into `out` once it and
/// every tree block above it check out.
pub fn tree_block<S: Source>(
    geometry: &Geometry,
    root: &Digest,
    source: &mut S,
    level: usize,
    index: u64,
    out: &mut [u8; BLOCK],
) -> Result<(), Fault<S::Error>> {
    if level >= geometry.levels || index >= blocks_at(geometry.data_blocks, level) {
        return Err(Fault::OutOfRange);
    }
    expected(geometry, root, source, level, index << FANOUT_BITS, out).map(|_| ())
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
        if hash_using(source.compress(), scratch) != want {
            return Err(Fault::Mismatch(block));
        }
        source.keep(block, scratch);
    }
    let mut digest = [0; DIGEST];
    digest.copy_from_slice(&scratch[at..at + DIGEST]);
    Ok(digest)
}

/// Hold `tree`, the image's blocks from its [top](Geometry::top) to the end
/// of level `lowest`, to `root`: the top hashes to it, and each block below
/// to the digest the level above holds of it, checked from the top down.
/// With no levels `tree` is the one data block. A tree held to the root
/// whole, `lowest` 0, is the one its data makes once each data block hashes
/// to its leaf's digest.
pub fn verify_tree(
    geometry: &Geometry,
    root: &Digest,
    compress: sha256::Compress,
    tree: &[u8],
    lowest: usize,
) -> Result<(), Fault<()>> {
    let top = geometry.top();
    let end = match geometry.level(lowest) {
        Some((start, count)) => start + count,
        None if geometry.levels == 0 => 1,
        None => return Err(Fault::OutOfRange),
    };
    if tree.len() as u64 != (end - top) * BLOCK as u64 {
        return Err(Fault::OutOfRange);
    }
    let block = |at: u64| -> Result<&[u8; BLOCK], Fault<()>> {
        let from = (at - top) as usize * BLOCK;
        tree[from..from + BLOCK]
            .try_into()
            .map_err(|_| Fault::OutOfRange)
    };
    if hash_using(compress, block(top)?) != *root {
        return Err(Fault::Mismatch(top));
    }
    for level in (lowest..geometry.levels.saturating_sub(1)).rev() {
        let (start, count) = geometry.level(level).ok_or(Fault::OutOfRange)?;
        for index in 0..count {
            let (holder, at) = geometry
                .digest_at(level + 1, index)
                .ok_or(Fault::OutOfRange)?;
            let digest = hash_using(compress, block(start + index)?);
            if block(holder)?[at..at + DIGEST] != digest {
                return Err(Fault::Mismatch(start + index));
            }
        }
    }
    Ok(())
}

/// `upper`, the levels from the top down to level 0, is already held to the
/// root.
pub fn verify_leaves(
    geometry: &Geometry,
    compress: sha256::Compress,
    upper: &[u8],
    first: u64,
    leaves: &[u8],
) -> Result<(), Fault<()>> {
    let (start, count) = geometry.level(0).ok_or(Fault::OutOfRange)?;
    let top = geometry.top();
    let blocks = (leaves.len() / BLOCK) as u64;
    let in_range = first.checked_add(blocks).is_some_and(|end| end <= count);
    if !leaves.len().is_multiple_of(BLOCK)
        || !in_range
        || upper.len() as u64 != (start - top) * BLOCK as u64
    {
        return Err(Fault::OutOfRange);
    }
    for (index, leaf) in (first..).zip(leaves.chunks_exact(BLOCK)) {
        let leaf: &[u8; BLOCK] = leaf.try_into().map_err(|_| Fault::OutOfRange)?;
        let (holder, at) = geometry.digest_at(1, index).ok_or(Fault::OutOfRange)?;
        let from = (holder - top) as usize * BLOCK + at;
        if upper[from..from + DIGEST] != hash_using(compress, leaf) {
            return Err(Fault::Mismatch(start + index));
        }
    }
    Ok(())
}

/// Keeps only the levels above the leaves, each leaf emitted as it fills,
/// so its room is about 1/16 000 of the data.
pub struct Streamed<'t> {
    geometry: Geometry,
    compress: sha256::Compress,
    upper: &'t mut [u8],
    leaf: &'t mut [u8; BLOCK],
    in_leaf: usize,
    leaves: u64,
    pushed: u64,
    only: Digest,
}

impl<'t> Streamed<'t> {
    /// `upper` spans the blocks from `geometry`'s top to its level 0.
    pub fn new(
        geometry: Geometry,
        compress: sha256::Compress,
        upper: &'t mut [u8],
        leaf: &'t mut [u8; BLOCK],
    ) -> Option<Self> {
        let leaves_at = geometry.level(0).map_or(geometry.top(), |(start, _)| start);
        if upper.len() as u64 != (leaves_at - geometry.top()) * BLOCK as u64 {
            return None;
        }
        upper.fill(0);
        leaf.fill(0);
        Some(Self {
            geometry,
            compress,
            upper,
            leaf,
            in_leaf: 0,
            leaves: 0,
            pushed: 0,
            only: ZERO,
        })
    }

    pub fn push_digest<E>(
        &mut self,
        digest: Digest,
        emit: &mut impl FnMut(u64, &[u8; BLOCK]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.pushed += 1;
        if self.geometry.levels == 0 {
            self.only = digest;
            return Ok(());
        }
        self.leaf[self.in_leaf * DIGEST..][..DIGEST].copy_from_slice(&digest);
        self.in_leaf += 1;
        if self.in_leaf as u64 == FANOUT {
            self.emit_leaf(emit)?;
        }
        Ok(())
    }

    fn emit_leaf<E>(
        &mut self,
        emit: &mut impl FnMut(u64, &[u8; BLOCK]) -> Result<(), E>,
    ) -> Result<(), E> {
        let index = self.leaves;
        emit(self.geometry.start[0] + index, self.leaf)?;
        let digest = hash_using(self.compress, self.leaf);
        self.place(1, index, digest);
        self.leaves += 1;
        self.in_leaf = 0;
        self.leaf.fill(0);
        Ok(())
    }

    fn place(&mut self, layer: usize, index: u64, digest: Digest) {
        match self.geometry.digest_at(layer, index) {
            Some((block, at)) => {
                let offset = (block - self.geometry.top()) as usize * BLOCK + at;
                if let Some(slot) = self.upper.get_mut(offset..offset + DIGEST) {
                    slot.copy_from_slice(&digest);
                }
            }
            None => self.only = digest,
        }
    }

    /// `None` until every data block is pushed.
    pub fn finish<E>(
        mut self,
        emit: &mut impl FnMut(u64, &[u8; BLOCK]) -> Result<(), E>,
    ) -> Result<Option<Digest>, E> {
        if self.pushed != self.geometry.data_blocks {
            return Ok(None);
        }
        if self.in_leaf > 0 {
            self.emit_leaf(emit)?;
        }
        for level in 1..self.geometry.levels {
            let first = (self.geometry.start[level] - self.geometry.top()) as usize;
            for index in 0..blocks_at(self.geometry.data_blocks, level) {
                let at = (first + index as usize) * BLOCK;
                let Ok(block) = self.upper[at..at + BLOCK].try_into() else {
                    return Ok(None);
                };
                let digest = hash_using(self.compress, block);
                self.place(level + 1, index, digest);
            }
        }
        Ok(Some(self.only))
    }
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
