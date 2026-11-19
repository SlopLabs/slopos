//! Laying a volume out from a tree, then writing it block by block in
//! order, so an image of any size streams to its file.
//!
//! Group 0 starts with the superblock and the descriptor table, and every
//! group's bitmaps and inode table follow them, as `flex_bg` lets them lie
//! anywhere; the groups `sparse_super` names keep a backup of both. Then
//! each directory, file and symlink takes its blocks in turn, contiguous but
//! for the backups in the way, its extent leaves after them. Nothing else is
//! free but what a backup left unused before it, so a volume is as large as
//! its tree. Inode numbers follow the tree in order, children by name, and
//! nothing records a time, so a tree lays out the same way every time.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use slopos_ext4_core::bytes::{put_le16, put_le32};
use slopos_ext4_core::crc::crc32c;
use slopos_ext4_core::extent::{self, Extent, Header};
use slopos_ext4_core::group::{self, Desc, DescCsum};
use slopos_ext4_core::inode::{self, off as ioff};
use slopos_ext4_core::superblock::{self as sbk, has_backup, off as soff};
use slopos_ext4_core::{dir, superblock};

use super::{
    BLOCK, BLOCK_GROUP_NR, BLOCKS_PER_GROUP, COMPAT, DESC_SIZE, EXTRA_ISIZE, FAST_SYMLINK_MAX,
    INCOMPAT, INODE_SIZE, INODES_PER_BLOCK, Kind, LOST_FOUND, LOST_FOUND_NAME, RO_COMPAT, ROOT,
    valid_name,
};

/// One thing the volume holds, by its path from the volume's root, and its
/// permission bits.
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: String,
    pub perm: u16,
    pub what: What,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Directory,
    /// A file this long, whose bytes [`Layout::write`] asks for.
    File(u64),
    Symlink(Vec<u8>),
}

impl What {
    fn kind(&self) -> Kind {
        match self {
            What::Directory => Kind::Directory,
            What::File(_) => Kind::File,
            What::Symlink(_) => Kind::Symlink,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// A path that is not absolute and canonical, names something twice, or
    /// lies under something that is no directory.
    Path(String),
    /// A symlink target the volume cannot hold.
    Target(String),
    /// A file whose blocks would take more extents than two levels hold.
    Fragmented(String),
    /// Past the 2³² blocks or inodes the kernel addresses.
    TooLarge,
}

impl core::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LayoutError::Path(path) => write!(f, "{path}: no path a volume can hold"),
            LayoutError::Target(path) => {
                write!(f, "{path}: a symlink whose target a volume cannot hold")
            }
            LayoutError::Fragmented(path) => write!(f, "{path}: too large a file"),
            LayoutError::TooLarge => f.write_str("larger than a volume can be"),
        }
    }
}

const NO_ENTRY: usize = usize::MAX;

fn leaf_max() -> usize {
    usize::from(extent::block_capacity(BLOCK))
}

struct Node {
    name: Vec<u8>,
    parent: usize,
    kind: Kind,
    perm: u16,
    size: u64,
    /// The caller's entry whose bytes fill it.
    entry: usize,
    target: Vec<u8>,
    children: Vec<usize>,
    ino: u32,
    extents: Vec<Extent>,
    leaves: Vec<u64>,
}

impl Node {
    fn fast_symlink(&self) -> bool {
        self.kind == Kind::Symlink && self.size as usize <= FAST_SYMLINK_MAX
    }

    fn data_blocks(&self) -> u64 {
        self.extents.iter().map(|e| u64::from(e.len)).sum()
    }

    fn blocks(&self) -> u64 {
        self.data_blocks() + self.leaves.len() as u64
    }
}

/// Where everything lies, from which [`Layout::write`] makes every block.
pub struct Layout {
    nodes: Vec<Node>,
    /// Node index by inode number, from [`ROOT`].
    by_ino: Vec<usize>,
    uuid: [u8; 16],
    seed: u32,
    groups: u32,
    inodes_per_group: u32,
    gdt_blocks: u64,
    blocks: u64,
    /// Blocks left free, as ranges.
    gaps: Vec<(u64, u64)>,
    descs: Vec<Desc>,
    block_bitmaps: Vec<u64>,
    inode_bitmaps: Vec<u64>,
    inode_tables: Vec<u64>,
}

