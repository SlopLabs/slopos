//! The ext4 profile's own behaviour: extent trees, checksums, extended
//! timestamps, `chattr` flags, and block-mapped files on a converted volume.

use slopos_ext4_core::extent;
use slopos_testing::TestResult;

use super::{FIX_ROOT_DIR_BLOCK, ext4_image_laid_out, phase3_image, with_mounted};
use crate::blockdev::MemoryBlockDevice;
use crate::ext2::ondisk::{EXT2_IMMUTABLE_FL, EXT2_INDEX_FL, EXT4_EXTENTS_FL, InodeTime};
use crate::ext2::{Ext2Error, Ext2Fs};

const BLOCKS: u32 = 512;
const BS: u64 = 1024;
/// The first inode a create hands out: `lost+found` is 11.
const FIRST_FILE: u32 = 12;

fn run(
    device: &MemoryBlockDevice,
    body: fn(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> TestResult {
    match with_mounted(device, body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => slopos_testing::fail!("{}", msg),
    }
}

fn fixture() -> Option<(MemoryBlockDevice, slopos_ext4_core::fixture::Layout)> {
    ext4_image_laid_out(BLOCKS, 0)
}

/// Every other block written: no two runs merge, so the four extents the
/// inode holds spill into a leaf, and a truncate takes the tree back down.
pub fn test_ext4_extent_tree_grows_and_shrinks() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, extent_tree_body)
}

#[inline(never)]
fn extent_tree_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let free_before = fs.superblock().free_blocks_count;
    let ino = fs.create_file(2, b"sparse").map_err(|_| "create")?;
    for i in 0..12u8 {
        fs.write_file(ino, u64::from(i) * 2 * BS, &[i + 1; 16])
            .map_err(|_| "write")?;
    }
    if depth_of(fs, ino)? == 0 {
        return Err("twelve runs still fit the inode's four extents");
    }
    for i in 0..12u8 {
        if byte_at(fs, ino, u64::from(i) * 2 * BS)? != i + 1 {
            return Err("a written block read back other bytes");
        }
        if byte_at(fs, ino, (u64::from(i) * 2 + 1) * BS)? != 0 {
            return Err("a hole read back data");
        }
    }
    fs.truncate_file(ino, 5 * BS).map_err(|_| "truncate")?;
    if depth_of(fs, ino)? != 0 {
        return Err("three runs left the tree a level deep");
    }
    if byte_at(fs, ino, 4 * BS)? != 3 {
        return Err("the truncate took a block it should have kept");
    }
    fs.unlink_entry(2, b"sparse").map_err(|_| "unlink")?;
    if fs.superblock().free_blocks_count != free_before {
        return Err("the file's blocks and its leaf were not all given back");
    }
    Ok(())
}

