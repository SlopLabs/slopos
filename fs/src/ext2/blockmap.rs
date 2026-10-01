use super::Ext2Error;
use super::cache::{BlockCache, BlockOwner};
use super::ext2_alloc;
use super::extents;
use super::geometry::Ext2Geometry;
use super::ondisk::{Inode, Superblock};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;

const DIRECT_BLOCKS: u32 = 12;
const INDIRECT_IDX: u32 = 12;
const DINDIRECT_IDX: u32 = 13;
const TINDIRECT_IDX: u32 = 14;

/// A chain of offsets to traverse from the inode's block[] array.
#[derive(Debug)]
pub struct BlockPath {
    pub depth: u8,
    pub offsets: [u32; 4],
}

/// The largest size `inode`'s mapping, extent tree or block map, can address.
pub fn max_file_size(inode: &Inode, geom: &Ext2Geometry) -> u64 {
    let block_size = u64::from(geom.block_size());
    if inode.uses_extents() {
        return slopos_ext4_core::extent::MAX_BLOCKS * block_size;
    }
    block_map_reach(geom.ptrs_per_block(), geom.block_size())
}

/// Bytes twelve direct blocks and three levels of indirection address.
/// Saturating rather than checked: `ptrs_per_block` is `block_size / 4`, so
/// the product cannot overflow `u64` for any block size accepted here.
pub fn block_map_reach(ptrs_per_block: u32, block_size: u32) -> u64 {
    let n = ptrs_per_block as u64;
    let blocks = (DIRECT_BLOCKS as u64)
        .saturating_add(n)
        .saturating_add(n.saturating_mul(n))
        .saturating_add(n.saturating_mul(n).saturating_mul(n));
    // An ext2 file block number is 32 bits, which binds first on a large block
    // size: 1024³ already exceeds `u32::MAX`.
    blocks
        .min(u32::MAX as u64)
        .saturating_mul(block_size as u64)
}

pub fn block_to_path(file_block: FileBlock, ptrs_per_block: u32) -> Result<BlockPath, Ext2Error> {
    let fb = file_block.raw();
    let n = ptrs_per_block;

    if fb < DIRECT_BLOCKS {
        return Ok(BlockPath {
            depth: 1,
            offsets: [fb, 0, 0, 0],
        });
    }

    let fb = fb - DIRECT_BLOCKS;
    if fb < n {
        return Ok(BlockPath {
            depth: 2,
            offsets: [INDIRECT_IDX, fb, 0, 0],
        });
    }

    let fb = fb - n;
    let n2 = n.checked_mul(n).ok_or(Ext2Error::InvalidBlock)?;
    if fb < n2 {
        return Ok(BlockPath {
            depth: 3,
            offsets: [DINDIRECT_IDX, fb / n, fb % n, 0],
        });
    }

    let fb = fb - n2;
    let n3 = n2.checked_mul(n).ok_or(Ext2Error::InvalidBlock)?;
    if fb < n3 {
        return Ok(BlockPath {
            depth: 4,
            offsets: [TINDIRECT_IDX, fb / n2, (fb / n) % n, fb % n],
        });
    }

    // Past the triple-indirect reach. `InvalidRange`, not `InvalidBlock`: this
    // is a caller's offset, and the latter is classified as image damage — a
    // one-byte `pwrite` past the reach would latch the whole mount read-only.
    Err(Ext2Error::InvalidRange)
}