/// Hands out blocks in order, around the backups' reserved ranges.
struct Alloc {
    next: u64,
    reserved: Vec<(u64, u64)>,
    gaps: Vec<(u64, u64)>,
}

impl Alloc {
    fn reserved_after(&self, at: u64) -> Option<(u64, u64)> {
        self.reserved.iter().copied().find(|&(_, end)| end > at)
    }

    fn skip_reserved(&mut self) {
        while let Some((start, end)) = self.reserved_after(self.next) {
            if start > self.next {
                break;
            }
            self.next = end;
        }
    }

    /// `n` contiguous blocks.
    fn take(&mut self, n: u64) -> u64 {
        loop {
            self.skip_reserved();
            match self.reserved_after(self.next) {
                Some((start, _)) if start < self.next + n => {
                    self.gaps.push((self.next, start));
                    self.next = start;
                }
                _ => {
                    self.next += n;
                    return self.next - n;
                }
            }
        }
    }

    /// Up to `n` contiguous blocks, stopping short of a reserved range.
    fn run(&mut self, n: u64) -> (u64, u64) {
        self.skip_reserved();
        let start = self.next;
        let len = match self.reserved_after(start) {
            Some((reserved, _)) => n.min(reserved - start),
            None => n,
        };
        self.next += len;
        (start, len)
    }
}

/// The tree `entries` describe, rooted at node 0, `lost+found` at node 1,
/// every directory a path implies made with mode 0755.
fn tree(entries: &[Entry]) -> Result<Vec<Node>, LayoutError> {
    let node = |name: &[u8], parent, kind, perm| Node {
        name: name.to_vec(),
        parent,
        kind,
        perm,
        size: 0,
        entry: NO_ENTRY,
        target: Vec::new(),
        children: Vec::new(),
        ino: 0,
        extents: Vec::new(),
        leaves: Vec::new(),
    };
    let mut nodes = vec![
        node(b"", 0, Kind::Directory, 0o755),
        node(LOST_FOUND_NAME, 0, Kind::Directory, 0o700),
    ];
    nodes[0].children.push(1);
    let mut paths: BTreeMap<String, usize> = BTreeMap::new();
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&a, &b| entries[a].path.cmp(&entries[b].path));
    for i in order {
        let entry = &entries[i];
        let bad = || LayoutError::Path(entry.path.clone());
        let components: Vec<&str> = entry
            .path
            .strip_prefix('/')
            .ok_or_else(bad)?
            .split('/')
            .collect();
        let mut at = 0;
        let mut walked = String::new();
        for (depth, part) in components.iter().enumerate() {
            if !valid_name(part.as_bytes()) || (at == 0 && part.as_bytes() == LOST_FOUND_NAME) {
                return Err(bad());
            }
            walked.push('/');
            walked.push_str(part);
            let last = depth + 1 == components.len();
            match (paths.get(&walked).copied(), last) {
                (Some(n), false) if nodes[n].kind == Kind::Directory => at = n,
                (Some(_), _) => return Err(bad()),
                (None, false) => {
                    nodes.push(node(part.as_bytes(), at, Kind::Directory, 0o755));
                    at = nodes.len() - 1;
                }
                (None, true) => {
                    let mut leaf =
                        node(part.as_bytes(), at, entry.what.kind(), entry.perm & 0o7777);
                    leaf.entry = i;
                    match &entry.what {
                        What::Directory => {}
                        What::File(size) => leaf.size = *size,
                        What::Symlink(target) => {
                            if target.is_empty() || target.len() >= BLOCK {
                                return Err(LayoutError::Target(entry.path.clone()));
                            }
                            leaf.size = target.len() as u64;
                            leaf.target = target.clone();
                        }
                    }
                    nodes.push(leaf);
                    at = nodes.len() - 1;
                }
            }
            if paths.get(&walked).is_none() {
                paths.insert(walked.clone(), at);
                let parent = nodes[at].parent;
                nodes[parent].children.push(at);
            }
        }
    }
    let names: Vec<Vec<u8>> = nodes.iter().map(|n| n.name.clone()).collect();
    for node in &mut nodes {
        node.children.sort_by(|&a, &b| names[a].cmp(&names[b]));
    }
    Ok(nodes)
}

