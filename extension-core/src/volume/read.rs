//! Reading a volume, refusing whatever the kernel could serve differently or
//! not at all: a feature or inode flag that changes what a block means, an
//! inode of another kind, generation or mapping than the writer gives, a
//! block outside the volume's own, a directory that holds a name twice or
//! whose `.` or `..` lies, an inode at two names, sizes past the volume's,
//! and a checksum that does not hold.

use alloc::boxed::Box;
use alloc::collections::{BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use slopos_abi::fs::USER_PATH_MAX;
use slopos_ext4_core::bytes::{le16, le32};
use slopos_ext4_core::crc::crc32c;
use slopos_ext4_core::dir;
use slopos_ext4_core::extent::{self, ExtentError, Node};
use slopos_ext4_core::group::{self, Desc, DescCsum};
use slopos_ext4_core::inode::{self, off as ioff};
use slopos_ext4_core::superblock::{self as sbk, incompat, off as soff, ro_compat};

use super::{
    BLOCK, BLOCKS_PER_GROUP, DESC_SIZE, FAST_SYMLINK_MAX, INCOMPAT, INODE_SIZE, Kind, LOST_FOUND,
    READ_COMPAT, RO_COMPAT, ROOT, valid_name,
};

/// Where a volume's blocks come from: the data of a verified image.
pub trait Blocks {
    type Error;

    fn read(&mut self, block: u64, out: &mut [u8; BLOCK]) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeError<E> {
    Read(E),
    /// Not an ext4 volume this reads: the reason.
    Unsupported(&'static str),
    /// Inconsistent with itself: the reason, and the inode when one is.
    Damaged(&'static str, u32),
    /// More entries, or longer paths, than a reader keeps.
    TooMany,
}

impl<E: core::fmt::Display> core::fmt::Display for VolumeError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VolumeError::Read(e) => write!(f, "{e}"),
            VolumeError::Unsupported(why) => write!(f, "not a volume an extension holds: {why}"),
            VolumeError::Damaged(why, 0) => write!(f, "a damaged volume: {why}"),
            VolumeError::Damaged(why, ino) => write!(f, "a damaged volume: inode {ino}: {why}"),
            VolumeError::TooMany => {
                f.write_str("more entries or longer paths than an extension may hold")
            }
        }
    }
}

impl<E> From<ExtentError> for VolumeError<E> {
    fn from(_: ExtentError) -> Self {
        VolumeError::Damaged("an extent tree", 0)
    }
}

#[derive(Clone, Debug)]
pub struct Inode {
    pub ino: u32,
    pub mode: u16,
    pub size: u64,
    flags: u32,
    i_block: [u8; inode::BLOCK_BYTES],
}

impl Inode {
    pub fn kind(&self) -> Option<Kind> {
        Kind::of_mode(self.mode)
    }
}

/// One thing a walk found, by its path from the volume's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub path: String,
    pub ino: u32,
    pub kind: Kind,
    /// Its mode, type and permission bits.
    pub mode: u16,
    pub size: u64,
}

pub struct Volume<B> {
    blocks: B,
    inodes_per_group: u32,
    inodes_count: u32,
    inode_size: usize,
    /// `metadata_csum`'s seed.
    seed: u32,
    inode_tables: Vec<u64>,
    /// What its directories, and its files, may take together: its own
    /// size, which only blocks named more than once can pass.
    bytes: u64,
    /// The blocks a file or a tree may name, past the superblock and the
    /// descriptor tables, as the kernel holds them.
    owned: core::ops::Range<u64>,
    block: Box<[u8; BLOCK]>,
}

type Got<T, E> = Result<T, VolumeError<E>>;

