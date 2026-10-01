//! The block layer on real disks: a GPT written through a whole-disk claim,
//! read back by a table re-read into partition nodes and `/dev/disk/by-*`
//! links, and the claims that keep a mounted partition's window from moving.
//!
//! Destructive, so only the NVMe scratch namespaces, one with 512-byte and one
//! with 4096-byte logical blocks. The table takes the first and last blocks;
//! the windows sit below the regions the engine tests write.

use slopos_abi::fs::block_ioctl;
use slopos_fs::blockdev::BlockDevice;
use slopos_fs::devfs::{
    BlockIoctlReply, DEV_NAME_MAX, devfs_block_ioctl, devfs_resolve_block_source,
};
use slopos_fs::partition::{PARTUUID_TEXT_MAX, format_guid};
use slopos_fs::verity::crc32;
use slopos_fs::vfs::{FileSystem, FileType, VfsError};
use slopos_ostd::KVec;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use crate::block::{self, ClaimError, DiskName, RereadError};

const ENTRY_SIZE: usize = 128;
const ENTRY_COUNT: usize = 4;
/// The windows, in bytes of the disk: 64 KiB..128 KiB and 128 KiB..256 KiB,
/// so both are whole logical blocks on either block size.
const P1: (u64, u64) = (64 << 10, 64 << 10);
const P2: (u64, u64) = (128 << 10, 128 << 10);

fn show(name: &[u8]) -> &str {
    core::str::from_utf8(name).unwrap_or("?")
}

fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn put_u64(buf: &mut [u8], at: usize, value: u64) {
    buf[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

/// Partition `slot`'s unique GUID: distinct per disk and slot, so one disk's
/// PARTUUID never resolves to the other's partition.
fn unique_guid(disk_tag: u8, slot: u8) -> [u8; 16] {
    core::array::from_fn(|i| match i {
        0 => disk_tag,
        1 => slot,
        _ => 0x40 + i as u8,
    })
}

/// The entry array's first block: the two windows, and the CRC over the
/// array the header names.
#[inline(never)]
fn gpt_array(block: u64, disk_tag: u8) -> Result<(KVec<u8>, u32), &'static str> {
    let mut array = KVec::<u8>::zeroed(block as usize).map_err(|_| "array alloc")?;
    for (slot, &(start, len)) in [P1, P2].iter().enumerate() {
        let row = &mut array[slot * ENTRY_SIZE..(slot + 1) * ENTRY_SIZE];
        row[..16].copy_from_slice(&[0x11 * (slot as u8 + 1); 16]);
        row[16..32].copy_from_slice(&unique_guid(disk_tag, slot as u8 + 1));
        put_u64(row, 32, start / block);
        put_u64(row, 40, (start + len) / block - 1);
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

/// The table for a disk of `blocks` logical blocks of `block` bytes: a
/// protective MBR, the primary header and array, and the backup pair.
#[inline(never)]
fn install_gpt(
    device: &dyn BlockDevice,
    block: u64,
    blocks: u64,
    disk_tag: u8,
) -> Result<(), &'static str> {
    let (array, crc) = gpt_array(block, disk_tag)?;
    let backup_array = blocks - 2;
    let primary = gpt_header(block, 1, blocks - 1, 2, backup_array - 1, crc)?;
    let backup = gpt_header(block, blocks - 1, 1, backup_array, backup_array - 1, crc)?;
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

/// One GPT header block: `lba` its own, `alt` the other copy's.
#[inline(never)]
fn gpt_header(
    block: u64,
    lba: u64,
    alt: u64,
    array_lba: u64,
    last_usable: u64,
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
    h[56..72].copy_from_slice(&[0x5A; 16]);
    put_u64(&mut h, 72, array_lba);
    put_u32(&mut h, 80, ENTRY_COUNT as u32);
    put_u32(&mut h, 84, ENTRY_SIZE as u32);
    put_u32(&mut h, 88, array_crc);
    let crc = crc32(&h[..92]);
    put_u32(&mut h, 16, crc);
    Ok(h)
}

/// Zero the table's first and last blocks: no signature is left to find.
#[inline(never)]
fn wipe_gpt(device: &dyn BlockDevice, block: u64, blocks: u64) -> Result<(), &'static str> {
    let zeros = KVec::<u8>::zeroed(3 * block as usize).map_err(|_| "zeros alloc")?;
    device.write_at(0, &zeros).map_err(|_| "wipe head")?;
    device
        .write_at((blocks - 2) * block, &zeros[..2 * block as usize])
        .map_err(|_| "wipe tail")?;
    device.flush().map_err(|_| "flush")
}

fn node_capacity(name: &[u8]) -> Option<u64> {
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let inode = fs.lookup(fs.root_inode(), name).ok()?;
    fs.stat(inode).ok().map(|s| s.size)
}

#[inline(never)]
fn partitioned_disk(disk: &[u8], disk_tag: u8) -> TestResult {
    let Some(device) = block::disk(disk) else {
        return fail!("scratch disk {} not attached", show(disk));
    };
    let block = u64::from(device.logical_block_size());
    let blocks = device.capacity() / block;
    {
        let whole = match block::claim(disk) {
            Ok(c) => c,
            Err(e) => return fail!("claiming {} failed: {:?}", show(disk), e),
        };
        if let Err(why) = install_gpt(whole.as_ref(), block, blocks, disk_tag) {
            return fail!("could not install the GPT: {}", why);
        }
        assert_test!(
            matches!(block::reread(disk), Err(RereadError::Busy)),
            "a re-read must be refused while the whole disk is claimed"
        );
    }
    if let Err(e) = block::reread(disk) {
        return fail!("re-reading {} failed: {:?}", show(disk), e);
    }

    let base = DiskName::nvme(
        u32::from(disk[4] - b'0'),
        u32::from(disk[disk.len() - 1] - b'0'),
    );
    let (p1, p2) = (base.partition(1), base.partition(2));
    assert_eq_test!(
        node_capacity(p1.as_bytes()),
        Some(P1.1),
        "partition 1's node must be its window"
    );
    assert_eq_test!(
        node_capacity(p2.as_bytes()),
        Some(P2.1),
        "partition 2's node must be its window"
    );

    let verdict = check_links(&p2, disk_tag);
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }
    let verdict = check_claims(disk, &p1, &p2, device.as_ref());
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }
    let verdict = check_read_claims(disk, &p1);
    if !matches!(verdict, TestResult::Pass) {
        return verdict;
    }

    let whole = match block::claim(disk) {
        Ok(c) => c,
        Err(e) => return fail!("the whole disk was not released: {:?}", e),
    };
    if let Err(why) = wipe_gpt(whole.as_ref(), block, blocks) {
        return fail!("{}", why);
    }
    drop(whole);
    if let Err(e) = block::reread(disk) {
        return fail!("the re-read after the wipe failed: {:?}", e);
    }
    assert_test!(
        node_capacity(p1.as_bytes()).is_none(),
        "a wiped table must leave no partition nodes"
    );
    pass!()
}

