//! The extent tree of an inode under `EXTENTS_FL`, kept in the block cache.
//!
//! The tree algorithm is `slopos_ext4_core::extent`'s; this is the store it
//! runs over: tree blocks are cached metadata of the inode they belong to,
//! checksummed with its seed, allocated and freed through the group bitmaps.

use super::Ext2Error;
use super::cache::{BlockCache, BlockOwner};
use super::ext2_alloc;
use super::geometry::Ext2Geometry;
use super::ondisk::{Inode, Superblock};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;
use slopos_ext4_core::extent::{self, Extent, ExtentError, Node, Store};
use slopos_ext4_core::inode as ext4_inode;

impl From<ExtentError> for Ext2Error {
    fn from(err: ExtentError) -> Self {
        match err {
            // A tree five levels deep maps more blocks than a 32-bit block
            // number reaches; running out of depth is running out of room.
            ExtentError::TooDeep => Ext2Error::NoSpace,
            ExtentError::Corrupt | ExtentError::Overlap => Ext2Error::InvalidBlock,
        }
    }
}

struct TreeStore<'x> {
    root: [u8; ext4_inode::BLOCK_BYTES],
    root_dirty: bool,
    cache: &'x mut BlockCache,
    device: &'x dyn BlockDevice,
    geom: &'x Ext2Geometry,
    /// `None` for a lookup, which never allocates or frees.
    superblock: Option<&'x mut Superblock>,
    owner: BlockOwner,
    /// The inode's checksum seed, when the volume carries checksums.
    seed: Option<u32>,
    /// Blocks the inode gained and lost through this store, nodes and data.
    gained: u32,
    lost: u32,
}

impl<'x> TreeStore<'x> {
    fn new(
        inode: &Inode,
        cache: &'x mut BlockCache,
        device: &'x dyn BlockDevice,
        geom: &'x Ext2Geometry,
        superblock: Option<&'x mut Superblock>,
        owner: BlockOwner,
    ) -> Self {
        let ino = owner.charged_inode().unwrap_or(0);
        Self {
            root: inode.block_bytes(),
            root_dirty: false,
            cache,
            device,
            geom,
            superblock,
            owner,
            seed: geom
                .csum_seed()
                .map(|fs_seed| ext4_inode::seed(fs_seed, ino, inode.generation)),
            gained: 0,
            lost: 0,
        }
    }

    fn block(&self, raw: u64) -> Result<BlockNum, Ext2Error> {
        let raw = u32::try_from(raw).map_err(|_| Ext2Error::InvalidBlock)?;
        self.geom
            .checked_owned_block(raw)
            .ok_or(Ext2Error::InvalidBlock)
    }

    fn release(&mut self, raw: u64) -> Result<(), Ext2Error> {
        let block = self.block(raw)?;
        let superblock = self.superblock.as_deref_mut().ok_or(Ext2Error::ReadOnly)?;
        ext2_alloc::free_block(
            block,
            self.geom,
            superblock,
            self.cache,
            self.device,
            self.owner,
        )?;
        self.cache.invalidate(block);
        self.lost += 1;
        Ok(())
    }

    fn put_root_back(self, inode: &mut Inode) {
        if self.root_dirty {
            inode.set_block_bytes(&self.root);
        }
    }
}

impl Store for TreeStore<'_> {
    type Error = Ext2Error;

    fn block_size(&self) -> usize {
        self.geom.block_size() as usize
    }

    fn read<R>(&mut self, node: Node, f: impl FnOnce(&[u8]) -> R) -> Result<R, Ext2Error> {
        let raw = match node {
            Node::Root => return Ok(f(&self.root)),
            Node::Block(raw) => raw,
        };
        let block = self.block(raw)?;
        let mut cached = self.cache.get_owned(block, self.device, self.owner)?;
        if let Some(seed) = self.seed
            && !cached.checked()
        {
            if !extent::verify(seed, cached.data()) {
                return Err(Ext2Error::BadChecksum);
            }
            cached.set_checked();
        }
        Ok(f(cached.data()))
    }

    fn write<R>(&mut self, node: Node, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, Ext2Error> {
        let raw = match node {
            Node::Root => {
                self.root_dirty = true;
                return Ok(f(&mut self.root));
            }
            Node::Block(raw) => raw,
        };
        let block = self.block(raw)?;
        let mut cached = self.cache.get_owned(block, self.device, self.owner)?;
        let data = cached.data_mut();
        let out = f(data);
        if let Some(seed) = self.seed {
            extent::seal(seed, data);
        }
        Ok(out)
    }

    fn alloc_node(&mut self, goal: u64) -> Result<u64, Ext2Error> {
        let superblock = self.superblock.as_deref_mut().ok_or(Ext2Error::ReadOnly)?;
        let goal = BlockNum(u32::try_from(goal).unwrap_or(0));
        let block = ext2_alloc::allocate_block_near(
            goal,
            self.geom,
            superblock,
            self.cache,
            self.device,
            self.owner,
        )?;
        drop(self.cache.get_zero_owned(block, self.device, self.owner)?);
        self.gained += 1;
        Ok(u64::from(block.raw()))
    }

    fn free_node(&mut self, block: u64) -> Result<(), Ext2Error> {
        self.release(block)
    }

    fn free_data(&mut self, first: u64, count: u32) -> Result<(), Ext2Error> {
        for k in 0..u64::from(count) {
            self.release(first + k)?;
        }
        Ok(())
    }
}

