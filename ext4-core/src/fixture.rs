//! A single-group volume built in memory, for the kernel's own suite, where
//! no `mke2fs` runs. The host tests hold it to `e2fsck`. The one deviation:
//! a journal may be shorter than the 1024 blocks Linux and e2fsck require,
//! so a test can fill the log in a few operations.

use crate::bytes::{put_le16, put_le32};
use crate::crc::crc32c;
use crate::extent::{self, Extent};
use crate::group::{self, Desc, DescCsum};
use crate::inode::{self, InodeTime, off as ioff};
use crate::superblock::{self as sbk, compat, incompat, off as soff, ro_compat};
use crate::{dir, jbd2};

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    /// 1024 or 4096.
    pub block_size: u32,
    /// At most one group's worth: eight times the block size.
    pub blocks: u32,
    /// A multiple of 8, and at least a block of records.
    pub inodes: u32,
    /// 128 or 256.
    pub inode_size: u16,
    /// Extent trees for the root, `lost+found` and the journal.
    pub extents: bool,
    /// 64-byte descriptors.
    pub bit64: bool,
    pub metadata_csum: bool,
    /// A journal of this many blocks in inode 8; zero for none. Needs
    /// `extents`.
    pub journal_blocks: u32,
    pub uuid: [u8; 16],
}

const ROOT: u32 = 2;
const JOURNAL: u32 = 8;
const LOST_FOUND: u32 = 11;
const FIRST_INO: u32 = 11;

/// Where the builder put things, for a test that wants to reach them.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    pub first_data_block: u32,
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub root_dir: u32,
    pub lost_found_dir: u32,
    pub journal: u32,
    /// The first block nothing uses.
    pub first_free: u32,
}

/// Format `image`, which must be `spec.blocks` blocks long.
pub fn format(image: &mut [u8], spec: &Spec) -> Result<Layout, &'static str> {
    let bs = spec.block_size as usize;
    if spec.block_size != 1024 && spec.block_size != 4096 {
        return Err("block size");
    }
    if spec.blocks > spec.block_size * 8 || image.len() < spec.blocks as usize * bs {
        return Err("one group's worth of blocks");
    }
    if spec.journal_blocks > 0 && !spec.extents {
        return Err("a journal needs extents");
    }
    if !matches!(spec.inode_size, 128 | 256) {
        return Err("inode size");
    }
    let per_block = spec.block_size / u32::from(spec.inode_size);
    if !spec.inodes.is_multiple_of(8) || spec.inodes < per_block {
        return Err("whole bytes of inode bitmap and a block of records");
    }
    image[..spec.blocks as usize * bs].fill(0);
    let fdb = u32::from(spec.block_size == 1024);
    let itable_blocks = (spec.inodes * u32::from(spec.inode_size)).div_ceil(spec.block_size);
    let block_bitmap = fdb + 2;
    let inode_bitmap = fdb + 3;
    let inode_table = fdb + 4;
    let root_dir = inode_table + itable_blocks;
    let lost_found_dir = root_dir + 1;
    let journal = lost_found_dir + 1;
    let first_free = journal + spec.journal_blocks;
    if first_free >= spec.blocks || spec.inodes < FIRST_INO + 1 {
        return Err("room for the metadata");
    }
    let layout = Layout {
        first_data_block: fdb,
        block_bitmap,
        inode_bitmap,
        inode_table,
        root_dir,
        lost_found_dir,
        journal,
        first_free,
    };

    let seed = crc32c(!0, &spec.uuid);
    write_superblock(&mut image[1024..2048], spec, &layout);
    write_inodes(image, spec, &layout, seed);
    write_dirs(image, spec, &layout, seed);
    write_group(image, spec, &layout, seed);
    sbk::seal(&mut image[1024..2048]);
    Ok(layout)
}