impl<B: Blocks> Volume<B> {
    /// The volume `blocks` holds, `data_blocks` long.
    pub fn open(mut blocks: B, data_blocks: u64) -> Got<Self, B::Error> {
        let mut block = Box::new([0u8; BLOCK]);
        blocks.read(0, &mut block).map_err(VolumeError::Read)?;
        let sb = &block[1024..2048];
        let unsupported = |why| Err(VolumeError::Unsupported(why));
        if le16(sb, soff::MAGIC) != sbk::MAGIC || le32(sb, soff::REV_LEVEL) < 1 {
            return unsupported("no ext4 superblock");
        }
        if le32(sb, soff::LOG_BLOCK_SIZE) != 2 || le32(sb, soff::LOG_CLUSTER_SIZE) != 2 {
            return unsupported("blocks other than 4 KiB");
        }
        let compat = le32(sb, soff::FEATURE_COMPAT);
        let features = le32(sb, soff::FEATURE_INCOMPAT);
        let ro = le32(sb, soff::FEATURE_RO_COMPAT);
        if compat & !READ_COMPAT != 0 || features & !INCOMPAT != 0 || ro & !RO_COMPAT != 0 {
            return unsupported("a feature that changes what its blocks mean");
        }
        if features & incompat::FILETYPE == 0 {
            return unsupported("directory entries without their types");
        }
        if ro & ro_compat::READONLY == 0 {
            return unsupported("not marked read-only");
        }
        if ro & ro_compat::METADATA_CSUM == 0 {
            return unsupported("no metadata checksums");
        }
        if !sbk::verify(sb) {
            return Err(VolumeError::Damaged("the superblock's checksum", 0));
        }
        let per_group = u64::from(le32(sb, soff::BLOCKS_PER_GROUP));
        let inodes_per_group = le32(sb, soff::INODES_PER_GROUP);
        let inode_size = usize::from(le16(sb, soff::INODE_SIZE));
        let desc_size = usize::from(sbk::desc_size(sb));
        let reserved = u64::from(le16(sb, soff::RESERVED_GDT_BLOCKS));
        if sb[soff::CHECKSUM_TYPE] != sbk::CHECKSUM_CRC32C {
            return unsupported("checksums other than CRC32C");
        }
        let blocks_count = sbk::blocks_count(sb);
        if blocks_count != data_blocks
            || le32(sb, soff::FIRST_DATA_BLOCK) != 0
            || per_group != BLOCKS_PER_GROUP
            || u64::from(le32(sb, soff::CLUSTERS_PER_GROUP)) != per_group
            || inodes_per_group == 0
            || inodes_per_group > 8 * BLOCK as u32
            || inode_size != INODE_SIZE
            || desc_size != DESC_SIZE
            || le32(sb, soff::FIRST_INO) != LOST_FOUND
        {
            return Err(VolumeError::Damaged("the superblock's geometry", 0));
        }
        let groups = blocks_count.div_ceil(per_group);
        if u64::from(le32(sb, soff::INODES_COUNT)) != groups * u64::from(inodes_per_group) {
            return Err(VolumeError::Damaged("the superblock's inode count", 0));
        }
        let seed = crc32c(!0, &sb[soff::UUID..soff::UUID + 16]);
        let kind = DescCsum::Crc32c { seed };
        let mut inode_tables = Vec::new();
        let per_block = (BLOCK / desc_size) as u64;
        let table_blocks = (u64::from(inodes_per_group) * inode_size as u64).div_ceil(BLOCK as u64);
        let owned = 1 + groups.div_ceil(per_block) + reserved..blocks_count;
        for g in 0..groups {
            if g % per_block == 0 {
                blocks
                    .read(1 + g / per_block, &mut block)
                    .map_err(VolumeError::Read)?;
            }
            let at = (g % per_block) as usize * desc_size;
            let raw = &block[at..at + desc_size];
            if !group::verify(kind, g as u32, raw) {
                return Err(VolumeError::Damaged("a group descriptor's checksum", 0));
            }
            let desc = Desc::parse(raw);
            let last = desc.inode_table.checked_add(table_blocks - 1);
            let held = [desc.block_bitmap, desc.inode_bitmap, desc.inode_table]
                .into_iter()
                .chain(last)
                .all(|block| owned.contains(&block));
            if last.is_none() || !held {
                return Err(VolumeError::Damaged("a group's inode table", 0));
            }
            inode_tables.push(desc.inode_table);
        }
        Ok(Self {
            blocks,
            inodes_per_group,
            inodes_count: groups as u32 * inodes_per_group,
            inode_size,
            seed,
            inode_tables,
            bytes: blocks_count * BLOCK as u64,
            owned,
            block,
        })
    }