#[inline(never)]
fn depth_of(fs: &mut Ext2Fs<'_>, ino: u32) -> Result<u16, &'static str> {
    let inode = fs.read_inode(ino).map_err(|_| "read inode")?;
    if inode.flags & EXT4_EXTENTS_FL == 0 {
        return Err("a new file on an extents volume is block-mapped");
    }
    extent::header(&inode.block_bytes())
        .map(|h| h.depth)
        .map_err(|_| "the inode's tree root does not parse")
}

#[inline(never)]
fn byte_at(fs: &mut Ext2Fs<'_>, ino: u32, offset: u64) -> Result<u8, &'static str> {
    let mut byte = [0u8; 1];
    fs.read_file(ino, offset, &mut byte).map_err(|_| "read")?;
    Ok(byte[0])
}

/// A record whose bytes no longer match its checksum is refused, not read.
pub fn test_ext4_inode_checksum_is_verified() -> TestResult {
    let Some((device, layout)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    if let Err(msg) = with_mounted(&device, create_and_sync) {
        return slopos_testing::fail!("{}", msg);
    }
    // In-inode xattr space: covered by the checksum and read by nothing else.
    let record = u64::from(layout.inode_table) * BS + u64::from(FIRST_FILE - 1) * 256;
    flip(&device, record + 0xF0);
    run(&device, |fs| match fs.read_inode(FIRST_FILE) {
        Err(Ext2Error::BadChecksum) => Ok(()),
        _ => Err("a corrupted record was read"),
    })
}

/// A directory block whose bytes no longer match its tail is refused.
pub fn test_ext4_directory_checksum_is_verified() -> TestResult {
    let Some((device, layout)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    // The free space past `lost+found`'s entry, which only the tail covers.
    flip(&device, u64::from(layout.root_dir) * BS + 600);
    run(&device, |fs| match fs.lookup_child(2, b"lost+found") {
        Err(Ext2Error::BadChecksum) => Ok(()),
        _ => Err("a corrupted directory block was read"),
    })
}

#[inline(never)]
fn create_and_sync(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"c").map_err(|_| "create")?;
    if ino != FIRST_FILE {
        return Err("the first create took another inode");
    }
    fs.sync().map_err(|_| "sync")
}

fn flip(device: &MemoryBlockDevice, at: u64) {
    device.with_buffer_mut(|buf| buf[at as usize] ^= 0x5A);
}

/// A 256-byte record's times carry nanoseconds and seconds past 2038.
pub fn test_ext4_times_carry_nanoseconds() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, times_body)
}

#[inline(never)]
fn times_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"t").map_err(|_| "create")?;
    let late = 1i64 << 33;
    fs.set_times(
        ino,
        Some(InodeTime::new(1_700_000_000, 123_456_789)),
        Some(InodeTime::new(late, 999_999_999)),
    )
    .map_err(|_| "set_times")?;
    let inode = fs.read_inode(ino).map_err(|_| "read inode")?;
    if (inode.atime.secs(), inode.atime.nanos()) != (1_700_000_000, 123_456_789) {
        return Err("the access time lost its nanoseconds");
    }
    if (inode.mtime.secs(), inode.mtime.nanos()) != (late, 999_999_999) {
        return Err("a time past 2038 did not survive");
    }
    if inode.crtime.secs() == 0 {
        return Err("a create stamped no birth time");
    }
    Ok(())
}

/// `chattr`'s rules: a tools-only flag is taken, an unhonoured one refused, the
/// seal moves only with authority, and a sealed inode changes nothing else.
pub fn test_ext4_inode_flags_honour_the_seal() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, flags_body)
}

#[inline(never)]
fn flags_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    use slopos_abi::fs::inode_flags::{FS_APPEND_FL, FS_NODUMP_FL};

    let ino = fs.create_file(2, b"f").map_err(|_| "create")?;
    let flags = fs.inode_flags(ino).map_err(|_| "get")?;
    if flags != EXT4_EXTENTS_FL {
        return Err("a new file shows other than its extent map");
    }
    fs.set_inode_flags(ino, flags | FS_NODUMP_FL, false)
        .map_err(|_| "nodump refused")?;
    if fs.set_inode_flags(ino, flags | FS_APPEND_FL, false) != Err(Ext2Error::UnsupportedFeature) {
        return Err("an unhonoured flag was not refused");
    }
    let sealed = flags | FS_NODUMP_FL | EXT2_IMMUTABLE_FL;
    if fs.set_inode_flags(ino, sealed, false) != Err(Ext2Error::Immutable) {
        return Err("the seal moved without the authority to move it");
    }
    fs.set_inode_flags(ino, sealed, true)
        .map_err(|_| "sealing refused")?;
    if fs.write_file(ino, 0, b"x") != Err(Ext2Error::Immutable) {
        return Err("a sealed file took a write");
    }
    if fs.set_inode_flags(ino, sealed & !FS_NODUMP_FL, true) != Err(Ext2Error::Immutable) {
        return Err("a sealed inode changed another flag");
    }
    fs.set_inode_flags(ino, flags, true)
        .map_err(|_| "unsealing refused")?;
    fs.write_file(ino, 0, b"x")
        .map_err(|_| "an unsealed file refused a write")?;
    Ok(())
}