#[inline(never)]
fn write_superblock(sb: &mut [u8], spec: &Spec, layout: &Layout) {
    let mut compat_bits = 0;
    let mut incompat_bits = incompat::FILETYPE;
    let mut ro_bits = ro_compat::SPARSE_SUPER | ro_compat::LARGE_FILE;
    if spec.journal_blocks > 0 {
        compat_bits |= compat::HAS_JOURNAL;
    }
    if spec.extents {
        incompat_bits |= incompat::EXTENTS;
    }
    if spec.bit64 {
        incompat_bits |= incompat::BIT64;
    }
    if spec.metadata_csum {
        ro_bits |= ro_compat::METADATA_CSUM;
    }
    if spec.inode_size > 128 {
        ro_bits |= ro_compat::EXTRA_ISIZE | ro_compat::HUGE_FILE | ro_compat::DIR_NLINK;
    }
    let fdb = layout.first_data_block;
    put_le32(sb, soff::INODES_COUNT, spec.inodes);
    put_le32(sb, soff::BLOCKS_COUNT_LO, spec.blocks);
    put_le32(
        sb,
        soff::FREE_BLOCKS_COUNT_LO,
        spec.blocks - layout.first_free,
    );
    put_le32(sb, soff::FREE_INODES_COUNT, spec.inodes - FIRST_INO);
    put_le32(sb, soff::FIRST_DATA_BLOCK, fdb);
    let log = if spec.block_size == 1024 { 0 } else { 2 };
    put_le32(sb, soff::LOG_BLOCK_SIZE, log);
    put_le32(sb, soff::LOG_CLUSTER_SIZE, log);
    put_le32(sb, soff::BLOCKS_PER_GROUP, spec.block_size * 8);
    put_le32(sb, soff::CLUSTERS_PER_GROUP, spec.block_size * 8);
    put_le32(sb, soff::INODES_PER_GROUP, spec.inodes);
    put_le16(sb, soff::MAX_MNT_COUNT, 0xFFFF);
    put_le16(sb, soff::MAGIC, sbk::MAGIC);
    put_le16(sb, soff::STATE, sbk::STATE_VALID);
    put_le16(sb, soff::ERRORS, 2);
    put_le32(sb, soff::REV_LEVEL, 1);
    put_le32(sb, soff::FIRST_INO, FIRST_INO);
    put_le16(sb, soff::INODE_SIZE, spec.inode_size);
    put_le32(sb, soff::FEATURE_COMPAT, compat_bits);
    put_le32(sb, soff::FEATURE_INCOMPAT, incompat_bits);
    put_le32(sb, soff::FEATURE_RO_COMPAT, ro_bits);
    sb[soff::UUID..soff::UUID + 16].copy_from_slice(&spec.uuid);
    if spec.bit64 {
        put_le16(sb, soff::DESC_SIZE, desc_size(spec) as u16);
    }
    if spec.inode_size > 128 {
        put_le16(sb, soff::MIN_EXTRA_ISIZE, 32);
        put_le16(sb, soff::WANT_EXTRA_ISIZE, 32);
    }
    if spec.metadata_csum {
        sb[soff::CHECKSUM_TYPE] = sbk::CHECKSUM_CRC32C;
    }
    if spec.journal_blocks > 0 {
        put_le32(sb, soff::JOURNAL_INUM, JOURNAL);
    }
}

fn desc_size(spec: &Spec) -> usize {
    if spec.bit64 {
        group::SIZE_64
    } else {
        group::SIZE_32
    }
}

fn block_range(spec: &Spec, n: u32) -> core::ops::Range<usize> {
    let bs = spec.block_size as usize;
    n as usize * bs..(n as usize + 1) * bs
}

#[inline(never)]
fn write_inodes(image: &mut [u8], spec: &Spec, layout: &Layout, seed: u32) {
    let isz = usize::from(spec.inode_size);
    let table = block_range(spec, layout.inode_table).start;
    let inode_at = |ino: u32| {
        let at = table + (ino - 1) as usize * isz;
        at..at + isz
    };
    write_dir_inode(&mut image[inode_at(ROOT)], spec, layout.root_dir, 3);
    write_dir_inode(
        &mut image[inode_at(LOST_FOUND)],
        spec,
        layout.lost_found_dir,
        2,
    );
    if spec.journal_blocks > 0 {
        write_journal_inode(image, inode_at(JOURNAL), spec, layout);
    }
    if spec.metadata_csum {
        for ino in [ROOT, LOST_FOUND, JOURNAL] {
            if ino != JOURNAL || spec.journal_blocks > 0 {
                inode::seal(seed, ino, &mut image[inode_at(ino)]);
            }
        }
    }
}