/// Directory entries `dir` holds, `.` and `..` first, cut into blocks.
fn dir_blocks(nodes: &[Node], dir: usize) -> Vec<Vec<(Vec<u8>, u32, u8)>> {
    let usable = dir::usable(BLOCK, true);
    let node = &nodes[dir];
    let entries = [
        (b".".to_vec(), node.ino, Kind::Directory.dirent()),
        (
            b"..".to_vec(),
            nodes[node.parent].ino,
            Kind::Directory.dirent(),
        ),
    ]
    .into_iter()
    .chain(node.children.iter().map(|&c| {
        let child = &nodes[c];
        (child.name.clone(), child.ino, child.kind.dirent())
    }));
    let mut blocks: Vec<Vec<(Vec<u8>, u32, u8)>> = vec![Vec::new()];
    let mut used = 0;
    for entry in entries {
        let len = rec_len(entry.0.len());
        if used + len > usable {
            blocks.push(Vec::new());
            used = 0;
        }
        used += len;
        if let Some(block) = blocks.last_mut() {
            block.push(entry);
        }
    }
    blocks
}

fn rec_len(name: usize) -> usize {
    (8 + name).next_multiple_of(4)
}

fn data_blocks(nodes: &[Node], n: usize) -> u64 {
    let node = &nodes[n];
    match node.kind {
        Kind::Directory => dir_blocks(nodes, n).len() as u64,
        Kind::File => node.size.div_ceil(BLOCK as u64),
        Kind::Symlink if node.fast_symlink() => 0,
        Kind::Symlink => 1,
    }
}

/// The volume `entries` describe, laid out.
pub fn layout(entries: &[Entry], uuid: [u8; 16]) -> Result<Layout, LayoutError> {
    let mut nodes = tree(entries)?;
    let mut order = vec![0usize];
    let mut i = 0;
    while i < order.len() {
        order.extend(nodes[order[i]].children.iter().copied().filter(|&c| c != 1));
        i += 1;
    }
    let last_ino =
        u32::try_from(order.len() + LOST_FOUND as usize - 1).map_err(|_| LayoutError::TooLarge)?;
    let mut by_ino = vec![NO_ENTRY; last_ino as usize + 1];
    for (k, &n) in order.iter().enumerate() {
        let ino = if k == 0 { ROOT } else { LOST_FOUND + k as u32 };
        nodes[n].ino = ino;
        by_ino[ino as usize] = n;
    }
    nodes[1].ino = LOST_FOUND;
    by_ino[LOST_FOUND as usize] = 1;
    let blocks_of: Vec<u64> = (0..nodes.len()).map(|n| data_blocks(&nodes, n)).collect();
    let max_inodes_per_group = 8 * BLOCK as u32;
    let mut groups: u32 = 1;
    loop {
        let per_group = last_ino.div_ceil(groups).next_multiple_of(INODES_PER_BLOCK);
        if per_group > max_inodes_per_group {
            groups = last_ino.div_ceil(max_inodes_per_group);
            continue;
        }
        let gdt_blocks = u64::from(groups).div_ceil((BLOCK / DESC_SIZE) as u64);
        let reserved: Vec<(u64, u64)> = (1..groups)
            .filter(|&g| has_backup(g, true))
            .map(|g| {
                let start = u64::from(g) * BLOCKS_PER_GROUP;
                (start, start + 1 + gdt_blocks)
            })
            .collect();
        let mut alloc = Alloc {
            next: 0,
            reserved,
            gaps: Vec::new(),
        };
        alloc.take(1 + gdt_blocks);
        let block_bitmaps: Vec<u64> = (0..groups).map(|_| alloc.take(1)).collect();
        let inode_bitmaps: Vec<u64> = (0..groups).map(|_| alloc.take(1)).collect();
        let table_blocks = u64::from(per_group / INODES_PER_BLOCK);
        let inode_tables: Vec<u64> = (0..groups).map(|_| alloc.take(table_blocks)).collect();
        for &n in core::iter::once(&0).chain(&[1]).chain(&order[1..]) {
            place(&mut nodes[n], blocks_of[n], &mut alloc)?;
        }
        let mut blocks = alloc.next;
        let needed =
            u32::try_from(blocks.div_ceil(BLOCKS_PER_GROUP)).map_err(|_| LayoutError::TooLarge)?;
        if needed > groups {
            groups = needed;
            continue;
        }
        let last = u64::from(groups - 1) * BLOCKS_PER_GROUP;
        let floor = last
            + alloc
                .reserved
                .last()
                .filter(|r| r.0 == last)
                .map_or(0, |r| r.1 - r.0)
            + 1;
        if blocks < floor {
            let mut at = blocks;
            for &(start, end) in alloc
                .reserved
                .iter()
                .filter(|r| r.1 > blocks && r.0 < floor)
            {
                if start > at {
                    alloc.gaps.push((at, start));
                }
                at = at.max(end);
            }
            if floor > at {
                alloc.gaps.push((at, floor));
            }
            blocks = floor;
        }
        if blocks > slopos_verity_core::MAX_DATA_BLOCKS {
            return Err(LayoutError::TooLarge);
        }
        let seed = crc32c(!0, &uuid);
        let mut layout = Layout {
            nodes,
            by_ino,
            uuid,
            seed,
            groups,
            inodes_per_group: per_group,
            gdt_blocks,
            blocks,
            gaps: alloc.gaps,
            descs: Vec::new(),
            block_bitmaps,
            inode_bitmaps,
            inode_tables,
        };
        layout.descs = (0..groups).map(|g| layout.desc(g)).collect();
        return Ok(layout);
    }
}