/// An append-only file another system marked is sealed here: no write, not
/// even at its end, no truncate, unlink or rename, and the VFS sees the seal.
pub fn test_ext4_append_only_is_sealed() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, append_only_body)
}

#[inline(never)]
fn append_only_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    use slopos_abi::fs::inode_flags::FS_APPEND_FL;

    let ino = fs.create_file(2, b"log").map_err(|_| "create")?;
    fs.write_file(ino, 0, b"one").map_err(|_| "write")?;
    let mut inode = fs.read_inode(ino).map_err(|_| "read")?;
    inode.flags |= FS_APPEND_FL;
    fs.write_inode_for_test(ino, &inode).map_err(|_| "mark")?;
    if fs.write_file(ino, 3, b"two") != Err(Ext2Error::Immutable) {
        return Err("an append-only file took a write");
    }
    if fs.truncate_file(ino, 0) != Err(Ext2Error::Immutable) {
        return Err("an append-only file was truncated");
    }
    if fs.unlink_entry(2, b"log") != Err(Ext2Error::Immutable) {
        return Err("an append-only file lost its name");
    }
    if fs.rename_entry(2, b"log", 2, b"moved") != Err(Ext2Error::Immutable) {
        return Err("an append-only file was renamed");
    }
    if fs.is_sealed(ino) != Ok(true) {
        return Err("the VFS does not see the seal");
    }
    Ok(())
}

/// A volume converted to extents keeps its block-mapped files: they read
/// and grow through their block map, and new files beside them get trees.
pub fn test_ext4_block_mapped_files_live_beside_extent_files() -> TestResult {
    let Some(device) = phase3_image(b"old.txt", b"from before extents") else {
        return TestResult::Skipped;
    };
    device.with_buffer_mut(|buf| {
        let incompat = &mut buf[1024 + 0x60..1024 + 0x64];
        let bits = u32::from_le_bytes([incompat[0], incompat[1], incompat[2], incompat[3]]);
        incompat.copy_from_slice(
            &(bits | slopos_ext4_core::superblock::incompat::EXTENTS).to_le_bytes(),
        );
    });
    run(&device, mixed_body)
}

#[inline(never)]
fn mixed_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let old = fs
        .resolve_path(b"/old.txt")
        .map_err(|_| "the old file is gone")?;
    if fs.read_inode(old).map_err(|_| "read")?.flags & EXT4_EXTENTS_FL != 0 {
        return Err("the conversion rewrote the old file's map");
    }
    let mut head = [0u8; 4];
    fs.read_file(old, 0, &mut head).map_err(|_| "read old")?;
    if &head != b"from" {
        return Err("the old file reads other bytes");
    }
    fs.write_file(old, 3 * BS, b"grown")
        .map_err(|_| "grow old")?;
    if byte_at(fs, old, 3 * BS)? != b'g' {
        return Err("the old file did not grow through its block map");
    }
    let new = fs.create_file(2, b"new.txt").map_err(|_| "create")?;
    fs.write_file(new, 0, b"tree").map_err(|_| "write new")?;
    if fs.read_inode(new).map_err(|_| "read")?.flags & EXT4_EXTENTS_FL == 0 {
        return Err("a new file on a converted volume is block-mapped");
    }
    Ok(())
}

/// A name that reaches a reserved inode — the journal's, the resize
/// inode's — is damage, not a file to open.
pub fn test_ext4_a_name_never_reaches_a_reserved_inode() -> TestResult {
    let Some(device) = phase3_image(b"r.txt", b"x") else {
        return TestResult::Skipped;
    };
    // The fixture's file entry follows `.` and `..`.
    let entry = FIX_ROOT_DIR_BLOCK as usize * BS as usize + 24;
    device.with_buffer_mut(|buf| buf[entry..entry + 4].copy_from_slice(&8u32.to_le_bytes()));
    run(&device, |fs| match fs.lookup_child(2, b"r.txt") {
        Err(Ext2Error::DirectoryFormat) => Ok(()),
        _ => Err("a name reached inode 8"),
    })
}