fn write_journal_inode(
    image: &mut [u8],
    record: core::ops::Range<usize>,
    spec: &Spec,
    layout: &Layout,
) {
    let rec = &mut image[record.clone()];
    prepare_record(rec, spec);
    put_le16(rec, ioff::MODE, 0x8000 | 0o600);
    put_le16(rec, ioff::LINKS_COUNT, 1);
    let size = u64::from(spec.journal_blocks) * u64::from(spec.block_size);
    put_le32(rec, ioff::SIZE_LO, size as u32);
    put_le32(rec, ioff::SIZE_HIGH, (size >> 32) as u32);
    put_le32(
        rec,
        ioff::BLOCKS_LO,
        spec.journal_blocks * (spec.block_size / 512),
    );
    put_le32(rec, ioff::FLAGS, inode::flags::EXTENTS);
    map_extent(rec, layout.journal, spec.journal_blocks);
    back_up_journal_map_in_superblock(image, record.start + ioff::BLOCK, size);
    write_journal_superblock(&mut image[block_range(spec, layout.journal)], spec);
}

fn back_up_journal_map_in_superblock(image: &mut [u8], i_block: usize, size: u64) {
    let backup = 1024 + soff::JNL_BLOCKS;
    image.copy_within(i_block..i_block + 60, backup);
    put_le32(image, backup + 60, (size >> 32) as u32);
    put_le32(image, backup + 64, size as u32);
    image[1024 + soff::JNL_BACKUP_TYPE] = 1;
}

#[inline(never)]
fn write_dirs(image: &mut [u8], spec: &Spec, layout: &Layout, seed: u32) {
    let usable = dir::usable(spec.block_size as usize, spec.metadata_csum);
    let d = &mut image[block_range(spec, layout.root_dir)];
    dirent(d, 0, ROOT, 12, b".", 2);
    dirent(d, 12, ROOT, 12, b"..", 2);
    dirent(d, 24, LOST_FOUND, usable - 24, b"lost+found", 2);
    finish_dir(d, spec, seed, ROOT);
    let d = &mut image[block_range(spec, layout.lost_found_dir)];
    dirent(d, 0, LOST_FOUND, 12, b".", 2);
    dirent(d, 12, ROOT, usable - 12, b"..", 2);
    finish_dir(d, spec, seed, LOST_FOUND);
}

/// The bitmaps and the one group descriptor.
#[inline(never)]
fn write_group(image: &mut [u8], spec: &Spec, layout: &Layout, seed: u32) {
    let fdb = layout.first_data_block;
    let in_group = spec.blocks - fdb;
    let bmap = &mut image[block_range(spec, layout.block_bitmap)];
    for bit in (0..layout.first_free - fdb).chain(in_group..spec.block_size * 8) {
        set(bmap, bit);
    }
    let imap = &mut image[block_range(spec, layout.inode_bitmap)];
    for bit in (0..FIRST_INO).chain(spec.inodes..spec.block_size * 8) {
        set(imap, bit);
    }

    let mut desc = Desc {
        block_bitmap: u64::from(layout.block_bitmap),
        inode_bitmap: u64::from(layout.inode_bitmap),
        inode_table: u64::from(layout.inode_table),
        free_blocks: spec.blocks - layout.first_free,
        free_inodes: spec.inodes - FIRST_INO,
        used_dirs: 2,
        itable_unused: 0,
        flags: 0,
        block_bitmap_csum: 0,
        inode_bitmap_csum: 0,
    };
    let desc_size = desc_size(spec);
    let mut kind = DescCsum::None;
    if spec.metadata_csum {
        kind = DescCsum::Crc32c { seed };
        desc.flags = group::ITABLE_ZEROED;
        desc.itable_unused = spec.inodes - FIRST_INO;
        let bmap = &image[block_range(spec, layout.block_bitmap)];
        desc.block_bitmap_csum = group::stored_bitmap_csum(
            group::bitmap_checksum(seed, bmap, spec.block_size * 8),
            desc_size,
        );
        let imap = &image[block_range(spec, layout.inode_bitmap)];
        desc.inode_bitmap_csum =
            group::stored_bitmap_csum(group::bitmap_checksum(seed, imap, spec.inodes), desc_size);
    }
    let gdt = block_range(spec, fdb + 1).start;
    desc.encode(&mut image[gdt..gdt + desc_size]);
    group::seal(kind, 0, &mut image[gdt..gdt + desc_size]);
}