    /// Where its blocks come from.
    pub fn blocks(&self) -> &B {
        &self.blocks
    }

    fn read_block(&mut self, block: u64) -> Got<(), B::Error> {
        self.blocks
            .read(block, &mut self.block)
            .map_err(VolumeError::Read)
    }

    pub fn inode(&mut self, ino: u32) -> Got<Inode, B::Error> {
        if ino == 0 || ino > self.inodes_count {
            return Err(VolumeError::Damaged("an inode number past the table", ino));
        }
        let index = u64::from(ino - 1);
        let table = self.inode_tables[(index / u64::from(self.inodes_per_group)) as usize];
        let at = (index % u64::from(self.inodes_per_group)) * self.inode_size as u64;
        self.read_block(table + at / BLOCK as u64)?;
        let at = (at % BLOCK as u64) as usize;
        let raw = &self.block[at..at + self.inode_size];
        if !inode::verify(self.seed, ino, raw) {
            return Err(VolumeError::Damaged("its checksum", ino));
        }
        let mut i_block = [0; inode::BLOCK_BYTES];
        i_block.copy_from_slice(&raw[ioff::BLOCK..ioff::BLOCK + inode::BLOCK_BYTES]);
        let mode = le16(raw, ioff::MODE);
        let high = match Kind::of_mode(mode) {
            Some(Kind::File) => u64::from(le32(raw, ioff::SIZE_HIGH)) << 32,
            _ => 0,
        };
        let found = Inode {
            ino,
            mode,
            size: u64::from(le32(raw, ioff::SIZE_LO)) | high,
            flags: le32(raw, ioff::FLAGS),
            i_block,
        };
        if le32(raw, ioff::GENERATION) != 0 {
            return Err(VolumeError::Unsupported(
                "an inode of a generation the writer never gives",
            ));
        }
        let refused = inode::flags::INDEX
            | inode::flags::ENCRYPT
            | inode::flags::VERITY
            | inode::flags::EA_INODE
            | inode::flags::INLINE_DATA
            | inode::flags::CASEFOLD;
        if found.kind().is_none() || found.flags & refused != 0 {
            return Err(VolumeError::Unsupported(
                "an inode of a kind or with a flag it does not read",
            ));
        }
        let mapped = found.flags & inode::flags::EXTENTS != 0;
        if mapped == self.is_fast_symlink(&found) {
            return Err(VolumeError::Unsupported("a file not mapped by extents"));
        }
        Ok(found)
    }

    fn is_fast_symlink(&self, inode: &Inode) -> bool {
        inode.kind() == Some(Kind::Symlink)
            && inode.size > 0
            && inode.size as usize <= FAST_SYMLINK_MAX
    }

    /// The block holding logical block `lblk` of `inode`, `None` for a hole
    /// or an unwritten extent, which read as zeros.
    fn map(&mut self, inode: &Inode, lblk: u32) -> Got<Option<u64>, B::Error> {
        let mut store = Tree {
            volume: self,
            root: inode.i_block,
            ino: inode.ino,
        };
        let found = extent::lookup(&mut store, lblk)?;
        if let Some(m) = found {
            let last = m.pblk.checked_add(u64::from(m.len).saturating_sub(1));
            if !self.owned.contains(&m.pblk) || !last.is_some_and(|b| self.owned.contains(&b)) {
                return Err(VolumeError::Damaged(
                    "an extent outside the volume's blocks",
                    inode.ino,
                ));
            }
        }
        Ok(found.filter(|m| !m.unwritten).map(|m| m.pblk))
    }

