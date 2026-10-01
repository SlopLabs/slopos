//! A GPT on a scratch disk, laid out as a partitioning tool writes one: a
//! protective MBR, the primary header and its array, and the backup pair at
//! the disk's end. Four entries, so the array is one block on either block
//! size.

use slopos_boot_core::Guid;
use slopos_boot_core::crc32::crc32;
use slopos_fs::blockdev::BlockDevice;
use slopos_ostd::KVec;

const ENTRY_SIZE: usize = 128;
const ENTRY_COUNT: usize = 4;

#[derive(Clone, Copy)]
pub struct Entry {
    pub type_guid: Guid,
    pub unique: Guid,
    /// The window, in bytes of the disk: whole logical blocks.
    pub start: u64,
    pub len: u64,
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[inline(never)]
fn array(block: u64, entries: &[Entry]) -> Result<(KVec<u8>, u32), &'static str> {
    if entries.len() > ENTRY_COUNT {
        return Err("more entries than the fixture's array holds");
    }
    let mut array = KVec::<u8>::zeroed(block as usize).map_err(|_| "array alloc")?;
    for (slot, entry) in entries.iter().enumerate() {
        let row = &mut array[slot * ENTRY_SIZE..(slot + 1) * ENTRY_SIZE];
        row[..16].copy_from_slice(&entry.type_guid.0);
        row[16..32].copy_from_slice(&entry.unique.0);
        put_u64(row, 32, entry.start / block);
        put_u64(row, 40, (entry.start + entry.len) / block - 1);
    }
    let crc = crc32(&array[..ENTRY_COUNT * ENTRY_SIZE]);
    Ok((array, crc))
}

#[inline(never)]
fn protective_mbr(blocks: u64) -> Result<KVec<u8>, &'static str> {
    let mut mbr = KVec::<u8>::zeroed(512).map_err(|_| "mbr alloc")?;
    mbr[446 + 4] = 0xEE;
    put_u32(&mut mbr, 446 + 8, 1);
    let span = u32::try_from(blocks - 1).unwrap_or(u32::MAX);
    put_u32(&mut mbr, 446 + 12, span);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;
    Ok(mbr)
}

/// One header block: `lba` its own, `alt` the other copy's.
#[inline(never)]
fn header(
    block: u64,
    lba: u64,
    alt: u64,
    array_lba: u64,
    last_usable: u64,
    disk: Guid,
    array_crc: u32,
) -> Result<KVec<u8>, &'static str> {
    let mut h = KVec::<u8>::zeroed(block as usize).map_err(|_| "header alloc")?;
    h[..8].copy_from_slice(b"EFI PART");
    put_u32(&mut h, 8, 0x0001_0000);
    put_u32(&mut h, 12, 92);
    put_u64(&mut h, 24, lba);
    put_u64(&mut h, 32, alt);
    put_u64(&mut h, 40, 3);
    put_u64(&mut h, 48, last_usable);
    h[56..72].copy_from_slice(&disk.0);
    put_u64(&mut h, 72, array_lba);
    put_u32(&mut h, 80, ENTRY_COUNT as u32);
    put_u32(&mut h, 84, ENTRY_SIZE as u32);
    put_u32(&mut h, 88, array_crc);
    let crc = crc32(&h[..92]);
    put_u32(&mut h, 16, crc);
    Ok(h)
}

/// Write a table naming `disk` and `entries` onto the whole of `device`.
#[inline(never)]
pub fn install(
    device: &dyn BlockDevice,
    disk: Guid,
    entries: &[Entry],
) -> Result<(), &'static str> {
    let block = u64::from(device.logical_block_size());
    let blocks = device.capacity() / block;
    let (array, crc) = array(block, entries)?;
    let backup_array = blocks - 2;
    let primary = header(block, 1, blocks - 1, 2, backup_array - 1, disk, crc)?;
    let backup = header(
        block,
        blocks - 1,
        1,
        backup_array,
        backup_array - 1,
        disk,
        crc,
    )?;
    let writes: [(u64, &[u8]); 5] = [
        (0, &protective_mbr(blocks)?),
        (block, &primary),
        (2 * block, &array),
        (backup_array * block, &array),
        ((blocks - 1) * block, &backup),
    ];
    for (at, bytes) in writes {
        device.write_at(at, bytes).map_err(|_| "table write")?;
    }
    device.flush().map_err(|_| "flush")
}

/// Zero the table's first and last blocks: no signature is left to find.
#[inline(never)]
pub fn wipe(device: &dyn BlockDevice) -> Result<(), &'static str> {
    let block = u64::from(device.logical_block_size());
    let blocks = device.capacity() / block;
    let zeros = KVec::<u8>::zeroed(3 * block as usize).map_err(|_| "zeros alloc")?;
    device.write_at(0, &zeros).map_err(|_| "wipe head")?;
    device
        .write_at((blocks - 2) * block, &zeros[..2 * block as usize])
        .map_err(|_| "wipe tail")?;
    device.flush().map_err(|_| "flush")
}
