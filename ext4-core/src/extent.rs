//! The extent tree that maps a file's logical blocks under `EXTENTS_FL`.
//!
//! A B+tree whose root is the inode's 60-byte `i_block` (four entries) and
//! whose other nodes are whole blocks. Every node starts with a 12-byte
//! header; an index node's entries point at the nodes one level down, a
//! leaf's at runs of data blocks. Every path has the same depth.
//!
//! The algorithm here reaches nodes only through a [`Store`], which owns
//! reading, writing, checksumming and allocation, so the same code runs over
//! the kernel's block cache and over a test's memory.

use crate::bytes::{le16, le32, put_le16, put_le32};
use crate::crc::crc32c;

pub const MAGIC: u16 = 0xF30A;

/// Logical blocks a tree maps: `0xFFFFFFFF` is the format's "no block", so
/// the last file block is the one before it.
pub const MAX_BLOCKS: u64 = u32::MAX as u64;
pub const HEADER: usize = 12;
pub const ENTRY: usize = 12;
/// Entries the root holds: 60 bytes less the header.
pub const ROOT_MAX: u16 = 4;
/// The deepest tree the format allows.
pub const MAX_DEPTH: u16 = 5;
/// Longest initialised extent; a stored length above it marks the extent
/// unwritten.
pub const MAX_LEN: u32 = 32768;
/// Longest unwritten extent.
pub const MAX_UNWRITTEN_LEN: u32 = 32767;

const LEVELS: usize = MAX_DEPTH as usize + 1;

/// Passes an insert or a conversion may take: each one places the entry or
/// adds room for the next, by a split or a new level, so only a damaged tree
/// runs out.
const MAX_PASSES: usize = 4 * LEVELS;

/// Entries a tree block of `block_size` bytes holds, leaving room for the
/// checksum tail behind them.
pub fn block_capacity(block_size: usize) -> u16 {
    ((block_size - HEADER) / ENTRY) as u16
}

/// Where a block's checksum tail sits: right after the entries its header
/// has room for.
pub fn tail_offset(max: u16) -> usize {
    HEADER + ENTRY * usize::from(max)
}

/// The tail checksum a tree block should carry, seeded with the owning
/// inode's seed.
pub fn checksum(inode_seed: u32, node: &[u8]) -> Option<u32> {
    let end = tail_offset(le16(node, 4));
    (end + 4 <= node.len()).then(|| crc32c(inode_seed, &node[..end]))
}

/// Stamp the tail of a node whose header is in place; a block that carries
/// no header yet has nowhere to put one.
pub fn seal(inode_seed: u32, node: &mut [u8]) {
    if le16(node, 0) != MAGIC {
        return;
    }
    if let Some(sum) = checksum(inode_seed, node) {
        let at = tail_offset(le16(node, 4));
        put_le32(node, at, sum);
    }
}

pub fn verify(inode_seed: u32, node: &[u8]) -> bool {
    checksum(inode_seed, node).is_some_and(|sum| le32(node, tail_offset(le16(node, 4))) == sum)
}

/// An empty tree, as a new file's `i_block` starts.
pub fn init_root(root: &mut [u8]) {
    root[..HEADER + ENTRY * usize::from(ROOT_MAX)].fill(0);
    write_header(
        root,
        Header {
            entries: 0,
            max: ROOT_MAX,
            depth: 0,
        },
    );
}

/// A tree of the one extent `e`, held in the root.
pub fn init_root_with(root: &mut [u8], e: &Extent) {
    init_root(root);
    put_leaf(root, 0, e);
    set_entries(root, 1);
}

/// The tree is malformed, or an operation would take it past what the format
/// can express.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtentError {
    /// A node's header or entries contradict the format or each other.
    Corrupt,
    /// The tree would grow past [`MAX_DEPTH`].
    TooDeep,
    /// An insert overlaps a mapping that already exists.
    Overlap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub entries: u16,
    pub max: u16,
    pub depth: u16,
}

/// Read and check a node's header against the bytes it lives in. A node
/// holds two entries at least, so a split leaves both halves some.
pub fn header(node: &[u8]) -> Result<Header, ExtentError> {
    if node.len() < HEADER || le16(node, 0) != MAGIC {
        return Err(ExtentError::Corrupt);
    }
    let h = Header {
        entries: le16(node, 2),
        max: le16(node, 4),
        depth: le16(node, 6),
    };
    if h.max < 2
        || h.entries > h.max
        || h.depth > MAX_DEPTH
        || HEADER + ENTRY * usize::from(h.max) > node.len()
    {
        return Err(ExtentError::Corrupt);
    }
    Ok(h)
}

/// A node's header, its entries held to the span `lo..hi` its parent gives
/// it: keys strictly increasing, extents non-empty and disjoint, an index
/// node not empty. A node two parents share fails at the second unless it is
/// an empty leaf.
fn checked_header(node: &[u8], lo: u64, hi: u64) -> Result<Header, ExtentError> {
    let h = header(node)?;
    if h.depth > 0 && h.entries == 0 {
        return Err(ExtentError::Corrupt);
    }
    let mut from = lo;
    for i in 0..usize::from(h.entries) {
        let (start, end) = if h.depth == 0 {
            let e = leaf(node, i);
            if e.len == 0 {
                return Err(ExtentError::Corrupt);
            }
            (u64::from(e.lblk), e.end())
        } else {
            let key = u64::from(index(node, i).0);
            (key, key + 1)
        };
        if start < from || end > hi {
            return Err(ExtentError::Corrupt);
        }
        from = end;
    }
    Ok(h)
}

fn root_header<S: Store>(s: &mut S) -> Result<Header, S::Error> {
    Ok(s.read(Node::Root, |n| checked_header(n, 0, MAX_BLOCKS))??)
}

