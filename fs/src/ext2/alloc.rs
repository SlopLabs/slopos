use super::Ext2Error;
use super::cache::{BlockCache, BlockOwner, CachedBlock};
use super::geometry::Ext2Geometry;
use super::ondisk::{GroupDesc, Superblock};
use super::types::{BlockNum, GroupIdx, InodeNum};
use crate::blockdev::BlockDevice;
use slopos_ext4_core::group::{self as ext4_group, BLOCK_UNINIT, INODE_UNINIT};
use slopos_ostd::bitmap_slice;

/// ext2's denial-of-service answer: `s_r_blocks_count` blocks are spendable
/// only by an entitled writer, so a process that fills the disk still leaves
/// the reserve for `/sbin/init`. An entitled handle carries a reserve of zero.
fn reserve_permits_allocation(geom: &Ext2Geometry, superblock: &Superblock) -> bool {
    superblock.free_blocks_count > geom.reserved_blocks()
}

/// One block, as near `goal` as the volume allows: `goal` itself when it is
/// free, so a file written in order lands in one extent.
pub fn allocate_block_near(
    goal: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    if !reserve_permits_allocation(geom, superblock) {
        return Err(Ext2Error::NoSpace);
    }
    // The reserve above is a system floor; this is the per-principal ceiling.
    // Charged before the search so a caller over it costs no bitmap work, and
    // given back below if the search finds nothing.
    cache.charge_blocks(geom.account(), owner.charged_inode(), 1)?;
    let mut allocated = allocate_searching(goal, geom, superblock, cache, device);
    // Space freed by operations whose records are not durable yet is held
    // back from reuse; one log sync hands it to this search.
    if matches!(allocated, Err(Ext2Error::NoSpace)) && cache.has_blocked_frees() {
        allocated = cache
            .sync_log(device)
            .and_then(|()| allocate_searching(goal, geom, superblock, cache, device));
    }
    if allocated.is_err() {
        cache.cancel_block_charge(geom.account(), owner.charged_inode(), 1);
    }
    allocated
}