    /// Up to `buf.len()` bytes of `inode` from `offset`; answers how many.
    pub fn read(&mut self, inode: &Inode, offset: u64, buf: &mut [u8]) -> Got<usize, B::Error> {
        if inode.kind() == Some(Kind::Directory) {
            return Err(VolumeError::Damaged("read as a file", inode.ino));
        }
        if self.is_fast_symlink(inode) {
            let target = &inode.i_block[..inode.size as usize];
            let start = (offset as usize).min(target.len());
            let n = (target.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&target[start..start + n]);
            return Ok(n);
        }
        let end = inode.size.min(offset.saturating_add(buf.len() as u64));
        let mut at = offset;
        while at < end {
            let lblk = u32::try_from(at / BLOCK as u64)
                .map_err(|_| VolumeError::Damaged("a size past the extent tree", inode.ino))?;
            let within = (at % BLOCK as u64) as usize;
            let n = ((end - at) as usize).min(BLOCK - within);
            let out = &mut buf[(at - offset) as usize..][..n];
            match self.map(inode, lblk)? {
                Some(block) => {
                    self.read_block(block)?;
                    out.copy_from_slice(&self.block[within..within + n]);
                }
                None if inode.kind() == Some(Kind::Symlink) => {
                    return Err(VolumeError::Damaged("a symlink with a hole", inode.ino));
                }
                None => out.fill(0),
            }
            at += n as u64;
        }
        Ok((end.saturating_sub(offset)) as usize)
    }

    /// The whole of `inode`, refused past `max` bytes.
    pub fn read_all(&mut self, inode: &Inode, max: u64) -> Got<Vec<u8>, B::Error> {
        if inode.size > max {
            return Err(VolumeError::TooMany);
        }
        let mut bytes = vec![0; inode.size as usize];
        self.read(inode, 0, &mut bytes)?;
        Ok(bytes)
    }

    /// The names `dir` holds and their inodes, once its `.` names it and its
    /// `..` names `parent`, refused past `room` of them.
    pub fn entries(
        &mut self,
        dir: &Inode,
        parent: u32,
        room: usize,
    ) -> Got<Vec<(Vec<u8>, u32, u8)>, B::Error> {
        let ino = dir.ino;
        let damaged = |why| Err(VolumeError::Damaged(why, ino));
        if dir.kind() != Some(Kind::Directory)
            || !dir.size.is_multiple_of(BLOCK as u64)
            || dir.size == 0
        {
            return damaged("a directory's size");
        }
        let mut out = Vec::new();
        let mut names = BTreeSet::new();
        let mut seen = 0;
        for lblk in 0..dir.size / BLOCK as u64 {
            let Some(block) = self.map(dir, lblk as u32)? else {
                return damaged("a hole in a directory");
            };
            self.read_block(block)?;
            let tail = dir::has_tail(&self.block[..]);
            if !tail || !dir::verify(inode::seed(self.seed, ino, 0), &self.block[..]) {
                return damaged("a directory block's checksum");
            }
            let end = dir::usable(BLOCK, true);
            let mut at = 0;
            while at < end {
                if at + 12 > end {
                    return damaged("a directory entry's length");
                }
                let child = le32(&self.block[..], at);
                let rec_len = usize::from(le16(&self.block[..], at + 4));
                let name_len = usize::from(self.block[at + 6]);
                if rec_len < 12
                    || !rec_len.is_multiple_of(4)
                    || at + rec_len > end
                    || 8 + name_len > rec_len
                {
                    return damaged("a directory entry's length");
                }
                let name = self.block[at + 8..at + 8 + name_len].to_vec();
                let kind = self.block[at + 7];
                at += rec_len;
                if child == 0 {
                    continue;
                }
                let expected: Option<(&[u8], u32)> = match seen {
                    0 => Some((b".", ino)),
                    1 => Some((b"..", parent)),
                    _ => None,
                };
                seen += 1;
                match expected {
                    Some((want, target)) if name == want && child == target => continue,
                    Some(_) => return damaged("a directory whose `.` or `..` lies"),
                    None => {}
                }
                if !(LOST_FOUND..=self.inodes_count).contains(&child) {
                    return damaged("an entry naming a reserved inode or none");
                }
                if out.len() == room {
                    return Err(VolumeError::TooMany);
                }
                if !valid_name(&name) || !names.insert(name.clone()) {
                    return damaged("a name a directory holds twice or cannot hold");
                }
                out.push((name, child, kind));
            }
        }
        if seen < 2 {
            return damaged("a directory without `.` and `..`");
        }
        Ok(out)
    }