/// The checked header of `child`, one level below `parent`, within `lo..hi`.
fn child_header<S: Store>(
    s: &mut S,
    child: u64,
    parent: Header,
    lo: u64,
    hi: u64,
) -> Result<Header, S::Error> {
    let h = s.read(Node::Block(child), |n| checked_header(n, lo, hi))??;
    if h.depth + 1 != parent.depth {
        return Err(ExtentError::Corrupt.into());
    }
    Ok(h)
}

/// Index entry `i` of `node`: its key, its child, and where the next
/// entry's span starts, `hi` for the last.
fn index_span(node: &[u8], h: Header, i: usize, hi: u64) -> (u64, u64, u64) {
    let (key, child) = index(node, i);
    let next = if i + 1 < usize::from(h.entries) {
        u64::from(index(node, i + 1).0)
    } else {
        hi
    };
    (u64::from(key), child, next)
}

fn write_header(node: &mut [u8], h: Header) {
    put_le16(node, 0, MAGIC);
    put_le16(node, 2, h.entries);
    put_le16(node, 4, h.max);
    put_le16(node, 6, h.depth);
}

fn set_entries(node: &mut [u8], entries: u16) {
    put_le16(node, 2, entries);
}

/// One run of a file's blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub lblk: u32,
    pub len: u32,
    pub pblk: u64,
    /// Allocated but never written: reads as zeros.
    pub unwritten: bool,
}

impl Extent {
    pub fn end(&self) -> u64 {
        u64::from(self.lblk) + u64::from(self.len)
    }

    fn max_len(&self) -> u32 {
        if self.unwritten {
            MAX_UNWRITTEN_LEN
        } else {
            MAX_LEN
        }
    }

    /// Whether `next` continues this extent on the device and may join it.
    fn joins(&self, next: &Extent) -> bool {
        self.end() == u64::from(next.lblk)
            && self.pblk + u64::from(self.len) == next.pblk
            && self.unwritten == next.unwritten
            && self.len + next.len <= self.max_len()
    }
}

fn entry_at(i: usize) -> usize {
    HEADER + ENTRY * i
}

pub fn leaf(node: &[u8], i: usize) -> Extent {
    let at = entry_at(i);
    let raw_len = u32::from(le16(node, at + 4));
    let (len, unwritten) = if raw_len > MAX_LEN {
        (raw_len - MAX_LEN, true)
    } else {
        (raw_len, false)
    };
    Extent {
        lblk: le32(node, at),
        len,
        pblk: (u64::from(le16(node, at + 6)) << 32) | u64::from(le32(node, at + 8)),
        unwritten,
    }
}

fn put_leaf(node: &mut [u8], i: usize, e: &Extent) {
    let at = entry_at(i);
    let raw_len = if e.unwritten { e.len + MAX_LEN } else { e.len };
    put_le32(node, at, e.lblk);
    put_le16(node, at + 4, raw_len as u16);
    put_le16(node, at + 6, (e.pblk >> 32) as u16);
    put_le32(node, at + 8, e.pblk as u32);
}

/// An index entry: the first logical block its subtree covers, and the
/// subtree's node.
pub fn index(node: &[u8], i: usize) -> (u32, u64) {
    let at = entry_at(i);
    (
        le32(node, at),
        u64::from(le32(node, at + 4)) | (u64::from(le16(node, at + 8)) << 32),
    )
}

fn put_index(node: &mut [u8], i: usize, key: u32, child: u64) {
    let at = entry_at(i);
    put_le32(node, at, key);
    put_le32(node, at + 4, child as u32);
    put_le16(node, at + 8, (child >> 32) as u16);
    put_le16(node, at + 10, 0);
}

fn set_key(node: &mut [u8], i: usize, key: u32) {
    put_le32(node, entry_at(i), key);
}

fn open_gap(node: &mut [u8], i: usize, entries: usize) {
    node.copy_within(entry_at(i)..entry_at(entries), entry_at(i + 1));
}

fn close_gap(node: &mut [u8], i: usize, entries: usize) {
    node.copy_within(entry_at(i + 1)..entry_at(entries), entry_at(i));
}

/// A node of the tree: the inode's root, or a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Root,
    Block(u64),
}

/// What the tree algorithm needs from whoever holds the nodes.
pub trait Store {
    type Error: From<ExtentError>;

    fn block_size(&self) -> usize;

    /// Run `f` over a node's bytes: 60 for the root, a block for the rest.
    fn read<R>(&mut self, node: Node, f: impl FnOnce(&[u8]) -> R) -> Result<R, Self::Error>;