/// What the logged superblock says about the volume's free blocks: not what
/// the fixture's own superblock says, so the test sees which one it has.
const LOGGED_FREE: u32 = 123;

/// After a replay that rewrites the superblock, the mount holds the replayed
/// copy: the one it read before, written back, would undo the replay.
pub fn test_ext4_replay_rereads_the_superblock() -> TestResult {
    let Some((device, layout)) = ext4_image_laid_out(BLOCKS, 48) else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    if let Err(msg) = log_the_superblock(&device, layout.journal) {
        return slopos_testing::fail!("{}", msg);
    }
    run(&device, |fs| match fs.attach_journal() {
        Ok(Some(r)) if r.transactions == 1 => {
            if fs.superblock().free_blocks_count == LOGGED_FREE {
                Ok(())
            } else {
                Err("the mount kept the superblock it read before the replay")
            }
        }
        _ => Err("the logged transaction did not replay"),
    })
}

/// One committed transaction logging the superblock (block 1 at 1 KiB blocks)
/// with another free count, on a volume flagged as needing recovery.
#[inline(never)]
fn log_the_superblock(device: &MemoryBlockDevice, journal: u32) -> Result<(), &'static str> {
    use slopos_ext4_core::bytes::{le32, put_le32};
    use slopos_ext4_core::jbd2::{self, Tag, feature};
    use slopos_ext4_core::superblock::{self as sbk, incompat, off};
    use slopos_ostd::KVec;

    let bs = BS as usize;
    let mut copy = KVec::<u8>::zeroed(bs).map_err(|_| "buffer")?;
    let mut descriptor = KVec::<u8>::zeroed(bs).map_err(|_| "buffer")?;
    let mut commit = KVec::<u8>::zeroed(bs).map_err(|_| "buffer")?;
    device.with_buffer_mut(|buf| {
        let sb = 1024..2048;
        copy.as_mut_slice().copy_from_slice(&buf[sb.clone()]);
        put_le32(copy.as_mut_slice(), off::FREE_BLOCKS_COUNT_LO, LOGGED_FREE);
        sbk::seal(copy.as_mut_slice());
        let flags = le32(&buf[sb.clone()], off::FEATURE_INCOMPAT) | incompat::RECOVER;
        put_le32(&mut buf[sb.clone()], off::FEATURE_INCOMPAT, flags);
        sbk::seal(&mut buf[sb]);

        let at = |i: u32| (journal + i) as usize * bs;
        let jsb = &mut buf[at(0)..at(0) + bs];
        jbd2::set_features(jsb, feature::INCOMPAT_CSUM_V3 | feature::INCOMPAT_64BIT);
        jbd2::set_log_state(jsb, 1, 1);
        let fmt = jbd2::Superblock::parse(jsb)
            .map_err(|_| "journal superblock")?
            .format();
        jbd2::begin_descriptor(descriptor.as_mut_slice(), 1);
        let tag = Tag {
            block: 1,
            flags: 0,
            checksum: fmt.data_checksum(1, copy.as_slice()),
        };
        jbd2::put_tag(&fmt, descriptor.as_mut_slice(), 0, 1, tag);
        jbd2::seal_descriptor(&fmt, descriptor.as_mut_slice());
        jbd2::encode_commit(&fmt, commit.as_mut_slice(), 1, 1_700_000_000, 0);
        buf[at(1)..at(1) + bs].copy_from_slice(descriptor.as_slice());
        buf[at(2)..at(2) + bs].copy_from_slice(copy.as_slice());
        buf[at(3)..at(3) + bs].copy_from_slice(commit.as_slice());
        Ok(())
    })
}