/// Sweeps the volume for a free block, hinted.
///
/// The goal group is searched from the goal's bit, then the others from the
/// group the last allocation succeeded in, each from the bit above the last
/// one taken, so a nearly-full volume does not rescan what it knows is
/// allocated.
fn allocate_searching(
    goal: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<BlockNum, Ext2Error> {
    let groups_count = geom.groups_count();
    if groups_count == 0 {
        return Err(Ext2Error::NoSpace);
    }
    cache.size_group_hints(groups_count);

    let located = geom.locate_block(goal);
    if let Some((group, bit)) = located
        && let Some(block) =
            try_alloc_block_in_group(group, Some(bit), geom, superblock, cache, device)?
    {
        cache.set_alloc_group(group.raw());
        return Ok(block);
    }
    let goal_group = located.map(|(g, _)| g);

    let start = cache.alloc_group() % groups_count;
    for k in 0..groups_count {
        let raw = (start + k) % groups_count;
        let Some(group) = geom.group(raw) else {
            continue;
        };
        if goal_group == Some(group) {
            continue;
        }
        if let Some(block) = try_alloc_block_in_group(group, None, geom, superblock, cache, device)?
        {
            cache.set_alloc_group(raw);
            return Ok(block);
        }
    }

    Err(Ext2Error::NoSpace)
}

pub fn allocate_block(
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    allocate_block_near(BlockNum::ZERO, geom, superblock, cache, device, owner)
}

pub fn allocate_inode(
    parent_group: GroupIdx,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<InodeNum, Ext2Error> {
    if superblock.free_inodes_count <= geom.reserved_inodes() {
        return Err(Ext2Error::NoSpace);
    }
    // Locality: files in one directory belong in one group.
    if let Some(ino) = try_alloc_inode_in_group(parent_group, geom, superblock, cache, device)? {
        return Ok(ino);
    }

    for g in 0..geom.groups_count() {
        let Some(group) = geom.group(g) else {
            continue;
        };
        if group == parent_group {
            continue;
        }
        if let Some(ino) = try_alloc_inode_in_group(group, geom, superblock, cache, device)? {
            return Ok(ino);
        }
    }

    Err(Ext2Error::NoSpace)
}

pub fn free_block(
    block: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    let (group, bit) = geom.locate_block(block).ok_or(Ext2Error::InvalidBlock)?;
    // Before the bitmap moves: no earlier log record may be replayed into
    // this block, because the next allocation may hand it out as file data.
    cache.note_revoke(block, device)?;
    cache.note_block_freed(block);
    // Deferred to the commit: a rollback restores the bitmap, so an operation
    // that frees and then fails still owes the block. Credited to the
    // principal charged for it, not to this caller.
    cache.note_blocks_freed(owner.charged_inode(), 1);

    let mut desc = read_group_desc(group, geom, cache, device)?;
    if left_uninit(&desc, BLOCK_UNINIT, geom) {
        return Err(Ext2Error::InvalidBlock);
    }
    {
        let mut bmap = block_bitmap(&desc, geom, cache, device)?;
        if !bitmap_slice::test_bit(bmap.data(), bit as usize) {
            return Err(Ext2Error::InvalidBlock);
        }
        bitmap_slice::clear_bit(bmap.data_mut(), bit as usize);
    }
    flip_block_bitmap_csum(&mut desc, geom, bit);

    desc.free_blocks_count = desc.free_blocks_count.saturating_add(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_add(1);

    Ok(())
}

pub fn free_inode(
    ino: InodeNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    let (group, bit) = geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;

    let mut desc = read_group_desc(group, geom, cache, device)?;
    if left_uninit(&desc, INODE_UNINIT, geom) {
        return Err(Ext2Error::InvalidBlock);
    }
    {
        let mut bmap = inode_bitmap(&desc, geom, cache, device)?;
        if !bitmap_slice::test_bit(bmap.data(), bit as usize) {
            return Err(Ext2Error::InvalidBlock);
        }
        bitmap_slice::clear_bit(bmap.data_mut(), bit as usize);
    }
    flip_inode_bitmap_csum(&mut desc, geom, bit);

    desc.free_inodes_count = desc.free_inodes_count.saturating_add(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_add(1);

    Ok(())
}

/// Whether a checksummed volume left `desc`'s group with `flag`'s bitmap
/// unwritten, so nothing it covers was ever handed out.
fn left_uninit(desc: &GroupDesc, flag: u16, geom: &Ext2Geometry) -> bool {
    geom.uninit_groups() && desc.flags & flag != 0
}

/// A group's block bitmap, its checksum verified against the descriptor on
/// first read.
fn block_bitmap<'c>(
    desc: &GroupDesc,
    geom: &Ext2Geometry,
    cache: &'c mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<CachedBlock<'c>, Ext2Error> {
    let mut bmap = cache.get_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
    if let Some(seed) = geom.csum_seed()
        && !bmap.checked()
    {
        let sum = ext4_group::bitmap_checksum(seed, bmap.data(), geom.blocks_per_group());
        if ext4_group::stored_bitmap_csum(sum, geom.desc_size()) != desc.block_bitmap_csum {
            return Err(Ext2Error::BadChecksum);
        }
        bmap.set_checked();
    }
    Ok(bmap)
}

fn inode_bitmap<'c>(
    desc: &GroupDesc,
    geom: &Ext2Geometry,
    cache: &'c mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<CachedBlock<'c>, Ext2Error> {
    let mut bmap = cache.get_owned(desc.inode_bitmap, device, BlockOwner::Alloc)?;
    if let Some(seed) = geom.csum_seed()
        && !bmap.checked()
    {
        let sum = ext4_group::bitmap_checksum(seed, bmap.data(), geom.inodes_per_group());
        if ext4_group::stored_bitmap_csum(sum, geom.desc_size()) != desc.inode_bitmap_csum {
            return Err(Ext2Error::BadChecksum);
        }
        bmap.set_checked();
    }
    Ok(bmap)
}

fn stamp_block_bitmap(desc: &mut GroupDesc, geom: &Ext2Geometry, bitmap: &[u8]) {
    if let Some(seed) = geom.csum_seed() {
        let sum = ext4_group::bitmap_checksum(seed, bitmap, geom.blocks_per_group());
        desc.block_bitmap_csum = ext4_group::stored_bitmap_csum(sum, geom.desc_size());
    }
}

fn stamp_inode_bitmap(desc: &mut GroupDesc, geom: &Ext2Geometry, bitmap: &[u8]) {
    if let Some(seed) = geom.csum_seed() {
        let sum = ext4_group::bitmap_checksum(seed, bitmap, geom.inodes_per_group());
        desc.inode_bitmap_csum = ext4_group::stored_bitmap_csum(sum, geom.desc_size());
    }
}

fn flip_block_bitmap_csum(desc: &mut GroupDesc, geom: &Ext2Geometry, bit: u32) {
    if geom.csum_seed().is_some() {
        let sum =
            ext4_group::flip_bitmap_checksum(desc.block_bitmap_csum, bit, geom.blocks_per_group());
        desc.block_bitmap_csum = ext4_group::stored_bitmap_csum(sum, geom.desc_size());
    }
}

fn flip_inode_bitmap_csum(desc: &mut GroupDesc, geom: &Ext2Geometry, bit: u32) {
    if geom.csum_seed().is_some() {
        let sum =
            ext4_group::flip_bitmap_checksum(desc.inode_bitmap_csum, bit, geom.inodes_per_group());
        desc.inode_bitmap_csum = ext4_group::stored_bitmap_csum(sum, geom.desc_size());
    }
}

/// The first block of `group` and how many of the volume's blocks it holds.
fn group_span(geom: &Ext2Geometry, group: GroupIdx) -> (u32, u32) {
    let base = geom.first_data_block().raw() + group.raw() * geom.blocks_per_group();
    let len = geom.blocks_per_group().min(geom.blocks_count() - base);
    (base, len)
}

/// Write the block bitmap an uninitialised group implies: its backup
/// superblock and descriptor table, the bitmaps and inode tables inside it,
/// and the bits past its end. Refused unless it agrees with the descriptor's
/// free count, since a wrong one would hand out a block something owns.
fn init_block_bitmap(
    group: GroupIdx,
    desc: &mut GroupDesc,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    let (base, len) = group_span(geom, group);
    let bits = geom.block_size() as usize * 8;
    {
        let mut bmap = cache.get_zero_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
        let data = bmap.data_mut();
        for bit in len as usize..bits {
            bitmap_slice::set_bit(data, bit);
        }
        if geom.group_has_super(group) {
            let reserved = 1 + geom.gdt_blocks() + geom.reserved_gdt_blocks();
            mark_range(data, base, len, base, reserved);
        }
    }
    let per_flex = geom.groups_per_flex();
    let first = group.raw() - group.raw() % per_flex;
    let mut scan = first..first.saturating_add(per_flex).min(geom.groups_count());
    let mut scanned_all = false;
    loop {
        for other in scan.clone().filter_map(|raw| geom.group(raw)) {
            mark_metadata_of(other, group, desc, geom, cache, device)?;
        }
        let used = count_set(desc, cache, device, len)?;
        if len - used == desc.free_blocks_count {
            break;
        }
        if scanned_all {
            return Err(Ext2Error::InvalidBlock);
        }
        // A resize can place a group's metadata outside its flex group.
        scanned_all = true;
        scan = 0..geom.groups_count();
    }
    desc.flags &= !BLOCK_UNINIT;
    let bmap = cache.get_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
    stamp_block_bitmap(desc, geom, bmap.data());
    Ok(())
}

/// Mark in `group`'s block bitmap the bitmaps and inode table of `other`
/// that lie inside it.
#[inline(never)]
fn mark_metadata_of(
    other: GroupIdx,
    group: GroupIdx,
    desc: &GroupDesc,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    let (base, len) = group_span(geom, group);
    let d = if other == group {
        *desc
    } else {
        read_group_desc(other, geom, cache, device)?
    };
    let mut bmap = cache.get_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
    let data = bmap.data_mut();
    mark_range(data, base, len, d.block_bitmap.raw(), 1);
    mark_range(data, base, len, d.inode_bitmap.raw(), 1);
    mark_range(
        data,
        base,
        len,
        d.inode_table.raw(),
        geom.itable_blocks_per_group(),
    );
    Ok(())
}

/// Set the bits of `[first, first + count)` that fall in the group starting
/// at block `base`, `len` blocks long.
fn mark_range(bitmap: &mut [u8], base: u32, len: u32, first: u32, count: u32) {
    let start = first.max(base);
    let end = first.saturating_add(count).min(base + len);
    for block in start..end {
        bitmap_slice::set_bit(bitmap, (block - base) as usize);
    }
}

fn count_set(
    desc: &GroupDesc,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    len: u32,
) -> Result<u32, Ext2Error> {
    let bmap = cache.get_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
    let data = bmap.data();
    Ok((0..len as usize)
        .filter(|&bit| bitmap_slice::test_bit(data, bit))
        .count() as u32)
}

/// An uninitialised inode bitmap is all clear but for the bits past the
/// group, which every reader expects set.
fn init_inode_bitmap(
    desc: &mut GroupDesc,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    if desc.free_inodes_count != geom.inodes_per_group() {
        return Err(Ext2Error::InvalidInode);
    }
    let mut bmap = cache.get_zero_owned(desc.inode_bitmap, device, BlockOwner::Alloc)?;
    let data = bmap.data_mut();
    for bit in geom.inodes_per_group() as usize..geom.block_size() as usize * 8 {
        bitmap_slice::set_bit(data, bit);
    }
    desc.flags &= !INODE_UNINIT;
    stamp_inode_bitmap(desc, geom, bmap.data());
    Ok(())
}

fn try_alloc_block_in_group(
    group: GroupIdx,
    goal_bit: Option<u32>,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<Option<BlockNum>, Ext2Error> {
    let mut desc = read_group_desc(group, geom, cache, device)?;
    if desc.free_blocks_count == 0 {
        return Ok(None);
    }
    if left_uninit(&desc, BLOCK_UNINIT, geom) {
        init_block_bitmap(group, &mut desc, geom, cache, device)?;
        write_group_desc(group, &desc, geom, cache, device)?;
    }

    let bits_in_group = geom.blocks_per_group();
    let hint = goal_bit.map_or_else(|| cache.group_hint(group.raw()), |bit| bit as usize);
    // `rescanned` is the hint's own correctness condition: a search that
    // found nothing above the hint must still see the blocks below it, and
    // the hint it skipped them on is stale.
    let (bit, rescanned) = {
        let bmap = block_bitmap(&desc, geom, cache, device)?;
        let data = bmap.data();
        let bits = bits_in_group as usize;
        let mut from = hint;
        let mut rescanned = false;
        let found = loop {
            match bitmap_slice::find_first_zero(data, bits, from) {
                Some(bit) => {
                    let blocked = geom
                        .block_of(group, bit as u32)
                        .is_some_and(|b| bmap.reuse_blocked(b));
                    if !blocked {
                        break Some(bit);
                    }
                    from = bit + 1;
                }
                None if !rescanned && hint > 0 => {
                    rescanned = true;
                    from = 0;
                }
                None => break None,
            }
        };
        (found, rescanned)
    };
    if rescanned && goal_bit.is_none() {
        cache.set_group_hint(group.raw(), 0);
    }

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get_owned(desc.block_bitmap, device, BlockOwner::Alloc)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }
    flip_block_bitmap_csum(&mut desc, geom, bit as u32);
    cache.set_group_hint(group.raw(), bit as u32 + 1);

    let Some(block_num) = geom.block_of(group, bit as u32) else {
        return Err(Ext2Error::InvalidBlock);
    };

    desc.free_blocks_count = desc.free_blocks_count.saturating_sub(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_sub(1);

    Ok(Some(block_num))
}

fn try_alloc_inode_in_group(
    group: GroupIdx,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<Option<InodeNum>, Ext2Error> {
    let mut desc = read_group_desc(group, geom, cache, device)?;
    if desc.free_inodes_count == 0 {
        return Ok(None);
    }
    if geom.uninit_groups() {
        if desc.flags & INODE_UNINIT != 0 {
            init_inode_bitmap(&mut desc, geom, cache, device)?;
        }
        // Other implementations treat an in-use inode in a `BLOCK_UNINIT`
        // group as damage.
        if desc.flags & BLOCK_UNINIT != 0 {
            init_block_bitmap(group, &mut desc, geom, cache, device)?;
        }
        write_group_desc(group, &desc, geom, cache, device)?;
    }

    let bits_in_group = geom.inodes_per_group();
    let start_bit = if group.raw() == 0 {
        superblock.first_ino.saturating_sub(1) as usize
    } else {
        0
    };

    let bit = {
        let bmap = inode_bitmap(&desc, geom, cache, device)?;
        bitmap_slice::find_first_zero(bmap.data(), bits_in_group as usize, start_bit)
    };

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get_owned(desc.inode_bitmap, device, BlockOwner::Alloc)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }
    flip_inode_bitmap_csum(&mut desc, geom, bit as u32);

    let Some(inode_num) = geom.inode_of(group, bit as u32) else {
        return Err(Ext2Error::InvalidInode);
    };

    // `bg_itable_unused` counts the table's never-used tail, which must start
    // past every inode taken.
    if geom.uninit_groups() {
        let first_unused = bits_in_group.saturating_sub(desc.itable_unused);
        if bit as u32 >= first_unused {
            desc.itable_unused = bits_in_group - bit as u32 - 1;
        }
    }
    desc.free_inodes_count = desc.free_inodes_count.saturating_sub(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_sub(1);

    Ok(Some(inode_num))
}

pub(crate) fn read_group_desc(
    group: GroupIdx,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<GroupDesc, Ext2Error> {
    let loc = geom.group_desc_loc(group);
    let blk = cache.get_owned(loc.block(), device, BlockOwner::Alloc)?;
    let raw = blk
        .slice(loc.within(), geom.desc_size())
        .ok_or(Ext2Error::InvalidBlock)?;
    if !ext4_group::verify(geom.desc_csum(), group.raw(), raw) {
        return Err(Ext2Error::BadChecksum);
    }
    let desc = GroupDesc::parse(raw)?;
    geom.validate_desc(group, &desc)?;
    Ok(desc)
}

pub(crate) fn write_group_desc(
    group: GroupIdx,
    desc: &GroupDesc,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    geom.validate_desc(group, desc)?;
    let loc = geom.group_desc_loc(group);
    let mut blk = cache.get_owned(loc.block(), device, BlockOwner::Alloc)?;
    let raw = blk
        .slice_mut(loc.within(), geom.desc_size())
        .ok_or(Ext2Error::InvalidBlock)?;
    desc.encode(raw);
    ext4_group::seal(geom.desc_csum(), group.raw(), raw);
    Ok(())
}
