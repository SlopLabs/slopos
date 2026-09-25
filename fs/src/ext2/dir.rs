use super::Ext2Error;
use super::blockmap;
use super::cache::{BlockCache, BlockOwner};
use super::dirindex::{DirProbe, hash_name};
use super::geometry::Ext2Geometry;
use super::ondisk::{
    DIR_ENTRY_HEADER_SIZE, DirEntry, Inode, Superblock, dir_entry_size, write_dir_entry,
};
use super::types::{FileBlock, InodeNum};
use crate::blockdev::BlockDevice;
use core::cmp;

/// One record's header fields, accepted only if every consumer of the record
/// can act on it.
///
/// The walker needed only room for the name, the inserter room for the
/// *padded* entry size; `name_len=1, rec_len=9` satisfied one and underflowed
/// the other. One predicate, used by both.
struct DirRecord {
    inode: u32,
    rec_len: usize,
    name_len: usize,
    file_type: u8,
    /// Bytes this record actually needs; never greater than `rec_len`.
    actual_size: usize,
}

fn parse_record(data: &[u8], cursor: usize, block_size: usize) -> Result<DirRecord, Ext2Error> {
    if cursor + DIR_ENTRY_HEADER_SIZE > data.len() || cursor + DIR_ENTRY_HEADER_SIZE > block_size {
        return Err(Ext2Error::DirectoryFormat);
    }
    let inode = u32::from_le_bytes([
        data[cursor],
        data[cursor + 1],
        data[cursor + 2],
        data[cursor + 3],
    ]);
    let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
    let name_len = data[cursor + 6] as usize;
    let file_type = data[cursor + 7];

    if rec_len < DIR_ENTRY_HEADER_SIZE || rec_len % 4 != 0 {
        return Err(Ext2Error::DirectoryFormat);
    }
    let end = cursor
        .checked_add(rec_len)
        .ok_or(Ext2Error::DirectoryFormat)?;
    if end > block_size || end > data.len() {
        return Err(Ext2Error::DirectoryFormat);
    }

    // A free record's name_len byte is stale and unconstrained, so only a live
    // record's name is held to fitting.
    let actual_size = if inode != 0 {
        let size = dir_entry_size(name_len);
        if size > rec_len {
            return Err(Ext2Error::DirectoryFormat);
        }
        size
    } else {
        DIR_ENTRY_HEADER_SIZE
    };

    Ok(DirRecord {
        inode,
        rec_len,
        name_len,
        file_type,
        actual_size,
    })
}

/// The directory these blocks belong to.
///
/// Every directory helper is already called with the owner that says so, and
/// that is the same fact the name index is keyed by; taking it from there
/// rather than as a twelfth parameter keeps the two from disagreeing.
fn dir_ino(owner: BlockOwner) -> u32 {
    owner.charged_inode().unwrap_or(0)
}