/// A block mapped for a write whose tree then cannot grow to hold it is
/// given back: nothing stays allocated that no extent names.
pub fn test_ext4_a_mapping_that_cannot_grow_leaks_nothing() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, unmappable_body)
}

#[inline(never)]
fn unmappable_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"four").map_err(|_| "create")?;
    for i in 0..u64::from(extent::ROOT_MAX) {
        fs.write_file(ino, 2 * i * BS, b"x").map_err(|_| "write")?;
    }
    let filler = fs.create_file(2, b"filler").map_err(|_| "create filler")?;
    let chunk = slopos_ostd::KVec::<u8>::zeroed(BS as usize).map_err(|_| "buffer")?;
    let mut end = 0u64;
    while fs.write_file(filler, end, chunk.as_slice()) == Ok(BS as usize) {
        end += BS;
    }
    fs.truncate_file(filler, end - BS)
        .map_err(|_| "truncate filler")?;
    if fs.superblock().free_blocks_count != 1 {
        return Err("the fixture did not leave exactly one block free");
    }
    if fs.write_file(ino, 8 * BS, b"y") != Err(Ext2Error::NoSpace) {
        return Err("a write whose tree had no room to grow did not fail");
    }
    if fs.superblock().free_blocks_count != 1 {
        return Err("the refused write kept the block it had mapped");
    }
    if depth_of(fs, ino)? != 0 {
        return Err("the refused write left the tree a level deeper");
    }
    Ok(())
}

/// An uncounted directory (link count 1) takes no subdirectory and keeps the 1
/// through an rmdir; an indexed one takes no entry rather than go linear.
pub fn test_ext4_an_uncounted_directory_stays_uncounted() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, uncounted_body)
}

#[inline(never)]
fn uncounted_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let dir = fs.create_directory(2, b"wide").map_err(|_| "mkdir")?;
    fs.create_directory(dir, b"a").map_err(|_| "mkdir inside")?;
    stop_counting(fs, dir, 0)?;
    if fs.create_directory(dir, b"b") != Err(Ext2Error::TooManyLinks) {
        return Err("a subdirectory was added to an uncounted directory");
    }
    fs.remove_directory(dir, b"a").map_err(|_| "rmdir inside")?;
    if links_of(fs, dir)? != 1 {
        return Err("an rmdir dropped an uncounted directory's count");
    }
    stop_counting(fs, dir, EXT2_INDEX_FL)?;
    if fs.create_file(dir, b"f") != Err(Ext2Error::TooManyLinks) {
        return Err("an uncounted indexed directory took an entry");
    }
    if !fs.read_inode(dir).map_err(|_| "read")?.is_indexed() {
        return Err("an uncounted directory was made linear");
    }
    Ok(())
}