fn set(bitmap: &mut [u8], bit: u32) {
    bitmap[(bit / 8) as usize] |= 1 << (bit % 8);
}

/// A new record: zeroed, its extension sized.
fn prepare_record(rec: &mut [u8], spec: &Spec) {
    rec.fill(0);
    if spec.inode_size > 128 {
        put_le16(rec, ioff::EXTRA_ISIZE, 32);
    }
    let epoch = InodeTime::new(1_700_000_000, 0);
    for (base, extra) in [
        (ioff::ATIME, ioff::ATIME_EXTRA),
        (ioff::CTIME, ioff::CTIME_EXTRA),
        (ioff::MTIME, ioff::MTIME_EXTRA),
        (ioff::CRTIME, ioff::CRTIME_EXTRA),
    ] {
        inode::put_time(rec, base, extra, epoch);
    }
}

fn map_extent(rec: &mut [u8], first: u32, len: u32) {
    extent::init_root_with(
        &mut rec[ioff::BLOCK..ioff::BLOCK + 60],
        &Extent {
            lblk: 0,
            len,
            pblk: u64::from(first),
            unwritten: false,
        },
    );
}

fn write_dir_inode(rec: &mut [u8], spec: &Spec, data: u32, links: u16) {
    prepare_record(rec, spec);
    put_le16(rec, ioff::MODE, 0x4000 | 0o755);
    put_le16(rec, ioff::LINKS_COUNT, links);
    put_le32(rec, ioff::SIZE_LO, spec.block_size);
    put_le32(rec, ioff::BLOCKS_LO, spec.block_size / 512);
    if spec.extents {
        put_le32(rec, ioff::FLAGS, inode::flags::EXTENTS);
        map_extent(rec, data, 1);
    } else {
        put_le32(rec, ioff::BLOCK, data);
    }
}

fn dirent(block: &mut [u8], at: usize, ino: u32, rec_len: usize, name: &[u8], file_type: u8) {
    put_le32(block, at, ino);
    put_le16(block, at + 4, rec_len as u16);
    block[at + 6] = name.len() as u8;
    block[at + 7] = file_type;
    block[at + 8..at + 8 + name.len()].copy_from_slice(name);
}

fn finish_dir(block: &mut [u8], spec: &Spec, fs_seed: u32, ino: u32) {
    if spec.metadata_csum {
        dir::write_tail(block);
        dir::seal(inode::seed(fs_seed, ino, 0), block);
    }
}

/// A version-2 journal superblock of an empty log, as `mke2fs` writes one.
fn write_journal_superblock(jsb: &mut [u8], spec: &Spec) {
    use crate::bytes::put_be32;
    put_be32(jsb, 0, jbd2::MAGIC);
    put_be32(jsb, 4, jbd2::blocktype::SUPERBLOCK_V2);
    put_be32(jsb, jbd2::sb_off::BLOCK_SIZE, spec.block_size);
    put_be32(jsb, jbd2::sb_off::MAXLEN, spec.journal_blocks);
    put_be32(jsb, jbd2::sb_off::FIRST, 1);
    put_be32(jsb, jbd2::sb_off::SEQUENCE, 1);
    jsb[jbd2::sb_off::UUID..jbd2::sb_off::UUID + 16].copy_from_slice(&spec.uuid);
    put_be32(jsb, jbd2::sb_off::NR_USERS, 1);
}
