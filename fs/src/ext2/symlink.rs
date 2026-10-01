use super::Ext2Error;
use super::blockmap;
use super::cache::{BlockCache, BlockOwner};
use super::geometry::Ext2Geometry;
use super::ondisk::{EXT4_EXTENTS_FL, FAST_SYMLINK_MAX, Inode, MODE_SYMLINK};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;
use slopos_ext4_core::extent::{self, Extent};

/// Whether `target` needs a data block, or fits inline in `i_block` as a fast
/// symlink. The caller allocates, because the allocator wants the same `&mut
/// BlockCache` this function does.
pub fn symlink_needs_block(target: &[u8]) -> bool {
    target.len() > FAST_SYMLINK_MAX
}

/// Create a symlink inode pointing to `target`. The caller must write it to
/// disk and add the directory entry.
///
/// `data_block` must be `Some` exactly when [`symlink_needs_block`] says so;
/// a mismatch is `InvalidBlock` rather than a silently truncated target.
/// `extents` maps a slow symlink's block with an extent, as every new file
/// on such a volume is.
pub fn create_symlink_inode(
    target: &[u8],
    block_size: u32,
    data_block: Option<BlockNum>,
    extents: bool,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<Inode, Ext2Error> {
    if target.is_empty() {
        return Err(Ext2Error::PathNotFound);
    }
    // The slow form stores the target in one block, so a longer target has
    // nowhere to go: ext2 has no multi-block symlink.
    if target.len() > block_size as usize {
        return Err(Ext2Error::NameTooLong);
    }
    if symlink_needs_block(target) != data_block.is_some() {
        return Err(Ext2Error::InvalidRange);
    }

    let mut inode = Inode {
        mode: MODE_SYMLINK | 0o777,
        size: target.len() as u64,
        links_count: 1,
        ..Inode::EMPTY
    };

    match data_block {
        None => {
            // Fast symlink: the target lives in inode.block[] itself, 15 × u32.
            let block_bytes =
                slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(&mut inode.block[..]);
            block_bytes[..target.len()].copy_from_slice(target);
        }
        Some(data_block) => {
            let mut blk = cache.get_zero_data(data_block, device, owner)?;
            let data = blk.data_mut();
            data[..target.len()].copy_from_slice(target);
            if extents {
                let mut root = inode.block_bytes();
                extent::init_root_with(
                    &mut root,
                    &Extent {
                        lblk: 0,
                        len: 1,
                        pblk: u64::from(data_block.raw()),
                        unwritten: false,
                    },
                );
                inode.set_block_bytes(&root);
                inode.flags |= EXT4_EXTENTS_FL;
            } else {
                inode.block[0] = data_block;
            }
            inode.blocks = u64::from(block_size / 512);
        }
    }

    Ok(inode)
}

pub fn read_symlink(
    inode: &Inode,
    buf: &mut [u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    owner: BlockOwner,
) -> Result<usize, Ext2Error> {
    if !inode.is_symlink() {
        return Err(Ext2Error::NotFile);
    }

    let target_len = inode.size as usize;
    let copy_len = core::cmp::min(target_len, buf.len());

    if inode.is_fast_symlink() {
        let block_bytes = slopos_ostd::util::byte_view::pod_slice_as_bytes(&inode.block[..]);
        // Clamped against the inline area rather than trusted from `i_size`:
        // an image can claim a fast symlink longer than the 60 bytes
        // `i_block` holds, and indexing past it panics the kernel.
        let copy_len = core::cmp::min(copy_len, block_bytes.len());
        buf[..copy_len].copy_from_slice(&block_bytes[..copy_len]);
        return Ok(copy_len);
    }

    let phys = blockmap::map_block(inode, FileBlock(0), geom, cache, device, owner)?;
    if !phys.is_valid() {
        return Err(Ext2Error::DeviceError);
    }
    let blk = cache.get_data(phys, device, owner)?;
    let data = blk.data();
    // Same clamp on the slow path: `i_size` is the image's claim.
    let copy_len = core::cmp::min(copy_len, data.len());
    buf[..copy_len].copy_from_slice(&data[..copy_len]);
    Ok(copy_len)
}