/// Partition 2 resolves by `PARTUUID=` and through its by-partuuid link.
#[inline(never)]
fn check_links(p2: &DiskName, disk_tag: u8) -> TestResult {
    let mut text = [0u8; PARTUUID_TEXT_MAX];
    format_guid(&unique_guid(disk_tag, 2), &mut text);
    let mut spec = [0u8; 9 + PARTUUID_TEXT_MAX];
    spec[..9].copy_from_slice(b"PARTUUID=");
    spec[9..].copy_from_slice(&text);
    let mut resolved = [0u8; DEV_NAME_MAX];
    match devfs_resolve_block_source(&spec, &mut resolved) {
        Ok(len) if resolved[..len] == *p2.as_bytes() => {}
        other => return fail!("PARTUUID= resolved to {:?}", other),
    }
    let mut link = [0u8; 22 + PARTUUID_TEXT_MAX];
    link[..22].copy_from_slice(b"/dev/disk/by-partuuid/");
    link[22..].copy_from_slice(&text);
    match devfs_resolve_block_source(&link, &mut resolved) {
        Ok(len) if resolved[..len] == *p2.as_bytes() => {}
        other => return fail!("the by-partuuid link resolved to {:?}", other),
    }

    pass!()
}

/// Two partitions claim side by side; either excludes the whole disk and a
/// re-read; a partition's writes land inside its window.
#[inline(never)]
fn check_claims(
    disk: &[u8],
    p1: &DiskName,
    p2: &DiskName,
    device: &(dyn BlockDevice + Send + Sync),
) -> TestResult {
    let first = match block::claim(p1.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("claiming partition 1 failed: {:?}", e),
    };
    let second = match block::claim(p2.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("partition 2 must be claimable beside partition 1: {:?}", e),
    };
    assert_test!(
        matches!(block::claim(p1.as_bytes()), Err(ClaimError::Busy)),
        "a claimed partition must refuse a second claim"
    );
    assert_test!(
        matches!(block::claim(disk), Err(ClaimError::Busy)),
        "the whole disk must refuse a claim while a partition is claimed"
    );
    assert_test!(
        matches!(block::reread(disk), Err(RereadError::Busy)),
        "a re-read must be refused while a partition is claimed"
    );
    assert_test!(
        matches!(block::reread(p1.as_bytes()), Err(RereadError::NotWholeDisk)),
        "a partition has no table to re-read"
    );

    let Ok(mut stamp) = KVec::<u8>::zeroed(512) else {
        return fail!("stamp alloc");
    };
    stamp.fill(0x9D);
    assert_test!(
        second.write_at(0, &stamp).is_ok(),
        "a write through the partition's claim"
    );
    let Ok(mut parent) = KVec::<u8>::zeroed(512) else {
        return fail!("readback alloc");
    };
    assert_test!(
        device.read_at(P2.0, &mut parent).is_ok() && parent[..] == stamp[..],
        "a partition write must land at the partition's start on the disk"
    );
    assert_test!(
        second.write_at(P2.1, &stamp).is_err(),
        "a write past the window must be refused"
    );
    drop((first, second));

    pass!()
}