/// Iterate directory entries, calling `f` for each valid entry.
/// Returns early if `f` returns `false`.
pub fn for_each_entry(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
    f: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<(), Ext2Error> {
    for_each_entry_from(
        inode,
        0,
        cache,
        device,
        geom,
        block_size,
        owner,
        &mut |_, e| f(e),
    )
    .map(|_| ())
}

/// Walk from a byte offset into the directory's data, handing each callback
/// the offset of the record *after* the one it is given.
///
/// The offset is what ext2 directories are indexed by — an entry's position
/// is stable under an unrelated create or unlink, whereas its ordinal is not
/// — so this is what a resumable `readdir` pages over. Answers the offset the
/// walk reached, which is the directory's size once it ends.
#[allow(clippy::too_many_arguments)]
pub fn for_each_entry_from(
    inode: &Inode,
    start: u64,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
    f: &mut dyn FnMut(u64, DirEntry<'_>) -> bool,
) -> Result<u64, Ext2Error> {
    if !inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    // The cookie reaches here from userland through `fs_list`. The record
    // parser bounds everything it reads, so this is a refusal rather than a
    // safety fix: a caller resuming from a cookie this directory never issued
    // is a bug worth reporting, not an empty listing.
    if start > inode.size {
        return Err(Ext2Error::InvalidRange);
    }
    let bs = block_size as u64;
    // Resume at the *block* holding the cookie and re-walk it from its start,
    // skipping the records that end at or before it. ext2 chains records by
    // `rec_len`, so a boundary is only reachable by walking from the block's
    // beginning — but the re-walk is bounded by one block.
    let mut offset = (start / bs) * bs;
    while offset < inode.size {
        let file_block =
            FileBlock(u32::try_from(offset / bs).map_err(|_| Ext2Error::InvalidRange)?);
        let phys = blockmap::map_block(inode, file_block, geom, cache, device, owner)?;
        if !phys.is_valid() {
            return Err(Ext2Error::DirectoryFormat);
        }
        let block = cache.get_owned(phys, device, owner)?;
        let data = block.data();
        let mut cursor = 0usize;
        while cursor + DIR_ENTRY_HEADER_SIZE <= block_size as usize {
            let record = parse_record(data, cursor, block_size as usize)?;
            let next = offset + (cursor + record.rec_len) as u64;
            if record.inode != 0 && next > start {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + record.name_len;
                let entry = DirEntry {
                    inode: InodeNum(record.inode),
                    file_type: record.file_type,
                    name: &data[name_start..name_end],
                    offset: offset + cursor as u64,
                };
                if !f(next, entry) {
                    return Ok(next);
                }
            }
            cursor += record.rec_len;
        }
        offset += bs;
    }
    Ok(offset)
}

/// The one directory lookup: `resolve_path`, the VFS, every create's existence
/// check and every unlink come through here, which is what makes it the single
/// place the name index has to hook.
///
/// An index hit is only a *candidate* position: the record is read and its
/// name compared, so a stale entry resolves to nothing rather than to the
/// wrong inode.
pub fn lookup_child(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<InodeNum, Ext2Error> {
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let ino = dir_ino(owner);
    let hash = hash_name(name);
    let mut cursor = 0u32;
    loop {
        match cache.dir_probe(ino, hash, &mut cursor) {
            DirProbe::Candidate(pos) => {
                if let Some(found) =
                    entry_at(parent, pos, name, cache, device, geom, block_size, owner)?
                {
                    return Ok(found);
                }
            }
            DirProbe::Absent => return Err(Ext2Error::PathNotFound),
            DirProbe::Unknown => break,
        }
    }
    scan_for_child(parent, name, cache, device, geom, block_size, owner)
}

/// The inode of the record at `pos`, if there is one there and it carries
/// `name`.
///
/// Everything the index could get wrong lands here — a position past the end,
/// inside a record rather than at its start, since freed, or since reused by
/// another name. Each is a non-match, not an error.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn entry_at(
    parent: &Inode,
    pos: u32,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<Option<InodeNum>, Ext2Error> {
    let bs = block_size as u64;
    let offset = pos as u64;
    if offset >= parent.size {
        return Ok(None);
    }
    let file_block = FileBlock(u32::try_from(offset / bs).map_err(|_| Ext2Error::InvalidRange)?);
    let phys = blockmap::map_block(parent, file_block, geom, cache, device, owner)?;
    if !phys.is_valid() {
        return Ok(None);
    }
    let cursor = (offset % bs) as usize;
    let block = cache.get_owned(phys, device, owner)?;
    let data = block.data();
    let Ok(record) = parse_record(data, cursor, block_size as usize) else {
        return Ok(None);
    };
    if record.inode == 0 || record.name_len != name.len() {
        return Ok(None);
    }
    let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
    if &data[name_start..name_start + record.name_len] != name {
        return Ok(None);
    }
    Ok(Some(InodeNum(record.inode)))
}

/// The linear scan, filing what it reads.
///
/// The walk is the one the lookup owed anyway, so the cold cost is unchanged;
/// what it leaves behind is the index. It stops at the name it was looking
/// for, leaving the table holding a prefix and therefore not complete — a
/// later miss walks the whole thing and finishes it.
#[inline(never)]
fn scan_for_child(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<InodeNum, Ext2Error> {
    let ino = dir_ino(owner);
    let mut index = cache.take_dir_index();
    let mut recording = index.begin_build(ino);
    let mut found: Option<InodeNum> = None;

    let walked = for_each_entry_from(
        parent,
        0,
        cache,
        device,
        geom,
        block_size,
        owner,
        &mut |_, entry| {
            if recording {
                match u32::try_from(entry.offset) {
                    Ok(pos) if index.add(ino, hash_name(entry.name), pos) => {}
                    _ => recording = false,
                }
            }
            if entry.name == name {
                found = Some(entry.inode);
                return false;
            }
            true
        },
    );

    // Complete only when the walk truly ran off the end: an early stop or an
    // error leaves records the table never saw.
    if recording && found.is_none() && walked.is_ok_and(|reached| reached >= parent.size) {
        index.finish_build(ino);
    }
    cache.put_dir_index(index);
    walked?;
    found.ok_or(Ext2Error::PathNotFound)
}

/// Remove a directory entry by name. Per ext2: extend the predecessor's
/// `rec_len` to absorb the deleted entry; for the first entry in a block, zero
/// the inode field instead.
pub fn remove_dir_entry(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let bs = block_size as usize;
    let mut offset = 0u64;
    let mut removed: Option<u64> = None;
    while offset < parent.size && removed.is_none() {
        let file_block = FileBlock(
            u32::try_from(offset / block_size as u64).map_err(|_| Ext2Error::InvalidBlock)?,
        );
        let phys = blockmap::map_block(parent, file_block, geom, cache, device, owner)?;
        if !phys.is_valid() {
            break;
        }
        let mut block = cache.get_owned(phys, device, owner)?;
        let data = block.data_mut();

        let mut cursor = 0usize;
        let mut prev_cursor: Option<usize> = None;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let record = parse_record(data, cursor, bs)?;
            let rec_len = record.rec_len;

            if record.inode != 0 {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end =
                    name_start + cmp::min(record.name_len, rec_len - DIR_ENTRY_HEADER_SIZE);
                if &data[name_start..name_end] == name {
                    match prev_cursor {
                        Some(prev) => {
                            let prev_rec =
                                u16::from_le_bytes([data[prev + 4], data[prev + 5]]) as usize;
                            let merged = prev_rec + rec_len;
                            // A merged record must still fit the block; a
                            // u16 truncation here would corrupt the chain.
                            let new_rec =
                                u16::try_from(merged).map_err(|_| Ext2Error::DirectoryFormat)?;
                            data[prev + 4..prev + 6].copy_from_slice(&new_rec.to_le_bytes());
                        }
                        None => {
                            data[cursor..cursor + 4].copy_from_slice(&0u32.to_le_bytes());
                        }
                    }
                    removed = Some(offset + cursor as u64);
                    break;
                }
                prev_cursor = Some(cursor);
            }
            cursor += rec_len;
        }
        offset += block_size as u64;
    }

    let Some(pos) = removed else {
        return Err(Ext2Error::PathNotFound);
    };
    let ino = dir_ino(owner);
    if let Ok(pos32) = u32::try_from(pos) {
        cache.note_dir_remove(ino, hash_name(name), pos32);
    }
    if let Ok(block) = u32::try_from(pos / block_size as u64) {
        cache.lower_dir_free_hint(ino, block);
    }
    Ok(())
}

/// Check if a directory is empty (only contains . and ..).
pub fn is_dir_empty(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<bool, Ext2Error> {
    let mut count = 0u32;
    for_each_entry(
        inode,
        cache,
        device,
        geom,
        block_size,
        owner,
        &mut |entry| {
            if entry.name == b"." || entry.name == b".." {
                count += 1;
                true
            } else {
                count += 1;
                false
            }
        },
    )?;
    Ok(count <= 2)
}

/// What one insert is placing, so the two range searches and the block append
/// do not each carry it as four more parameters.
struct Insert<'n> {
    child: InodeNum,
    name: &'n [u8],
    file_type: u8,
    /// Padded bytes the record needs.
    needed: usize,
}

#[allow(clippy::too_many_arguments)]
pub fn append_dir_entry(
    parent_inode: &mut Inode,
    child: InodeNum,
    name: &[u8],
    file_type: u8,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    superblock: &mut Superblock,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    let ino = dir_ino(owner);
    let req = Insert {
        child,
        name,
        file_type,
        needed: dir_entry_size(name.len()),
    };
    let need = req.needed as u32;
    let blocks = parent_inode.size.div_ceil(block_size as u64);
    let (hint_block, proved) = cache.dir_free_hint(ino);
    let hint = u64::from(hint_block).min(blocks);

    // First fit from the hint, and then over the prefix it skipped — unless
    // something already proved a record this small will not fit down there.
    // The second pass is what keeps the hint an accelerator and nothing more:
    // an insert that would have fitted must still fit. Skipping it is only
    // ever done on that proof, because a growing directory would otherwise
    // repeat the whole-prefix scan every time a block filled.
    let mut placed = place_in_range(
        parent_inode,
        &req,
        hint,
        blocks,
        cache,
        device,
        geom,
        block_size,
        owner,
    )?;
    // What the suffix pass established: no slack of `need` from `hint`
    // onwards, on top of whatever was already known below `hint`.
    let mut proof = if hint == 0 {
        need
    } else if proved == 0 {
        0
    } else {
        proved.max(need)
    };
    if placed.is_none() && hint > 0 && !(proved != 0 && need >= proved) {
        placed = place_in_range(
            parent_inode,
            &req,
            0,
            hint,
            cache,
            device,
            geom,
            block_size,
            owner,
        )?;
        // Either the whole directory holds no slack of `need`, or the record
        // went in below the hint and nothing is known about what precedes it.
        proof = if placed.is_none() { need } else { 0 };
    }
    let pos = match placed {
        Some(pos) => pos,
        None => grow_and_place(
            parent_inode,
            &req,
            cache,
            device,
            geom,
            block_size,
            superblock,
            owner,
        )?,
    };

    if let Ok(block) = u32::try_from(pos / block_size as u64) {
        cache.set_dir_hint(ino, block, proof);
    }
    match u32::try_from(pos) {
        Ok(pos) => cache.note_dir_insert(ino, hash_name(name), pos),
        // Past what a position fits in: a complete index that is missing a
        // name is the one failure that answers wrongly.
        Err(_) => cache.forget_dir_index(ino),
    }
    Ok(())
}

/// First fit over file blocks `first..end`, answering the byte offset the new
/// record was written at.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn place_in_range(
    parent_inode: &Inode,
    req: &Insert<'_>,
    first: u64,
    end: u64,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<Option<u64>, Ext2Error> {
    let bs = block_size as usize;
    for index in first..end {
        let offset = index * block_size as u64;
        if offset >= parent_inode.size {
            break;
        }
        let file_block = FileBlock(u32::try_from(index).map_err(|_| Ext2Error::InvalidBlock)?);
        let phys = blockmap::map_block(parent_inode, file_block, geom, cache, device, owner)?;
        if !phys.is_valid() {
            continue;
        }
        let mut block = cache.get_owned(phys, device, owner)?;
        let data = block.data_mut();
        let mut cursor = 0usize;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let record = parse_record(data, cursor, bs)?;
            let rec_len = record.rec_len;
            let actual_size = record.actual_size;
            let slack = rec_len - actual_size;

            if slack >= req.needed {
                if record.inode != 0 {
                    data[cursor + 4..cursor + 6]
                        .copy_from_slice(&(actual_size as u16).to_le_bytes());
                    let new_cursor = cursor + actual_size;
                    let new_rec_len = rec_len - actual_size;
                    write_dir_entry(
                        &mut data[new_cursor..new_cursor + new_rec_len],
                        req.child,
                        req.name,
                        req.file_type,
                        new_rec_len,
                    );
                    return Ok(Some(offset + new_cursor as u64));
                }
                write_dir_entry(
                    &mut data[cursor..cursor + rec_len],
                    req.child,
                    req.name,
                    req.file_type,
                    rec_len,
                );
                return Ok(Some(offset + cursor as u64));
            }
            cursor += rec_len;
        }
    }
    Ok(None)
}

/// Nothing fitted: one more block on the end, the record at its start.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn grow_and_place(
    parent_inode: &mut Inode,
    req: &Insert<'_>,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    superblock: &mut Superblock,
    owner: BlockOwner,
) -> Result<u64, Ext2Error> {
    let bs = block_size as usize;
    let file_block = FileBlock(
        u32::try_from(parent_inode.size / block_size as u64)
            .map_err(|_| Ext2Error::InvalidBlock)?,
    );
    let (new_block, allocated) = blockmap::ensure_data_block(
        parent_inode,
        file_block,
        cache,
        device,
        geom,
        superblock,
        owner,
    )?;

    let mut block = cache.get_zero_owned(new_block, device, owner)?;
    let data = block.data_mut();
    write_dir_entry(&mut data[..bs], req.child, req.name, req.file_type, bs);
    drop(block);

    let pos = parent_inode.size;
    parent_inode.size += block_size as u64;
    parent_inode.blocks += allocated * (block_size / 512);
    Ok(pos)
}

pub fn update_dotdot(
    dir_inode: &Inode,
    new_parent: InodeNum,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    geom: &Ext2Geometry,
    block_size: u32,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    if !dir_inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let phys = blockmap::map_block(dir_inode, FileBlock(0), geom, cache, device, owner)?;
    if !phys.is_valid() {
        return Err(Ext2Error::DirectoryFormat);
    }
    let mut block = cache.get_owned(phys, device, owner)?;
    let data = block.data_mut();
    let bs = block_size as usize;

    let mut cursor = 0usize;
    while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
        let record = parse_record(data, cursor, bs)?;
        if record.inode != 0 && record.name_len == 2 {
            let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
            if &data[name_start..name_start + 2] == b".." {
                data[cursor..cursor + 4].copy_from_slice(&new_parent.raw().to_le_bytes());
                return Ok(());
            }
        }
        cursor += record.rec_len;
    }
    Err(Ext2Error::DirectoryFormat)
}