    /// Every directory, file and symlink beneath the root, parents first:
    /// at most `max` of them, each path at most `USER_PATH_MAX` bytes and
    /// all of them `bytes` together.
    pub fn walk(&mut self, max: usize, bytes: usize) -> Got<Vec<Found>, B::Error> {
        let root = self.inode(ROOT)?;
        if root.kind() != Some(Kind::Directory) {
            return Err(VolumeError::Damaged("a root that is no directory", ROOT));
        }
        let mut found = Vec::new();
        let mut spent = 0usize;
        let mut listed = 0u64;
        let mut held = 0u64;
        let mut reached = BTreeSet::from([ROOT]);
        let mut dirs = VecDeque::from([(root, ROOT, String::new())]);
        while let Some((dir, parent, path)) = dirs.pop_front() {
            listed = listed.saturating_add(dir.size);
            if listed > self.bytes {
                return Err(VolumeError::Damaged(
                    "directories larger than the volume",
                    dir.ino,
                ));
            }
            for (name, ino, kind) in self.entries(&dir, parent, max - found.len())? {
                if ino < LOST_FOUND {
                    return Err(VolumeError::Damaged(
                        "an entry naming a reserved inode",
                        ino,
                    ));
                }
                if !reached.insert(ino) {
                    return Err(VolumeError::Unsupported("an inode at two names"));
                }
                if found.len() == max {
                    return Err(VolumeError::TooMany);
                }
                let inode = self.inode(ino)?;
                let Some(is) = inode.kind().filter(|k| k.dirent() == kind) else {
                    return Err(VolumeError::Damaged(
                        "an entry that names another type",
                        ino,
                    ));
                };
                if is == Kind::File {
                    held = inode
                        .size
                        .checked_next_multiple_of(BLOCK as u64)
                        .and_then(|size| held.checked_add(size))
                        .filter(|&held| held <= self.bytes)
                        .ok_or(VolumeError::Damaged("files larger than the volume", ino))?;
                }
                let name = String::from_utf8(name)
                    .map_err(|_| VolumeError::Unsupported("a name that is not UTF-8"))?;
                let len = path.len() + 1 + name.len();
                spent = spent.saturating_add(len);
                if len > USER_PATH_MAX || spent > bytes {
                    return Err(VolumeError::TooMany);
                }
                let child = alloc::format!("{path}/{name}");
                found.push(Found {
                    path: child.clone(),
                    ino,
                    kind: is,
                    mode: inode.mode,
                    size: inode.size,
                });
                if is == Kind::Directory {
                    dirs.push_back((inode, dir.ino, child));
                }
            }
        }
        Ok(found)
    }
}

/// An inode's extent tree as ext4-core's algorithm reads one.
struct Tree<'v, B> {
    volume: &'v mut Volume<B>,
    root: [u8; inode::BLOCK_BYTES],
    ino: u32,
}

impl<B: Blocks> extent::Store for Tree<'_, B> {
    type Error = VolumeError<B::Error>;

    fn block_size(&self) -> usize {
        BLOCK
    }

    fn read<R>(&mut self, node: Node, f: impl FnOnce(&[u8]) -> R) -> Got<R, B::Error> {
        match node {
            Node::Root => Ok(f(&self.root)),
            Node::Block(block) => {
                if !self.volume.owned.contains(&block) {
                    return Err(VolumeError::Damaged(
                        "an extent block outside the volume's blocks",
                        self.ino,
                    ));
                }
                self.volume.read_block(block)?;
                let seed = inode::seed(self.volume.seed, self.ino, 0);
                if !extent::verify(seed, &self.volume.block[..]) {
                    return Err(VolumeError::Damaged("an extent block's checksum", self.ino));
                }
                Ok(f(&self.volume.block[..]))
            }
        }
    }

    fn write<R>(&mut self, _: Node, _: impl FnOnce(&mut [u8]) -> R) -> Got<R, B::Error> {
        Err(VolumeError::Unsupported("a write"))
    }

    fn alloc_node(&mut self, _: u64) -> Got<u64, B::Error> {
        Err(VolumeError::Unsupported("a write"))
    }

    fn free_node(&mut self, _: u64) -> Got<(), B::Error> {
        Err(VolumeError::Unsupported("a write"))
    }

    fn free_data(&mut self, _: u64, _: u32) -> Got<(), B::Error> {
        Err(VolumeError::Unsupported("a write"))
    }
}