/// Readers of a partition share it with each other and with a reader of the
/// whole disk, and hold a writer and a re-read off it; a read claim writes
/// nothing.
#[inline(never)]
fn check_read_claims(disk: &[u8], p1: &DiskName) -> TestResult {
    let (first, second, whole) = match (
        block::claim_read(p1.as_bytes()),
        block::claim_read(p1.as_bytes()),
        block::claim_read(disk),
    ) {
        (Ok(first), Ok(second), Ok(whole)) => (first, second, whole),
        other => {
            return fail!(
                "readers must share: {:?}",
                (other.0.err(), other.1.err(), other.2.err())
            );
        }
    };
    assert_test!(
        matches!(block::claim(p1.as_bytes()), Err(ClaimError::Busy)),
        "a partition being read must refuse a writer"
    );
    assert_test!(
        matches!(block::claim(disk), Err(ClaimError::Busy)),
        "a disk being read must refuse a writer"
    );
    assert_test!(
        matches!(block::reread(disk), Err(RereadError::Busy)),
        "a re-read must be refused while a partition is read"
    );
    assert_test!(
        first.write_protected() && first.write_at(0, &[0u8; 512]).is_err(),
        "a read claim must refuse writes"
    );
    drop((first, second, whole));

    let writer = match block::claim(p1.as_bytes()) {
        Ok(c) => c,
        Err(e) => return fail!("the readers did not let go: {:?}", e),
    };
    assert_test!(
        matches!(block::claim_read(p1.as_bytes()), Err(ClaimError::Busy)),
        "a partition being written must refuse a reader"
    );
    assert_test!(
        matches!(block::claim_read(disk), Err(ClaimError::Busy)),
        "a disk with a partition being written must refuse a reader"
    );
    drop(writer);
    pass!()
}

pub fn test_block_partitions_on_512_byte_blocks() -> TestResult {
    partitioned_disk(b"nvme0n2", 0xA5)
}

pub fn test_block_partitions_on_4096_byte_blocks() -> TestResult {
    partitioned_disk(b"nvme1n2", 0x4C)
}