fn read_ptr(data: &[u8], idx: u32) -> BlockNum {
    let off = idx as usize * 4;
    BlockNum(u32::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn write_ptr(data: &mut [u8], idx: u32, block: BlockNum) {
    let off = idx as usize * 4;
    data[off..off + 4].copy_from_slice(&block.raw().to_le_bytes());
}

/// A block pointer the *image* chose, held to the volume it claims to be in.
///
/// `BlockCache::get_kind` turns a block number straight into a device offset,
/// so an unbounded pointer reads and writes outside the filesystem. A hole is
/// not a pointer, so zero passes through for the caller to read as one.
pub(super) fn checked_ptr(geom: &Ext2Geometry, block: BlockNum) -> Result<BlockNum, Ext2Error> {
    if !block.is_valid() {
        return Ok(BlockNum::ZERO);
    }
    geom.checked_owned_block(block.raw())
        .ok_or(Ext2Error::InvalidBlock)
}

/// Returns `BlockNum::ZERO` for holes (sparse file).
pub fn map_block(
    inode: &Inode,
    file_block: FileBlock,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    if inode.uses_extents() {
        return extents::map_block(inode, file_block, geom, cache, device, owner);
    }
    let path = block_to_path(file_block, geom.ptrs_per_block())?;

    let mut current = checked_ptr(geom, inode.block[path.offsets[0] as usize])?;
    if !current.is_valid() {
        return Ok(BlockNum::ZERO);
    }

    for level in 1..path.depth as usize {
        let block = cache.get_owned(current, device, owner)?;
        current = checked_ptr(geom, read_ptr(block.data(), path.offsets[level]))?;
        if !current.is_valid() {
            return Ok(BlockNum::ZERO);
        }
    }

    Ok(current)
}

/// The block holding `file_block`, allocated and zeroed if the file has none
/// there. Every block linked into the map is counted into `i_blocks`, even
/// when the call then fails, since a linked block is the inode's either way.
pub fn ensure_data_block(
    inode: &mut Inode,
    file_block: FileBlock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    if inode.uses_extents() {
        return extents::ensure_block(inode, file_block, cache, device, geom, superblock, owner);
    }
    let mut allocated = 0u32;
    let mapped = ensure_mapped(
        inode,
        file_block,
        cache,
        device,
        geom,
        superblock,
        owner,
        &mut allocated,
    );
    inode.blocks += u64::from(allocated) * u64::from(geom.block_size() / 512);
    mapped
}

#[allow(clippy::too_many_arguments)]
fn ensure_mapped(
    inode: &mut Inode,
    file_block: FileBlock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    owner: BlockOwner,
    allocated: &mut u32,
) -> Result<BlockNum, Ext2Error> {
    let path = block_to_path(file_block, geom.ptrs_per_block())?;

    if path.depth == 1 {
        let idx = path.offsets[0] as usize;
        let existing = checked_ptr(geom, inode.block[idx])?;
        if existing.is_valid() {
            return Ok(existing);
        }
        let new_block = allocate_zeroed(BlockKind::Data, geom, superblock, cache, device, owner)?;
        inode.block[idx] = new_block;
        *allocated += 1;
        return Ok(new_block);
    }

    let top_idx = path.offsets[0] as usize;
    let mut current_indirect = checked_ptr(geom, inode.block[top_idx])?;
    if !current_indirect.is_valid() {
        let new_block = allocate_zeroed(BlockKind::Map, geom, superblock, cache, device, owner)?;
        inode.block[top_idx] = new_block;
        current_indirect = new_block;
        *allocated += 1;
    }

    for level in 1..path.depth as usize - 1 {
        let child = {
            let block = cache.get_owned(current_indirect, device, owner)?;
            checked_ptr(geom, read_ptr(block.data(), path.offsets[level]))?
        };
        current_indirect = if child.is_valid() {
            child
        } else {
            let at = (current_indirect, path.offsets[level]);
            let new_block = link_new(BlockKind::Map, at, geom, superblock, cache, device, owner)?;
            *allocated += 1;
            new_block
        };
    }

    let data_idx = path.offsets[path.depth as usize - 1];
    let existing = {
        let block = cache.get_owned(current_indirect, device, owner)?;
        checked_ptr(geom, read_ptr(block.data(), data_idx))?
    };

    if existing.is_valid() {
        return Ok(existing);
    }

    let at = (current_indirect, data_idx);
    let new_data = link_new(BlockKind::Data, at, geom, superblock, cache, device, owner)?;
    *allocated += 1;
    Ok(new_data)
}

/// A fresh block linked at entry `at.1` of map block `at.0`, or given back.
fn link_new(
    kind: BlockKind,
    at: (BlockNum, u32),
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    let block = allocate_zeroed(kind, geom, superblock, cache, device, owner)?;
    let linked = cache
        .get_owned(at.0, device, owner)
        .map(|mut parent| write_ptr(parent.data_mut(), at.1, block));
    if let Err(e) = linked {
        ext2_alloc::free_block(block, geom, superblock, cache, device, owner)?;
        return Err(e);
    }
    Ok(block)
}

#[derive(Clone, Copy)]
enum BlockKind {
    Data,
    Map,
}

/// A fresh block, zeroed in the cache before anything maps it so no file sees
/// its previous owner's bytes; given back if it cannot be zeroed.
fn allocate_zeroed(
    kind: BlockKind,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    let block = ext2_alloc::allocate_block(geom, superblock, cache, device, owner)?;
    let zeroed = match kind {
        BlockKind::Data => cache.get_zero_data(block, device, owner).map(drop),
        BlockKind::Map => cache.get_zero_owned(block, device, owner).map(drop),
    };
    if let Err(e) = zeroed {
        ext2_alloc::free_block(block, geom, superblock, cache, device, owner)?;
        return Err(e);
    }
    Ok(block)
}