#[inline(never)]
fn stop_counting(fs: &mut Ext2Fs<'_>, dir: u32, flags: u32) -> Result<(), &'static str> {
    let mut inode = fs.read_inode(dir).map_err(|_| "read")?;
    inode.links_count = 1;
    inode.flags |= flags;
    fs.write_inode_for_test(dir, &inode).map_err(|_| "write")
}

#[inline(never)]
fn links_of(fs: &mut Ext2Fs<'_>, ino: u32) -> Result<u16, &'static str> {
    Ok(fs.read_inode(ino).map_err(|_| "read")?.links_count)
}

/// An orphan list whose head is a reserved inode — the journal's — is
/// damage, which the drain leaves to `e2fsck` rather than freeing it.
pub fn test_ext4_an_orphan_list_never_reaches_a_reserved_inode() -> TestResult {
    use slopos_ext4_core::bytes::put_le32;
    use slopos_ext4_core::superblock::{self as sbk, off};

    let Some((device, _)) = ext4_image_laid_out(BLOCKS, 48) else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    device.with_buffer_mut(|buf| {
        put_le32(&mut buf[1024..2048], off::LAST_ORPHAN, 8);
        sbk::seal(&mut buf[1024..2048]);
    });
    run(&device, |fs| {
        if fs.attach_journal().is_err() {
            return Err("the journal would not attach");
        }
        if fs.drain_orphans() != Ok(0) {
            return Err("the drain freed something");
        }
        match fs.read_inode(8) {
            Ok(journal) if journal.mode != 0 => Ok(()),
            _ => Err("the journal's inode went with the orphan list"),
        }
    })
}

/// A record whose length runs over the checksum tail would hand an insert
/// the tail's bytes; the block is refused when it is first read.
pub fn test_ext4_a_record_spanning_the_tail_is_refused() -> TestResult {
    use slopos_ext4_core::bytes::put_le16;
    use slopos_ext4_core::{crc32c, dir, inode};

    let Some((device, layout)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    let fs_seed = crc32c(!0, b"slopos-ext4-test");
    device.with_buffer_mut(|buf| {
        let at = layout.root_dir as usize * BS as usize;
        let block = &mut buf[at..at + BS as usize];
        // Stretch `lost+found`, the last record, over the tail.
        put_le16(block, 24 + 4, BS as u16 - 24);
        dir::seal(inode::seed(fs_seed, 2, 0), block);
    });
    run(&device, |fs| match fs.lookup_child(2, b"lost+found") {
        Err(Ext2Error::DirectoryFormat) => Ok(()),
        _ => Err("a block whose records run over its tail was read"),
    })
}

/// A tree that maps a file block onto the superblock is damage: reading
/// through it is refused, so writing through it never happens.
pub fn test_ext4_an_extent_never_reaches_the_superblock() -> TestResult {
    let Some((device, _)) = fixture() else {
        return slopos_testing::fail!("the ext4 fixture did not build");
    };
    run(&device, superblock_extent_body)
}

#[inline(never)]
fn superblock_extent_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"aimed").map_err(|_| "create")?;
    fs.write_file(ino, 0, b"x").map_err(|_| "write")?;
    let mut inode = fs.read_inode(ino).map_err(|_| "read")?;
    let mut root = inode.block_bytes();
    extent::init_root_with(
        &mut root,
        &extent::Extent {
            lblk: 0,
            len: 1,
            pblk: 1,
            unwritten: false,
        },
    );
    inode.set_block_bytes(&root);
    fs.write_inode_for_test(ino, &inode)
        .map_err(|_| "write inode")?;
    let mut byte = [0u8; 1];
    match fs.read_file(ino, 0, &mut byte) {
        Err(Ext2Error::InvalidBlock) => Ok(()),
        _ => Err("a read went through an extent naming the superblock"),
    }
}

slopos_testing::stest!(name = test_ext4_replay_rereads_the_superblock, suite = fs);
slopos_testing::stest!(
    name = test_ext4_an_orphan_list_never_reaches_a_reserved_inode,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext4_a_record_spanning_the_tail_is_refused,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext4_an_extent_never_reaches_the_superblock,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext4_a_mapping_that_cannot_grow_leaks_nothing,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext4_an_uncounted_directory_stays_uncounted,
    suite = fs
);
slopos_testing::stest!(name = test_ext4_extent_tree_grows_and_shrinks, suite = fs);
slopos_testing::stest!(name = test_ext4_inode_checksum_is_verified, suite = fs);
slopos_testing::stest!(name = test_ext4_directory_checksum_is_verified, suite = fs);
slopos_testing::stest!(name = test_ext4_times_carry_nanoseconds, suite = fs);
slopos_testing::stest!(name = test_ext4_inode_flags_honour_the_seal, suite = fs);
slopos_testing::stest!(name = test_ext4_append_only_is_sealed, suite = fs);
slopos_testing::stest!(
    name = test_ext4_block_mapped_files_live_beside_extent_files,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext4_a_name_never_reaches_a_reserved_inode,
    suite = fs
);