/// The labelled volume the harness attaches resolves by label and by UUID,
/// and `/dev/disk/by-label` lists it as a link to its node.
pub fn test_block_volume_links() -> TestResult {
    let mut resolved = [0u8; DEV_NAME_MAX];
    match devfs_resolve_block_source(b"LABEL=slopos-media", &mut resolved) {
        Ok(len) if resolved[..len] == *b"nvme1n1" => {}
        other => return fail!("LABEL=slopos-media resolved to {:?}", other),
    }
    let Some(media) = block::disk(b"nvme1n1") else {
        return fail!("the media disk is not attached");
    };
    let mut sb = [0u8; 136];
    assert_test!(media.read_at(1024, &mut sb).is_ok(), "superblock read");
    let mut uuid = [0u8; 5 + 36];
    uuid[..5].copy_from_slice(b"UUID=");
    let mut at = 5;
    for (i, byte) in sb[104..120].iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            uuid[at] = b'-';
            at += 1;
        }
        uuid[at..at + 2].copy_from_slice(&[
            b"0123456789ABCDEF"[usize::from(byte >> 4)],
            b"0123456789ABCDEF"[usize::from(byte & 0xF)],
        ]);
        at += 2;
    }
    match devfs_resolve_block_source(&uuid, &mut resolved) {
        Ok(len) if resolved[..len] == *b"nvme1n1" => {}
        other => return fail!("an uppercase UUID= resolved to {:?}", other),
    }

    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let lookup = |parent, name: &[u8]| fs.lookup(parent, name);
    let Ok(disk_dir) = lookup(fs.root_inode(), b"disk") else {
        return fail!("/dev/disk is missing");
    };
    let Ok(by_label) = lookup(disk_dir, b"by-label") else {
        return fail!("/dev/disk/by-label is missing");
    };
    let mut listed = None;
    let walked = fs.readdir(by_label, 0, &mut |name, inode, kind| {
        if name == b"slopos-media" {
            listed = Some((inode, kind));
        }
        true
    });
    let Some((inode, FileType::Symlink)) = listed.filter(|_| walked.is_ok()) else {
        return fail!("by-label does not list slopos-media as a link");
    };
    assert_eq_test!(
        lookup(by_label, b"slopos-media").ok(),
        Some(inode),
        "the listed link must be what a lookup finds"
    );
    let mut target = [0u8; 32];
    match fs.readlink(inode, &mut target) {
        Ok(len) if target[..len] == *b"../../nvme1n1" => pass!(),
        other => fail!("the link points at {:?}", other),
    }
}

/// A block node answers its size, its block size and whether it refuses
/// writes; an unknown request, or an inode that is no block node, gets none.
pub fn test_block_ioctls() -> TestResult {
    let fs = slopos_fs::vfs::init::vfs_devfs_instance();
    let Ok(inode) = fs.lookup(fs.root_inode(), b"nvme1n2") else {
        return fail!("/dev/nvme1n2 is missing");
    };
    let Some(disk) = block::disk(b"nvme1n2") else {
        return fail!("nvme1n2 is not registered");
    };
    let ask = |request| devfs_block_ioctl(inode, request);
    assert_eq_test!(
        ask(block_ioctl::BLKSSZGET),
        Some(Ok(BlockIoctlReply::Int(4096))),
        "BLKSSZGET"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKGETSIZE64),
        Some(Ok(BlockIoctlReply::U64(disk.capacity()))),
        "BLKGETSIZE64"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKGETSIZE),
        Some(Ok(BlockIoctlReply::U64(disk.capacity() / 512))),
        "BLKGETSIZE counts 512-byte sectors"
    );
    assert_eq_test!(
        ask(block_ioctl::BLKROGET),
        Some(Ok(BlockIoctlReply::Int(0))),
        "BLKROGET"
    );
    assert_eq_test!(
        ask(0x1234),
        Some(Err(VfsError::NotSupported)),
        "an unknown request"
    );
    assert_test!(
        devfs_block_ioctl(fs.root_inode(), block_ioctl::BLKSSZGET).is_none(),
        "a non-block inode is not answered"
    );
    pass!()
}

pub fn test_block_disk_names() -> TestResult {
    let virtio = |i| DiskName::virtio(i);
    assert_test!(virtio(0).as_bytes() == b"vda", "vda");
    assert_test!(virtio(25).as_bytes() == b"vdz", "vdz");
    assert_test!(virtio(26).as_bytes() == b"vdaa", "vdaa follows vdz");
    assert_test!(virtio(701).as_bytes() == b"vdzz", "vdzz");
    assert_test!(virtio(702).as_bytes() == b"vdaaa", "vdaaa follows vdzz");
    let nvme = DiskName::nvme(1, 12);
    assert_test!(nvme.as_bytes() == b"nvme1n12", "nvme1n12");
    assert_test!(
        nvme.partition(3).as_bytes() == b"nvme1n12p3",
        "a name ending in a digit takes a p"
    );
    assert_test!(
        virtio(1).partition(128).as_bytes() == b"vdb128",
        "a name ending in a letter does not"
    );
    pass!()
}

slopos_testing::stest!(name = test_block_disk_names, suite = block_layer);
slopos_testing::stest!(
    name = test_block_partitions_on_512_byte_blocks,
    suite = block_layer
);
slopos_testing::stest!(
    name = test_block_partitions_on_4096_byte_blocks,
    suite = block_layer
);
slopos_testing::stest!(name = test_block_volume_links, suite = block_layer);
slopos_testing::stest!(name = test_block_ioctls, suite = block_layer);