/// Give `node` its `count` data blocks, then the extent leaves they need.
fn place(node: &mut Node, count: u64, alloc: &mut Alloc) -> Result<(), LayoutError> {
    node.extents.clear();
    node.leaves.clear();
    let mut lblk = 0u64;
    while lblk < count {
        let (start, len) = alloc.run((count - lblk).min(u64::from(extent::MAX_LEN)));
        node.extents.push(Extent {
            lblk: u32::try_from(lblk).map_err(|_| LayoutError::TooLarge)?,
            len: len as u32,
            pblk: start,
            unwritten: false,
        });
        lblk += len;
    }
    if node.extents.len() > usize::from(extent::ROOT_MAX) {
        let leaves = node.extents.len().div_ceil(leaf_max());
        if leaves > usize::from(extent::ROOT_MAX) {
            return Err(LayoutError::Fragmented(
                String::from_utf8_lossy(&node.name).into(),
            ));
        }
        node.leaves = (0..leaves).map(|_| alloc.take(1)).collect();
    }
    Ok(())
}

/// What one block of the volume is.
enum Part {
    Super,
    Table(u64),
    Backup(u32),
    BlockBitmap(u32),
    InodeBitmap(u32),
    Inodes(u32, u64),
    Data(usize, u64),
    Leaf(usize, usize),
    Free,
}