/// An empty tree in `i_block`, as every new file under `extents` starts.
pub fn init_root(inode: &mut Inode) {
    let mut root = [0u8; ext4_inode::BLOCK_BYTES];
    extent::init_root(&mut root);
    inode.set_block_bytes(&root);
}

/// Where `file_block` lives, or `BlockNum::ZERO` for a hole or a block that
/// was allocated but never written, both of which read as zeros.
pub fn map_block(
    inode: &Inode,
    file_block: FileBlock,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    Ok(map_run(inode, file_block, geom, cache, device, owner)?.0)
}

/// [`map_block`], with how many blocks after it continue on the device in
/// the same extent.
pub fn map_run(
    inode: &Inode,
    file_block: FileBlock,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<(BlockNum, u32), Ext2Error> {
    let mut store = TreeStore::new(inode, cache, device, geom, None, owner);
    match extent::lookup(&mut store, file_block.raw())? {
        Some(m) if !m.unwritten => {
            let block = store.block(m.pblk)?;
            store.block(m.pblk + u64::from(m.len) - 1)?;
            Ok((block, m.len))
        }
        _ => Ok((BlockNum::ZERO, 0)),
    }
}

/// The block holding `file_block`, allocated and zeroed if it was a hole or
/// never written. `i_blocks` and the root are updated on failure too: a split
/// that finished belongs to the tree.
pub fn ensure_block(
    inode: &mut Inode,
    file_block: FileBlock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    let fallback_goal = inode_goal(geom, owner);
    let mut store = TreeStore::new(inode, cache, device, geom, Some(superblock), owner);
    let mapped = store.ensure(file_block.raw(), fallback_goal);
    let sectors = u64::from(geom.block_size() / 512);
    let (gained, lost) = (u64::from(store.gained), u64::from(store.lost));
    store.put_root_back(inode);
    inode.blocks = (inode.blocks + gained * sectors).saturating_sub(lost * sectors);
    mapped
}

impl TreeStore<'_> {
    fn ensure(&mut self, lblk: u32, fallback_goal: u64) -> Result<BlockNum, Ext2Error> {
        match extent::lookup(self, lblk)? {
            Some(m) if !m.unwritten => self.block(m.pblk),
            Some(m) => {
                let block = self.block(m.pblk)?;
                drop(self.cache.get_zero_data(block, self.device, self.owner)?);
                extent::mark_written(self, lblk, m.pblk)?;
                Ok(block)
            }
            None => {
                let goal = extent::goal(self, lblk)?.unwrap_or(fallback_goal);
                let block = self.alloc_data(goal)?;
                let run = Extent {
                    lblk,
                    len: 1,
                    pblk: u64::from(block.raw()),
                    unwritten: false,
                };
                if let Err(e) = extent::insert(self, run, goal) {
                    self.release(run.pblk)?;
                    return Err(e);
                }
                Ok(block)
            }
        }
    }

    /// A data block, zeroed in the cache before anything maps it.
    fn alloc_data(&mut self, goal: u64) -> Result<BlockNum, Ext2Error> {
        let block = {
            let superblock = self.superblock.as_deref_mut().ok_or(Ext2Error::ReadOnly)?;
            let goal = BlockNum(u32::try_from(goal).unwrap_or(0));
            ext2_alloc::allocate_block_near(
                goal,
                self.geom,
                superblock,
                self.cache,
                self.device,
                self.owner,
            )?
        };
        self.gained += 1;
        let zeroed = self
            .cache
            .get_zero_data(block, self.device, self.owner)
            .map(drop);
        if let Err(e) = zeroed {
            self.release(u64::from(block.raw()))?;
            return Err(e);
        }
        Ok(block)
    }
}

/// A file with no blocks yet starts in its inode's own group.
fn inode_goal(geom: &Ext2Geometry, owner: BlockOwner) -> u64 {
    let ino = owner.charged_inode().unwrap_or(0);
    let group = ino.saturating_sub(1) / geom.inodes_per_group().max(1);
    u64::from(geom.first_data_block().raw()) + u64::from(group) * u64::from(geom.blocks_per_group())
}

/// Free every block at or past `from`, and the tree nodes left empty, taking
/// them off `i_blocks`.
pub fn truncate(
    inode: &mut Inode,
    from: FileBlock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    let mut store = TreeStore::new(inode, cache, device, geom, Some(superblock), owner);
    extent::truncate(&mut store, from.raw())?;
    let lost = u64::from(store.lost) * u64::from(geom.block_size() / 512);
    store.put_root_back(inode);
    inode.blocks = inode.blocks.saturating_sub(lost);
    Ok(())
}

/// Every run of the file, in order; `f` answers whether to go on.
pub fn for_each_run(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    owner: BlockOwner,
    f: &mut dyn FnMut(Extent) -> bool,
) -> Result<(), Ext2Error> {
    let mut store = TreeStore::new(inode, cache, device, geom, None, owner);
    extent::for_each(&mut store, f)
}