    /// Run `f` over a node's bytes for writing. A block's checksum tail is
    /// the store's to update afterwards.
    fn write<R>(&mut self, node: Node, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, Self::Error>;

    /// A fresh tree block near `goal`, every byte zero. The algorithm writes
    /// its header before anything reads it.
    fn alloc_node(&mut self, goal: u64) -> Result<u64, Self::Error>;

    fn free_node(&mut self, block: u64) -> Result<(), Self::Error>;

    /// Release `count` data blocks from `first`, which a truncate cut away.
    fn free_data(&mut self, first: u64, count: u32) -> Result<(), Self::Error>;
}

/// The nodes from the root down to a leaf, and the entry taken at each.
#[derive(Debug, Clone, Copy)]
struct Path {
    levels: usize,
    nodes: [Node; LEVELS],
    /// The index entry followed at each level above the leaf.
    taken: [usize; LEVELS],
    headers: [Header; LEVELS],
    /// The first block past the leaf's subtree: the next index key at the
    /// lowest level that has one.
    bound: u64,
}

impl Path {
    fn leaf(&self) -> usize {
        self.levels - 1
    }
}

/// The last entry whose key is at or below `lblk`, or `None` when every key
/// is above it. Keys are strictly increasing within a node.
fn search(node: &[u8], entries: usize, lblk: u32) -> Option<usize> {
    let (mut lo, mut hi) = (0usize, entries);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if le32(node, entry_at(mid)) <= lblk {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.checked_sub(1)
}

fn descend<S: Store>(s: &mut S, lblk: u32) -> Result<Path, S::Error> {
    let root = root_header(s)?;
    let mut path = Path {
        levels: 0,
        nodes: [Node::Root; LEVELS],
        taken: [0; LEVELS],
        headers: [root; LEVELS],
        bound: MAX_BLOCKS,
    };
    let mut node = Node::Root;
    let mut h = root;
    loop {
        let level = path.levels;
        path.nodes[level] = node;
        path.headers[level] = h;
        path.levels += 1;
        if h.depth == 0 {
            return Ok(path);
        }
        let bound = path.bound;
        let (i, (key, child, next)) = s.read(node, |n| {
            let i = search(n, usize::from(h.entries), lblk).unwrap_or(0);
            (i, index_span(n, h, i, bound))
        })?;
        path.taken[level] = i;
        path.bound = next;
        h = child_header(s, child, h, key, next)?;
        node = Node::Block(child);
    }
}

/// Where `lblk` lives: its block, how many blocks of the same extent follow
/// it, and whether they were ever written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub pblk: u64,
    pub len: u32,
    pub unwritten: bool,
}

pub fn lookup<S: Store>(s: &mut S, lblk: u32) -> Result<Option<Mapping>, S::Error> {
    let path = descend(s, lblk)?;
    let leaf_level = path.leaf();
    let h = path.headers[leaf_level];
    s.read(path.nodes[leaf_level], |n| {
        let e = search(n, usize::from(h.entries), lblk).map(|i| leaf(n, i))?;
        (u64::from(lblk) < e.end()).then(|| {
            let into = lblk - e.lblk;
            Mapping {
                pblk: e.pblk + u64::from(into),
                len: e.len - into,
                unwritten: e.unwritten,
            }
        })
    })
}

/// Where a new block for `lblk` should go: in line with the extent of its
/// leaf at or before it, or else the leaf's first.
pub fn goal<S: Store>(s: &mut S, lblk: u32) -> Result<Option<u64>, S::Error> {
    let path = descend(s, lblk)?;
    let leaf_level = path.leaf();
    let h = path.headers[leaf_level];
    s.read(path.nodes[leaf_level], |n| {
        let found = search(n, usize::from(h.entries), lblk).or((h.entries > 0).then_some(0));
        found.map(|i| {
            let e = leaf(n, i);
            if lblk >= e.lblk {
                e.pblk + u64::from(lblk - e.lblk)
            } else {
                e.pblk.saturating_sub(u64::from(e.lblk - lblk))
            }
        })
    })
}

/// Lower the keys above a leaf whose first entry starts at `key`. Each index
/// key must stay at or below the first block its subtree maps.
fn fix_keys<S: Store>(s: &mut S, path: &Path, key: u32) -> Result<(), S::Error> {
    for level in (0..path.leaf()).rev() {
        let i = path.taken[level];
        let lowered = s.write(path.nodes[level], |n| {
            if le32(n, entry_at(i)) > key {
                set_key(n, i, key);
                true
            } else {
                false
            }
        })?;
        if !lowered || i != 0 {
            break;
        }
    }
    Ok(())
}

enum Placed {
    Done,
    /// The leaf is full; the entry belongs at this position in it.
    Full(usize),
}

fn place_in_leaf<S: Store>(s: &mut S, path: &Path, e: Extent) -> Result<Placed, S::Error> {
    let level = path.leaf();
    let node = path.nodes[level];
    let h = path.headers[level];
    let entries = usize::from(h.entries);
    let (at, prev, next) = s.read(node, |n| {
        let at = search(n, entries, e.lblk).map_or(0, |i| i + 1);
        let prev = at.checked_sub(1).map(|i| leaf(n, i));
        let next = (at < entries).then(|| leaf(n, at));
        (at, prev, next)
    })?;
    if prev.is_some_and(|p| p.end() > u64::from(e.lblk))
        || next.is_some_and(|x| e.end() > u64::from(x.lblk))
        || e.end() > path.bound
    {
        return Err(ExtentError::Overlap.into());
    }

    if let Some(mut p) = prev.filter(|p| p.joins(&e)) {
        p.len += e.len;
        let swallow = next.filter(|x| p.joins(x));
        s.write(node, |n| {
            if let Some(x) = swallow {
                p.len += x.len;
                close_gap(n, at, entries);
                set_entries(n, h.entries - 1);
            }
            put_leaf(n, at - 1, &p);
        })?;
        return Ok(Placed::Done);
    }
    if let Some(x) = next.filter(|x| e.joins(x)) {
        let joined = Extent {
            lblk: e.lblk,
            len: e.len + x.len,
            pblk: e.pblk,
            unwritten: e.unwritten,
        };
        s.write(node, |n| put_leaf(n, at, &joined))?;
        if at == 0 {
            fix_keys(s, path, e.lblk)?;
        }
        return Ok(Placed::Done);
    }
    if h.entries == h.max {
        return Ok(Placed::Full(at));
    }
    s.write(node, |n| {
        open_gap(n, at, entries);
        put_leaf(n, at, &e);
        set_entries(n, h.entries + 1);
    })?;
    if at == 0 {
        fix_keys(s, path, e.lblk)?;
    }
    Ok(Placed::Done)
}

/// Map `[e.lblk, e.end())`, which must be unmapped, joining a neighbour where
/// the run continues one.
pub fn insert<S: Store>(s: &mut S, e: Extent, goal: u64) -> Result<(), S::Error> {
    if e.len == 0 || e.len > e.max_len() || e.end() > MAX_BLOCKS {
        return Err(ExtentError::Corrupt.into());
    }
    for _ in 0..MAX_PASSES {
        let path = descend(s, e.lblk)?;
        match place_in_leaf(s, &path, e)? {
            Placed::Done => return Ok(()),
            Placed::Full(at) => make_room(s, &path, path.leaf(), at, e.lblk, goal)?,
        }
    }
    Err(ExtentError::Corrupt.into())
}

/// Give the node at `level` room for one more entry, pending at `at` with
/// first block `key`. A leaf splits where the entry goes, so an append starts
/// an empty leaf and a file written in order keeps its leaves full.
fn make_room<S: Store>(
    s: &mut S,
    path: &Path,
    level: usize,
    at: usize,
    key: u32,
    goal: u64,
) -> Result<(), S::Error> {
    if level == 0 {
        return grow(s, goal);
    }
    let parent = level - 1;
    if path.headers[parent].entries == path.headers[parent].max {
        return make_room(s, path, parent, path.taken[parent] + 1, key, goal);
    }
    let node = path.nodes[level];
    let h = path.headers[level];
    let entries = usize::from(h.entries);
    let split = if h.depth > 0 || at == 0 {
        entries / 2
    } else {
        at
    };
    let moved = entries - split;
    let block_max = block_capacity(s.block_size());
    if moved > usize::from(block_max) {
        return Err(ExtentError::Corrupt.into());
    }
    let sibling = s.alloc_node(goal)?;
    let first_moved = s.read(node, |n| (moved > 0).then(|| le32(n, entry_at(split))))?;
    s.write(Node::Block(sibling), |n| {
        write_header(
            n,
            Header {
                entries: moved as u16,
                max: block_max,
                depth: h.depth,
            },
        )
    })?;
    let mut staged = [0u8; ENTRY];
    for k in 0..moved {
        s.read(node, |n| {
            staged.copy_from_slice(&n[entry_at(split + k)..entry_at(split + k + 1)])
        })?;
        s.write(Node::Block(sibling), |n| {
            n[entry_at(k)..entry_at(k + 1)].copy_from_slice(&staged)
        })?;
    }
    s.write(node, |n| set_entries(n, split as u16))?;
    let sibling_key = first_moved.unwrap_or(key);
    let slot = path.taken[parent] + 1;
    let ph = path.headers[parent];
    s.write(path.nodes[parent], |n| {
        open_gap(n, slot, usize::from(ph.entries));
        put_index(n, slot, sibling_key, sibling);
        set_entries(n, ph.entries + 1);
    })?;
    Ok(())
}

/// Add a level: the root's entries move into a new block, and the root
/// becomes an index of that one block.
fn grow<S: Store>(s: &mut S, goal: u64) -> Result<(), S::Error> {
    let h = s.read(Node::Root, header)??;
    if h.depth >= MAX_DEPTH {
        return Err(ExtentError::TooDeep.into());
    }
    let child = s.alloc_node(goal)?;
    let mut entries = [0u8; ENTRY * ROOT_MAX as usize];
    let count = usize::from(h.entries);
    let first_key = s.read(Node::Root, |n| {
        entries[..ENTRY * count].copy_from_slice(&n[HEADER..entry_at(count)]);
        if count > 0 { le32(n, HEADER) } else { 0 }
    })?;
    let max = block_capacity(s.block_size());
    s.write(Node::Block(child), |n| {
        write_header(
            n,
            Header {
                entries: h.entries,
                max,
                depth: h.depth,
            },
        );
        n[HEADER..entry_at(count)].copy_from_slice(&entries[..ENTRY * count]);
    })?;
    s.write(Node::Root, |n| {
        n[HEADER..entry_at(usize::from(ROOT_MAX))].fill(0);
        write_header(
            n,
            Header {
                entries: 1,
                max: ROOT_MAX,
                depth: h.depth + 1,
            },
        );
        put_index(n, 0, first_key, child);
    })
}

/// Turn the unwritten block at `lblk` into a written one, splitting its
/// extent around it. The caller owes the block's contents, since what it
/// holds is not the file's.
pub fn mark_written<S: Store>(s: &mut S, lblk: u32, goal: u64) -> Result<(), S::Error> {
    let (path, i, e) = unwritten_with_room_to_split(s, lblk, goal)?;
    let level = path.leaf();
    let h = path.headers[level];
    let node = path.nodes[level];
    let into = lblk - e.lblk;
    let written = Extent {
        lblk,
        len: 1,
        pblk: e.pblk + u64::from(into),
        unwritten: false,
    };
    let after_len = e.len - into - 1;
    let kept = if into == 0 {
        written
    } else {
        Extent { len: into, ..e }
    };
    let entries = usize::from(h.entries);
    s.write(node, |n| {
        let joined = (into == 0 && i > 0)
            .then(|| leaf(n, i - 1))
            .filter(|p| p.joins(&kept));
        match joined {
            Some(mut p) => {
                p.len += kept.len;
                put_leaf(n, i - 1, &p);
                close_gap(n, i, entries);
                set_entries(n, h.entries - 1);
            }
            None => put_leaf(n, i, &kept),
        }
    })?;
    if into != 0 {
        insert(s, written, goal)?;
    }
    if after_len > 0 {
        let after = Extent {
            lblk: lblk + 1,
            len: after_len,
            pblk: written.pblk + 1,
            unwritten: true,
        };
        insert(s, after, goal)?;
    }
    Ok(())
}

/// The path to the unwritten extent holding `lblk` and its entry, once its
/// leaf has room for the pieces splitting it adds: a volume too full to grow
/// the tree refuses the conversion before anything in the leaf changes.
fn unwritten_with_room_to_split<S: Store>(
    s: &mut S,
    lblk: u32,
    goal: u64,
) -> Result<(Path, usize, Extent), S::Error> {
    let mut passes = 0;
    loop {
        let path = descend(s, lblk)?;
        let level = path.leaf();
        let h = path.headers[level];
        let found = s.read(path.nodes[level], |n| {
            search(n, usize::from(h.entries), lblk).map(|i| (i, leaf(n, i)))
        })?;
        let Some((i, e)) = found.filter(|(_, e)| u64::from(lblk) < e.end() && e.unwritten) else {
            return Err(ExtentError::Corrupt.into());
        };
        let added = u16::from(lblk != e.lblk) + u16::from(u64::from(lblk) + 1 < e.end());
        if h.max.saturating_sub(h.entries) >= added {
            return Ok((path, i, e));
        }
        passes += 1;
        if passes > MAX_PASSES {
            return Err(ExtentError::Corrupt.into());
        }
        make_room(s, &path, level, 0, lblk, goal)?;
    }
}

/// Drop every mapping at or past `from`, freeing the blocks and the nodes
/// that empties, and pull the tree back into the inode when it fits there.
pub fn truncate<S: Store>(s: &mut S, from: u32) -> Result<(), S::Error> {
    let root = root_header(s)?;
    let empty = truncate_node(s, Node::Root, root, MAX_BLOCKS, from)?;
    if empty {
        s.write(Node::Root, init_root)?;
        return Ok(());
    }
    collapse(s)
}

fn truncate_node<S: Store>(
    s: &mut S,
    node: Node,
    h: Header,
    hi: u64,
    from: u32,
) -> Result<bool, S::Error> {
    let mut keep = usize::from(h.entries);
    if h.depth == 0 {
        while keep > 0 {
            let e = s.read(node, |n| leaf(n, keep - 1))?;
            if e.lblk >= from {
                s.free_data(e.pblk, e.len)?;
                keep -= 1;
                continue;
            }
            if e.end() > u64::from(from) {
                let kept = from - e.lblk;
                s.free_data(e.pblk + u64::from(kept), e.len - kept)?;
                s.write(node, |n| put_leaf(n, keep - 1, &Extent { len: kept, ..e }))?;
            }
            break;
        }
    } else {
        // Every subtree whose key is at or past `from` empties, so what is
        // removed is always a suffix; anything else is a damaged tree.
        for i in (0..keep).rev() {
            let (key, child, next) = s.read(node, |n| index_span(n, h, i, hi))?;
            let child_h = child_header(s, child, h, key, next)?;
            let emptied = truncate_node(s, Node::Block(child), child_h, next, from)?;
            if emptied {
                if keep != i + 1 {
                    return Err(ExtentError::Corrupt.into());
                }
                s.free_node(child)?;
                keep = i;
            }
            if key < u64::from(from) {
                break;
            }
        }
    }
    if keep != usize::from(h.entries) {
        s.write(node, |n| set_entries(n, keep as u16))?;
    }
    Ok(keep == 0)
}

/// While the root indexes a single node whose entries fit in the root, move
/// them up and free the node.
fn collapse<S: Store>(s: &mut S) -> Result<(), S::Error> {
    loop {
        let h = s.read(Node::Root, header)??;
        if h.depth == 0 || h.entries != 1 {
            return Ok(());
        }
        let child = s.read(Node::Root, |n| index(n, 0).1)?;
        let mut entries = [0u8; ENTRY * ROOT_MAX as usize];
        let child_h = s.read(Node::Block(child), header)??;
        if child_h.entries > ROOT_MAX || child_h.depth + 1 != h.depth {
            return Ok(());
        }
        let count = usize::from(child_h.entries);
        s.read(Node::Block(child), |n| {
            entries[..ENTRY * count].copy_from_slice(&n[HEADER..entry_at(count)])
        })?;
        s.write(Node::Root, |n| {
            n[HEADER..entry_at(usize::from(ROOT_MAX))].fill(0);
            n[HEADER..entry_at(count)].copy_from_slice(&entries[..ENTRY * count]);
            write_header(
                n,
                Header {
                    entries: child_h.entries,
                    max: ROOT_MAX,
                    depth: child_h.depth,
                },
            );
        })?;
        s.free_node(child)?;
    }
}

/// Visit every extent in logical order; `f` answers whether to go on.
pub fn for_each<S: Store>(s: &mut S, f: &mut dyn FnMut(Extent) -> bool) -> Result<(), S::Error> {
    let root = root_header(s)?;
    walk(s, Node::Root, root, MAX_BLOCKS, f).map(|_| ())
}

fn walk<S: Store>(
    s: &mut S,
    node: Node,
    h: Header,
    hi: u64,
    f: &mut dyn FnMut(Extent) -> bool,
) -> Result<bool, S::Error> {
    for i in 0..usize::from(h.entries) {
        if h.depth == 0 {
            let e = s.read(node, |n| leaf(n, i))?;
            if !f(e) {
                return Ok(false);
            }
            continue;
        }
        let (key, child, next) = s.read(node, |n| index_span(n, h, i, hi))?;
        let child_h = child_header(s, child, h, key, next)?;
        if !walk(s, Node::Block(child), child_h, next, f)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Every tree block, depth-first; `f` answers whether to go on.
pub fn for_each_node<S: Store>(s: &mut S, f: &mut dyn FnMut(u64) -> bool) -> Result<(), S::Error> {
    let root = root_header(s)?;
    walk_nodes(s, Node::Root, root, MAX_BLOCKS, f).map(|_| ())
}

fn walk_nodes<S: Store>(
    s: &mut S,
    node: Node,
    h: Header,
    hi: u64,
    f: &mut dyn FnMut(u64) -> bool,
) -> Result<bool, S::Error> {
    if h.depth == 0 {
        return Ok(true);
    }
    for i in 0..usize::from(h.entries) {
        let (key, child, next) = s.read(node, |n| index_span(n, h, i, hi))?;
        if !f(child) {
            return Ok(false);
        }
        let child_h = child_header(s, child, h, key, next)?;
        if !walk_nodes(s, Node::Block(child), child_h, next, f)? {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    /// Tree blocks in memory, data allocation from a bump pointer, and every
    /// free recorded.
    struct Mem {
        root: [u8; 60],
        blocks: BTreeMap<u64, Vec<u8>>,
        block_size: usize,
        next: u64,
        freed_data: u64,
        freed_nodes: u64,
        fail_alloc: bool,
    }

    impl Mem {
        fn new(block_size: usize) -> Self {
            let mut root = [0u8; 60];
            init_root(&mut root);
            Self {
                root,
                blocks: BTreeMap::new(),
                block_size,
                next: 1_000_000,
                freed_data: 0,
                freed_nodes: 0,
                fail_alloc: false,
            }
        }
    }

    impl Store for Mem {
        type Error = ExtentError;

        fn block_size(&self) -> usize {
            self.block_size
        }

        fn read<R>(&mut self, node: Node, f: impl FnOnce(&[u8]) -> R) -> Result<R, ExtentError> {
            match node {
                Node::Root => Ok(f(&self.root)),
                Node::Block(b) => {
                    let n = self.blocks.get(&b).ok_or(ExtentError::Corrupt)?;
                    assert!(verify(7, n), "tail checksum of {b}");
                    Ok(f(n))
                }
            }
        }

        fn write<R>(
            &mut self,
            node: Node,
            f: impl FnOnce(&mut [u8]) -> R,
        ) -> Result<R, ExtentError> {
            match node {
                Node::Root => Ok(f(&mut self.root)),
                Node::Block(b) => {
                    let n = self.blocks.get_mut(&b).ok_or(ExtentError::Corrupt)?;
                    let r = f(n);
                    seal(7, n);
                    Ok(r)
                }
            }
        }

        fn alloc_node(&mut self, _goal: u64) -> Result<u64, ExtentError> {
            if self.fail_alloc {
                return Err(ExtentError::TooDeep);
            }
            self.next += 1;
            self.blocks.insert(self.next, vec![0u8; self.block_size]);
            Ok(self.next)
        }

        fn free_node(&mut self, block: u64) -> Result<(), ExtentError> {
            self.blocks.remove(&block).ok_or(ExtentError::Corrupt)?;
            self.freed_nodes += 1;
            Ok(())
        }

        fn free_data(&mut self, _first: u64, count: u32) -> Result<(), ExtentError> {
            self.freed_data += u64::from(count);
            Ok(())
        }
    }

    fn all(s: &mut Mem) -> Vec<Extent> {
        let mut out = Vec::new();
        for_each(s, &mut |e| {
            out.push(e);
            true
        })
        .unwrap();
        out
    }

    /// Structural invariants: sorted, non-overlapping, keys at or below the
    /// first block of their subtree, depth uniform.
    fn check(s: &mut Mem) {
        let root = s.read(Node::Root, header).unwrap().unwrap();
        check_node(s, Node::Root, root, 0, u32::MAX);
        let v = all(s);
        for w in v.windows(2) {
            assert!(w[0].end() <= u64::from(w[1].lblk), "{w:?}");
        }
    }

    fn check_node(s: &mut Mem, node: Node, h: Header, lo: u32, hi: u32) {
        for i in 0..usize::from(h.entries) {
            if h.depth == 0 {
                let e = s.read(node, |n| leaf(n, i)).unwrap();
                assert!(
                    e.lblk >= lo && e.end() <= u64::from(hi) + 1,
                    "{e:?} outside {lo}..{hi}"
                );
                continue;
            }
            let (key, child) = s.read(node, |n| index(n, i)).unwrap();
            let next_key = if i + 1 < usize::from(h.entries) {
                s.read(node, |n| index(n, i + 1).0).unwrap()
            } else {
                hi
            };
            assert!(key >= lo && key <= next_key);
            let ch = s.read(Node::Block(child), header).unwrap().unwrap();
            assert_eq!(ch.depth + 1, h.depth);
            assert!(ch.entries > 0, "empty node left in the tree");
            let first = s
                .read(Node::Block(child), |n| le32(n, entry_at(0)))
                .unwrap();
            assert_eq!(key, first, "an index key is its subtree's first block");
            check_node(
                s,
                Node::Block(child),
                ch,
                key,
                next_key.saturating_sub(1).max(key),
            );
        }
    }

    #[test]
    fn append_merges_into_one_extent() {
        let mut s = Mem::new(1024);
        for i in 0..100u32 {
            insert(
                &mut s,
                Extent {
                    lblk: i,
                    len: 1,
                    pblk: 5000 + u64::from(i),
                    unwritten: false,
                },
                0,
            )
            .unwrap();
        }
        assert_eq!(
            all(&mut s),
            [Extent {
                lblk: 0,
                len: 100,
                pblk: 5000,
                unwritten: false
            }]
        );
        assert_eq!(lookup(&mut s, 42).unwrap().unwrap().pblk, 5042);
        assert_eq!(lookup(&mut s, 100).unwrap(), None);
    }

    #[test]
    fn fragmented_appends_grow_and_split() {
        let mut s = Mem::new(1024);
        for i in 0..2000u32 {
            insert(
                &mut s,
                Extent {
                    lblk: i,
                    len: 1,
                    pblk: 10 * u64::from(i) + 1,
                    unwritten: false,
                },
                0,
            )
            .unwrap();
        }
        check(&mut s);
        let root = s.read(Node::Root, header).unwrap().unwrap();
        assert!(
            root.depth >= 2,
            "2000 extents over 84-entry leaves need two levels"
        );
        for i in 0..2000u32 {
            assert_eq!(
                lookup(&mut s, i).unwrap().unwrap().pblk,
                10 * u64::from(i) + 1
            );
        }
        truncate(&mut s, 0).unwrap();
        assert!(s.blocks.is_empty());
        assert_eq!(s.freed_data, 2000);
        assert_eq!(s.read(Node::Root, header).unwrap().unwrap().depth, 0);
    }

    #[test]
    fn truncate_trims_and_collapses() {
        let mut s = Mem::new(1024);
        for i in 0..400u32 {
            insert(
                &mut s,
                Extent {
                    lblk: 2 * i,
                    len: 1,
                    pblk: 3 * u64::from(i) + 7,
                    unwritten: false,
                },
                0,
            )
            .unwrap();
        }
        truncate(&mut s, 5).unwrap();
        check(&mut s);
        assert_eq!(all(&mut s).len(), 3);
        let root = s.read(Node::Root, header).unwrap().unwrap();
        assert_eq!(root.depth, 0, "three extents fit the inode again");
        assert!(s.blocks.is_empty());
    }

    #[test]
    fn unwritten_block_splits_in_three() {
        let mut s = Mem::new(1024);
        insert(
            &mut s,
            Extent {
                lblk: 10,
                len: 20,
                pblk: 100,
                unwritten: true,
            },
            0,
        )
        .unwrap();
        mark_written(&mut s, 15, 0).unwrap();
        assert_eq!(
            all(&mut s),
            [
                Extent {
                    lblk: 10,
                    len: 5,
                    pblk: 100,
                    unwritten: true
                },
                Extent {
                    lblk: 15,
                    len: 1,
                    pblk: 105,
                    unwritten: false
                },
                Extent {
                    lblk: 16,
                    len: 14,
                    pblk: 106,
                    unwritten: true
                },
            ]
        );
        mark_written(&mut s, 16, 0).unwrap();
        assert_eq!(
            all(&mut s)[1],
            Extent {
                lblk: 15,
                len: 2,
                pblk: 105,
                unwritten: false
            }
        );
    }

    #[test]
    fn overlap_is_refused() {
        let mut s = Mem::new(1024);
        insert(
            &mut s,
            Extent {
                lblk: 10,
                len: 5,
                pblk: 100,
                unwritten: false,
            },
            0,
        )
        .unwrap();
        assert_eq!(
            insert(
                &mut s,
                Extent {
                    lblk: 12,
                    len: 1,
                    pblk: 900,
                    unwritten: false
                },
                0
            ),
            Err(ExtentError::Overlap)
        );
        assert_eq!(
            insert(
                &mut s,
                Extent {
                    lblk: 5,
                    len: 6,
                    pblk: 900,
                    unwritten: false
                },
                0
            ),
            Err(ExtentError::Overlap)
        );
    }

    fn one(lblk: u32, len: u32, pblk: u64, unwritten: bool) -> Extent {
        Extent {
            lblk,
            len,
            pblk,
            unwritten,
        }
    }

    #[test]
    fn the_last_file_block_is_the_one_before_no_block() {
        let mut s = Mem::new(1024);
        let last = (MAX_BLOCKS - 1) as u32;
        assert_eq!(
            insert(&mut s, one(u32::MAX, 1, 7, false), 0),
            Err(ExtentError::Corrupt)
        );
        insert(&mut s, one(last - 1, 2, 7, true), 0).unwrap();
        mark_written(&mut s, last, 0).unwrap();
        assert_eq!(
            all(&mut s),
            [one(last - 1, 1, 7, true), one(last, 1, 8, false)]
        );
    }

    /// Root entries laid down as given, unchecked.
    fn raw_root(s: &mut Mem, depth: u16, entries: &[(u32, u32, u64)]) {
        write_header(
            &mut s.root,
            Header {
                entries: entries.len() as u16,
                max: ROOT_MAX,
                depth,
            },
        );
        for (i, &(key, len, block)) in entries.iter().enumerate() {
            if depth == 0 {
                put_leaf(&mut s.root, i, &one(key, len, block, false));
            } else {
                put_index(&mut s.root, i, key, block);
            }
        }
    }

    /// A node two index entries share holds blocks inside only one of their
    /// spans, so a walk stops at the second visit rather than repeating it.
    #[test]
    fn a_node_two_parents_share_is_refused() {
        let mut s = Mem::new(1024);
        let shared = s.alloc_node(0).unwrap();
        s.write(Node::Block(shared), |n| {
            write_header(
                n,
                Header {
                    entries: 1,
                    max: block_capacity(1024),
                    depth: 0,
                },
            );
            put_leaf(n, 0, &one(10, 1, 500, false));
        })
        .unwrap();
        raw_root(&mut s, 1, &[(0, 0, shared), (100, 0, shared)]);
        assert_eq!(for_each(&mut s, &mut |_| true), Err(ExtentError::Corrupt));
        assert_eq!(
            for_each_node(&mut s, &mut |_| true),
            Err(ExtentError::Corrupt)
        );
    }

    #[test]
    fn a_node_that_cannot_split_is_refused() {
        let mut s = Mem::new(1024);
        let narrow = s.alloc_node(0).unwrap();
        s.write(Node::Block(narrow), |n| {
            write_header(
                n,
                Header {
                    entries: 1,
                    max: 1,
                    depth: 0,
                },
            );
            put_leaf(n, 0, &one(10, 1, 500, false));
        })
        .unwrap();
        raw_root(&mut s, 1, &[(0, 0, narrow)]);
        assert_eq!(lookup(&mut s, 10), Err(ExtentError::Corrupt));
        assert_eq!(
            insert(&mut s, one(50, 1, 900, false), 0),
            Err(ExtentError::Corrupt)
        );
    }

    #[test]
    fn entries_out_of_order_or_empty_are_refused() {
        let mut s = Mem::new(1024);
        raw_root(&mut s, 0, &[(100, 10, 500), (50, 10, 900)]);
        assert_eq!(
            insert(&mut s, one(105, 1, 7000, false), 0),
            Err(ExtentError::Corrupt)
        );
        raw_root(&mut s, 0, &[(10, 0, 500)]);
        assert_eq!(lookup(&mut s, 10), Err(ExtentError::Corrupt));
        raw_root(&mut s, 1, &[]);
        assert_eq!(lookup(&mut s, 10), Err(ExtentError::Corrupt));
    }

    #[test]
    fn a_conversion_that_cannot_split_leaves_the_tree_as_it_was() {
        let mut s = Mem::new(1024);
        for i in 0..3u32 {
            insert(&mut s, one(10 * i, 1, 100 + 10 * u64::from(i), false), 0).unwrap();
        }
        insert(&mut s, one(40, 5, 500, true), 0).unwrap();
        let before = all(&mut s);
        s.fail_alloc = true;
        assert!(mark_written(&mut s, 42, 0).is_err());
        assert_eq!(all(&mut s), before);
        s.fail_alloc = false;
        mark_written(&mut s, 42, 0).unwrap();
        assert_eq!(lookup(&mut s, 42).unwrap().unwrap().unwritten, false);
        check(&mut s);
    }

    /// An entry whose run reaches into the next leaf's range overlaps it,
    /// though nothing in its own leaf does.
    #[test]
    fn overlap_across_leaves_is_refused() {
        let mut s = Mem::new(1024);
        for i in 0..200u32 {
            insert(&mut s, one(4 * i, 1, 10 * u64::from(i) + 1, false), 0).unwrap();
        }
        assert!(s.read(Node::Root, header).unwrap().unwrap().depth > 0);
        let boundary = s.read(Node::Root, |n| index(n, 1).0).unwrap();
        assert_eq!(
            insert(&mut s, one(boundary - 1, 2, 9000, false), 0),
            Err(ExtentError::Overlap)
        );
        check(&mut s);
    }

    /// Random inserts, truncates and conversions against a plain map.
    #[test]
    fn matches_a_model_under_random_operations() {
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for block_size in [1024usize, 4096] {
            let mut s = Mem::new(block_size);
            let mut model: BTreeMap<u32, (u64, bool)> = BTreeMap::new();
            let mut phys = 1u64;
            for round in 0..6000 {
                let op = next() % 10;
                if op < 7 {
                    let lblk = (next() % 20_000) as u32;
                    let len = 1 + (next() % 8) as u32;
                    if (lblk..lblk + len).any(|b| model.contains_key(&b)) {
                        continue;
                    }
                    let unwritten = next() % 4 == 0;
                    let pblk = if next() % 3 == 0 { phys + 50 } else { phys };
                    phys = pblk + u64::from(len);
                    insert(
                        &mut s,
                        Extent {
                            lblk,
                            len,
                            pblk,
                            unwritten,
                        },
                        0,
                    )
                    .unwrap();
                    for k in 0..len {
                        model.insert(lblk + k, (pblk + u64::from(k), unwritten));
                    }
                } else if op < 9 {
                    let unwritten: Vec<u32> = model
                        .iter()
                        .filter(|(_, v)| v.1)
                        .map(|(k, _)| *k)
                        .take(50)
                        .collect();
                    if let Some(&b) = unwritten.get((next() % 50) as usize) {
                        mark_written(&mut s, b, 0).unwrap();
                        model.get_mut(&b).unwrap().1 = false;
                    }
                } else if round % 7 == 0 {
                    let from = (next() % 20_000) as u32;
                    truncate(&mut s, from).unwrap();
                    model.retain(|&k, _| k < from);
                }
            }
            check(&mut s);
            let mut flat = BTreeMap::new();
            for e in all(&mut s) {
                for k in 0..e.len {
                    flat.insert(e.lblk + k, (e.pblk + u64::from(k), e.unwritten));
                }
            }
            assert_eq!(flat, model);
            for (&k, &(p, u)) in model.iter().step_by(37) {
                let m = lookup(&mut s, k).unwrap().unwrap();
                assert_eq!((m.pblk, m.unwritten), (p, u));
            }
            truncate(&mut s, 0).unwrap();
            assert!(s.blocks.is_empty(), "every node freed");
        }
    }

    #[test]
    fn failed_split_leaves_the_tree_readable() {
        let mut s = Mem::new(1024);
        for i in 0..4u32 {
            insert(
                &mut s,
                Extent {
                    lblk: 2 * i,
                    len: 1,
                    pblk: 3 * u64::from(i) + 1,
                    unwritten: false,
                },
                0,
            )
            .unwrap();
        }
        s.fail_alloc = true;
        assert!(
            insert(
                &mut s,
                Extent {
                    lblk: 100,
                    len: 1,
                    pblk: 999,
                    unwritten: false
                },
                0
            )
            .is_err()
        );
        check(&mut s);
        assert_eq!(all(&mut s).len(), 4);
    }
}