impl Layout {
    /// How many blocks the volume takes.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    fn free(&self, block: u64) -> bool {
        self.gaps
            .iter()
            .any(|&(start, end)| (start..end).contains(&block))
    }

    fn free_in(&self, g: u32) -> u64 {
        let (lo, hi) = self.group_span(g);
        self.gaps
            .iter()
            .map(|&(start, end)| end.min(hi).saturating_sub(start.max(lo)))
            .sum()
    }

    fn group_span(&self, g: u32) -> (u64, u64) {
        let lo = u64::from(g) * BLOCKS_PER_GROUP;
        (lo, (lo + BLOCKS_PER_GROUP).min(self.blocks))
    }

    fn used_inodes_in(&self, g: u32) -> u32 {
        let first = g * self.inodes_per_group;
        let last = self.by_ino.len() as u32 - 1;
        last.saturating_sub(first).min(self.inodes_per_group)
    }

    fn block_bitmap(&self, g: u32, out: &mut [u8; BLOCK]) {
        out.fill(0);
        let (lo, hi) = self.group_span(g);
        for bit in 0..BLOCKS_PER_GROUP {
            let block = lo + bit;
            if block >= hi || !self.free(block) {
                out[(bit / 8) as usize] |= 1 << (bit % 8);
            }
        }
    }

    fn inode_bitmap(&self, g: u32, out: &mut [u8; BLOCK]) {
        out.fill(0);
        let used = self.used_inodes_in(g);
        for bit in (0..used).chain(self.inodes_per_group..8 * BLOCK as u32) {
            out[(bit / 8) as usize] |= 1 << (bit % 8);
        }
    }

    fn desc(&self, g: u32) -> Desc {
        let first = g * self.inodes_per_group + 1;
        let dirs = (first..first + self.used_inodes_in(g))
            .filter_map(|ino| self.node_of(ino))
            .filter(|node| node.kind == Kind::Directory)
            .count() as u32;
        let mut bitmap = [0u8; BLOCK];
        self.block_bitmap(g, &mut bitmap);
        let block_bitmap_csum = group::stored_bitmap_csum(
            group::bitmap_checksum(self.seed, &bitmap, BLOCKS_PER_GROUP as u32),
            DESC_SIZE,
        );
        self.inode_bitmap(g, &mut bitmap);
        let inode_bitmap_csum = group::stored_bitmap_csum(
            group::bitmap_checksum(self.seed, &bitmap, self.inodes_per_group),
            DESC_SIZE,
        );
        let used = self.used_inodes_in(g);
        Desc {
            block_bitmap: self.block_bitmaps[g as usize],
            inode_bitmap: self.inode_bitmaps[g as usize],
            inode_table: self.inode_tables[g as usize],
            free_blocks: self.free_in(g) as u32,
            free_inodes: self.inodes_per_group - used,
            used_dirs: dirs,
            itable_unused: self.inodes_per_group - used,
            flags: group::ITABLE_ZEROED,
            block_bitmap_csum,
            inode_bitmap_csum,
        }
    }

    fn node_of(&self, ino: u32) -> Option<&Node> {
        self.by_ino
            .get(ino as usize)
            .filter(|&&n| n != NO_ENTRY)
            .map(|&n| &self.nodes[n])
    }

    fn superblock(&self, group_nr: u32, sb: &mut [u8]) {
        sb.fill(0);
        let inodes = self.groups * self.inodes_per_group;
        let free_blocks: u64 = (0..self.groups).map(|g| self.free_in(g)).sum();
        let used_inodes = self.by_ino.len() as u32 - 1;
        put_le32(sb, soff::INODES_COUNT, inodes);
        put_le32(sb, soff::BLOCKS_COUNT_LO, self.blocks as u32);
        put_le32(sb, soff::BLOCKS_COUNT_HI, (self.blocks >> 32) as u32);
        put_le32(sb, soff::FREE_BLOCKS_COUNT_LO, free_blocks as u32);
        put_le32(sb, soff::FREE_BLOCKS_COUNT_HI, (free_blocks >> 32) as u32);
        put_le32(sb, soff::FREE_INODES_COUNT, inodes - used_inodes);
        put_le32(sb, soff::LOG_BLOCK_SIZE, 2);
        put_le32(sb, soff::LOG_CLUSTER_SIZE, 2);
        put_le32(sb, soff::BLOCKS_PER_GROUP, BLOCKS_PER_GROUP as u32);
        put_le32(sb, soff::CLUSTERS_PER_GROUP, BLOCKS_PER_GROUP as u32);
        put_le32(sb, soff::INODES_PER_GROUP, self.inodes_per_group);
        put_le16(sb, soff::MAX_MNT_COUNT, 0xFFFF);
        put_le16(sb, soff::MAGIC, sbk::MAGIC);
        put_le16(sb, soff::STATE, sbk::STATE_VALID);
        put_le16(sb, soff::ERRORS, 1);
        put_le32(sb, soff::REV_LEVEL, 1);
        put_le32(sb, soff::FIRST_INO, LOST_FOUND);
        put_le16(sb, soff::INODE_SIZE, INODE_SIZE as u16);
        put_le16(sb, BLOCK_GROUP_NR, group_nr as u16);
        put_le32(sb, soff::FEATURE_COMPAT, COMPAT);
        put_le32(sb, soff::FEATURE_INCOMPAT, INCOMPAT);
        put_le32(sb, soff::FEATURE_RO_COMPAT, RO_COMPAT);
        sb[soff::UUID..soff::UUID + 16].copy_from_slice(&self.uuid);
        put_le16(sb, soff::DESC_SIZE, DESC_SIZE as u16);
        put_le16(sb, soff::MIN_EXTRA_ISIZE, EXTRA_ISIZE);
        put_le16(sb, soff::WANT_EXTRA_ISIZE, EXTRA_ISIZE);
        sb[soff::LOG_GROUPS_PER_FLEX] = 4;
        sb[soff::CHECKSUM_TYPE] = sbk::CHECKSUM_CRC32C;
        sbk::seal(sb);
    }

    fn table(&self, index: u64, out: &mut [u8; BLOCK]) {
        out.fill(0);
        let per_block = (BLOCK / DESC_SIZE) as u64;
        for (k, g) in (index * per_block..(index + 1) * per_block).enumerate() {
            let Some(desc) = self.descs.get(g as usize) else {
                break;
            };
            let raw = &mut out[k * DESC_SIZE..(k + 1) * DESC_SIZE];
            desc.encode(raw);
            group::seal(DescCsum::Crc32c { seed: self.seed }, g as u32, raw);
        }
    }

    fn inode(&self, node: &Node, raw: &mut [u8]) {
        raw.fill(0);
        put_le16(raw, ioff::MODE, node.kind.mode() | node.perm);
        let size = match node.kind {
            Kind::Directory => node.data_blocks() * BLOCK as u64,
            _ => node.size,
        };
        put_le32(raw, ioff::SIZE_LO, size as u32);
        put_le32(raw, ioff::SIZE_HIGH, (size >> 32) as u32);
        let links = match node.kind {
            Kind::Directory => {
                2 + node
                    .children
                    .iter()
                    .filter(|&&c| self.nodes[c].kind == Kind::Directory)
                    .count()
            }
            _ => 1,
        };
        put_le16(
            raw,
            ioff::LINKS_COUNT,
            links.min(usize::from(u16::MAX)) as u16,
        );
        let sectors = node.blocks() * (BLOCK as u64 / 512);
        put_le32(raw, ioff::BLOCKS_LO, sectors as u32);
        put_le16(raw, ioff::BLOCKS_HIGH, (sectors >> 32) as u16);
        put_le16(raw, ioff::EXTRA_ISIZE, EXTRA_ISIZE);
        let i_block = &mut raw[ioff::BLOCK..ioff::BLOCK + inode::BLOCK_BYTES];
        if node.fast_symlink() {
            i_block[..node.target.len()].copy_from_slice(&node.target);
        } else {
            put_le32(raw, ioff::FLAGS, inode::flags::EXTENTS);
            self.extent_root(
                node,
                &mut raw[ioff::BLOCK..ioff::BLOCK + inode::BLOCK_BYTES],
            );
        }
        inode::seal(self.seed, node.ino, raw);
    }

    fn extent_root(&self, node: &Node, root: &mut [u8]) {
        extent::init_root(root);
        if node.leaves.is_empty() {
            for (i, e) in node.extents.iter().enumerate() {
                extent::put_leaf(root, i, e);
            }
            extent::set_entries(root, node.extents.len() as u16);
            return;
        }
        extent::write_header(
            root,
            Header {
                entries: node.leaves.len() as u16,
                max: extent::ROOT_MAX,
                depth: 1,
            },
        );
        for (i, &leaf) in node.leaves.iter().enumerate() {
            extent::put_index(root, i, node.extents[i * leaf_max()].lblk, leaf);
        }
    }

    fn leaf(&self, node: &Node, index: usize, out: &mut [u8; BLOCK]) {
        out.fill(0);
        let extents = node
            .extents
            .chunks(leaf_max())
            .nth(index)
            .unwrap_or_default();
        extent::write_header(
            out,
            Header {
                entries: extents.len() as u16,
                max: leaf_max() as u16,
                depth: 0,
            },
        );
        for (i, e) in extents.iter().enumerate() {
            extent::put_leaf(out, i, e);
        }
        extent::seal(inode::seed(self.seed, node.ino, 0), out);
    }

    fn dir(&self, n: usize, index: u64, out: &mut [u8; BLOCK]) {
        out.fill(0);
        let blocks = dir_blocks(&self.nodes, n);
        let Some(entries) = blocks.get(index as usize) else {
            return;
        };
        let usable = dir::usable(BLOCK, true);
        let mut at = 0;
        for (k, (name, ino, kind)) in entries.iter().enumerate() {
            let len = if k + 1 == entries.len() {
                usable - at
            } else {
                rec_len(name.len())
            };
            put_le32(out, at, *ino);
            put_le16(out, at + 4, len as u16);
            out[at + 6] = name.len() as u8;
            out[at + 7] = *kind;
            out[at + 8..at + 8 + name.len()].copy_from_slice(name);
            at += len;
        }
        dir::write_tail(out);
        dir::seal(inode::seed(self.seed, self.nodes[n].ino, 0), out);
    }

    /// Every part's first block, in order.
    fn parts(&self) -> Vec<(u64, u64, Part)> {
        let mut parts = vec![(0, 1, Part::Super)];
        for i in 0..self.gdt_blocks {
            parts.push((1 + i, 1, Part::Table(i)));
        }
        for g in (1..self.groups).filter(|&g| has_backup(g, true)) {
            let start = u64::from(g) * BLOCKS_PER_GROUP;
            parts.push((start, 1, Part::Backup(g)));
            for i in 0..self.gdt_blocks {
                parts.push((start + 1 + i, 1, Part::Table(i)));
            }
        }
        let table_blocks = u64::from(self.inodes_per_group / INODES_PER_BLOCK);
        for g in 0..self.groups {
            parts.push((self.block_bitmaps[g as usize], 1, Part::BlockBitmap(g)));
            parts.push((self.inode_bitmaps[g as usize], 1, Part::InodeBitmap(g)));
            for i in 0..table_blocks {
                parts.push((self.inode_tables[g as usize] + i, 1, Part::Inodes(g, i)));
            }
        }
        for (n, node) in self.nodes.iter().enumerate() {
            for e in &node.extents {
                parts.push((e.pblk, u64::from(e.len), Part::Data(n, u64::from(e.lblk))));
            }
            for (i, &leaf) in node.leaves.iter().enumerate() {
                parts.push((leaf, 1, Part::Leaf(n, i)));
            }
        }
        for &(start, end) in &self.gaps {
            parts.push((start, end - start, Part::Free));
        }
        parts.sort_by_key(|&(start, _, _)| start);
        parts
    }

    /// Hand every block of the volume to `sink` in order. `fill` reads an
    /// entry's bytes: entry, offset and room, which it fills whole but past
    /// the end.
    pub fn write<E>(
        &self,
        fill: &mut dyn FnMut(usize, u64, &mut [u8]) -> Result<(), E>,
        sink: &mut dyn FnMut(&[u8; BLOCK]) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut block = Box::new([0u8; BLOCK]);
        for (_, len, part) in self.parts() {
            for i in 0..len {
                self.part(&part, i, &mut block, fill)?;
                sink(&block)?;
            }
        }
        Ok(())
    }

    fn part<E>(
        &self,
        part: &Part,
        i: u64,
        out: &mut [u8; BLOCK],
        fill: &mut dyn FnMut(usize, u64, &mut [u8]) -> Result<(), E>,
    ) -> Result<(), E> {
        out.fill(0);
        match *part {
            Part::Super => self.superblock(0, &mut out[1024..2048]),
            Part::Table(index) => self.table(index, out),
            Part::Backup(g) => self.superblock(g, &mut out[..superblock::SIZE]),
            Part::BlockBitmap(g) => self.block_bitmap(g, out),
            Part::InodeBitmap(g) => self.inode_bitmap(g, out),
            Part::Inodes(g, index) => {
                let first = g * self.inodes_per_group + index as u32 * INODES_PER_BLOCK + 1;
                for (k, raw) in out.chunks_exact_mut(INODE_SIZE).enumerate() {
                    if let Some(node) = self.node_of(first + k as u32) {
                        self.inode(node, raw);
                    }
                }
            }
            Part::Data(n, lblk) => {
                let node = &self.nodes[n];
                let file_block = lblk + i;
                match node.kind {
                    Kind::Directory => self.dir(n, file_block, out),
                    Kind::Symlink => out[..node.target.len()].copy_from_slice(&node.target),
                    Kind::File => {
                        let offset = file_block * BLOCK as u64;
                        let len = (node.size - offset).min(BLOCK as u64) as usize;
                        fill(node.entry, offset, &mut out[..len])?;
                    }
                }
            }
            Part::Leaf(n, index) => self.leaf(&self.nodes[n], index, out),
            Part::Free => {}
        }
        Ok(())
    }
}
