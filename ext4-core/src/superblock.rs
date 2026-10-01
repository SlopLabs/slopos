//! The superblock: 1024 bytes at byte 1024 of the volume.

use crate::bytes::{le16, le32, put_le32};
use crate::crc::crc32c;

pub const OFFSET: u64 = 1024;
pub const SIZE: usize = 1024;
pub const MAGIC: u16 = 0xEF53;

pub mod off {
    pub const INODES_COUNT: usize = 0x00;
    pub const BLOCKS_COUNT_LO: usize = 0x04;
    pub const R_BLOCKS_COUNT_LO: usize = 0x08;
    pub const FREE_BLOCKS_COUNT_LO: usize = 0x0C;
    pub const FREE_INODES_COUNT: usize = 0x10;
    pub const FIRST_DATA_BLOCK: usize = 0x14;
    pub const LOG_BLOCK_SIZE: usize = 0x18;
    pub const LOG_CLUSTER_SIZE: usize = 0x1C;
    pub const BLOCKS_PER_GROUP: usize = 0x20;
    pub const CLUSTERS_PER_GROUP: usize = 0x24;
    pub const INODES_PER_GROUP: usize = 0x28;
    pub const MTIME: usize = 0x2C;
    pub const WTIME: usize = 0x30;
    pub const MNT_COUNT: usize = 0x34;
    pub const MAX_MNT_COUNT: usize = 0x36;
    pub const MAGIC: usize = 0x38;
    pub const STATE: usize = 0x3A;
    pub const ERRORS: usize = 0x3C;
    pub const LASTCHECK: usize = 0x40;
    pub const CHECKINTERVAL: usize = 0x44;
    pub const REV_LEVEL: usize = 0x4C;
    pub const FIRST_INO: usize = 0x54;
    pub const INODE_SIZE: usize = 0x58;
    pub const FEATURE_COMPAT: usize = 0x5C;
    pub const FEATURE_INCOMPAT: usize = 0x60;
    pub const FEATURE_RO_COMPAT: usize = 0x64;
    pub const UUID: usize = 0x68;
    pub const RESERVED_GDT_BLOCKS: usize = 0xCE;
    pub const JOURNAL_UUID: usize = 0xD0;
    pub const JOURNAL_INUM: usize = 0xE0;
    pub const JOURNAL_DEV: usize = 0xE4;
    pub const LAST_ORPHAN: usize = 0xE8;
    /// 1 when `JNL_BLOCKS` holds a copy of the journal inode's map.
    pub const JNL_BACKUP_TYPE: usize = 0xFD;
    pub const DESC_SIZE: usize = 0xFE;
    pub const FIRST_META_BG: usize = 0x104;
    /// The journal inode's `i_block`, then its size high and low words.
    pub const JNL_BLOCKS: usize = 0x10C;
    pub const BLOCKS_COUNT_HI: usize = 0x150;
    pub const R_BLOCKS_COUNT_HI: usize = 0x154;
    pub const FREE_BLOCKS_COUNT_HI: usize = 0x158;
    pub const MIN_EXTRA_ISIZE: usize = 0x15C;
    pub const WANT_EXTRA_ISIZE: usize = 0x15E;
    pub const LOG_GROUPS_PER_FLEX: usize = 0x174;
    pub const CHECKSUM_TYPE: usize = 0x175;
    pub const CHECKSUM_SEED: usize = 0x270;
    pub const CHECKSUM: usize = 0x3FC;
}

pub mod compat {
    pub const DIR_PREALLOC: u32 = 0x0001;
    pub const IMAGIC_INODES: u32 = 0x0002;
    pub const HAS_JOURNAL: u32 = 0x0004;
    pub const EXT_ATTR: u32 = 0x0008;
    pub const RESIZE_INODE: u32 = 0x0010;
    pub const DIR_INDEX: u32 = 0x0020;
    pub const LAZY_BG: u32 = 0x0040;
    pub const EXCLUDE_BITMAP: u32 = 0x0100;
    pub const SPARSE_SUPER2: u32 = 0x0200;
    pub const FAST_COMMIT: u32 = 0x0400;
    pub const STABLE_INODES: u32 = 0x0800;
    pub const ORPHAN_FILE: u32 = 0x1000;
}

pub mod incompat {
    pub const COMPRESSION: u32 = 0x0001;
    pub const FILETYPE: u32 = 0x0002;
    /// The journal may hold transactions not yet written home.
    pub const RECOVER: u32 = 0x0004;
    pub const JOURNAL_DEV: u32 = 0x0008;
    pub const META_BG: u32 = 0x0010;
    pub const EXTENTS: u32 = 0x0040;
    pub const BIT64: u32 = 0x0080;
    pub const MMP: u32 = 0x0100;
    pub const FLEX_BG: u32 = 0x0200;
    pub const EA_INODE: u32 = 0x0400;
    pub const DIRDATA: u32 = 0x1000;
    pub const CSUM_SEED: u32 = 0x2000;
    pub const LARGEDIR: u32 = 0x4000;
    pub const INLINE_DATA: u32 = 0x8000;
    pub const ENCRYPT: u32 = 0x10000;
    pub const CASEFOLD: u32 = 0x20000;
}

pub mod ro_compat {
    pub const SPARSE_SUPER: u32 = 0x0001;
    pub const LARGE_FILE: u32 = 0x0002;
    pub const BTREE_DIR: u32 = 0x0004;
    pub const HUGE_FILE: u32 = 0x0008;
    pub const GDT_CSUM: u32 = 0x0010;
    pub const DIR_NLINK: u32 = 0x0020;
    pub const EXTRA_ISIZE: u32 = 0x0040;
    pub const HAS_SNAPSHOT: u32 = 0x0080;
    pub const QUOTA: u32 = 0x0100;
    pub const BIGALLOC: u32 = 0x0200;
    pub const METADATA_CSUM: u32 = 0x0400;
    pub const REPLICA: u32 = 0x0800;
    pub const READONLY: u32 = 0x1000;
    pub const PROJECT: u32 = 0x2000;
    pub const SHARED_BLOCKS: u32 = 0x4000;
    pub const VERITY: u32 = 0x8000;
    pub const ORPHAN_PRESENT: u32 = 0x10000;
}

/// `s_feature_compat` bits SlopOS writes correctly beside. The two left out
/// would misplace an allocation: `sparse_super2` moves the backups an
/// uninitialised group's bitmap is computed around, and `exclude_bitmap`
/// belongs to snapshots, which are not kept here.
pub const WRITABLE_COMPAT: u32 = !(compat::SPARSE_SUPER2 | compat::EXCLUDE_BITMAP);

/// `s_feature_incompat` bits SlopOS reads and writes. Any other one is a
/// layout it cannot represent, and the volume is refused.
pub const WRITABLE_INCOMPAT: u32 = incompat::FILETYPE
    | incompat::RECOVER
    | incompat::EXTENTS
    | incompat::BIT64
    | incompat::FLEX_BG
    | incompat::CSUM_SEED;

/// `s_feature_ro_compat` bits SlopOS writes. Any other one leaves the volume
/// readable but not writable.
pub const WRITABLE_RO_COMPAT: u32 = ro_compat::SPARSE_SUPER
    | ro_compat::LARGE_FILE
    | ro_compat::HUGE_FILE
    | ro_compat::GDT_CSUM
    | ro_compat::DIR_NLINK
    | ro_compat::EXTRA_ISIZE
    | ro_compat::METADATA_CSUM;

/// `s_state`: unmounted cleanly.
pub const STATE_VALID: u16 = 0x0001;
/// `s_state`: errors were detected.
pub const STATE_ERROR: u16 = 0x0002;

/// `s_checksum_type`: the only one the format defines.
pub const CHECKSUM_CRC32C: u8 = 1;

/// The `metadata_csum` seed every other checksum starts from: the stored
/// seed under `metadata_csum_seed`, otherwise one derived from the UUID.
pub fn csum_seed(sb: &[u8]) -> u32 {
    if le32(sb, off::FEATURE_INCOMPAT) & incompat::CSUM_SEED != 0 {
        le32(sb, off::CHECKSUM_SEED)
    } else {
        crc32c(!0, &sb[off::UUID..off::UUID + 16])
    }
}

pub fn has_metadata_csum(sb: &[u8]) -> bool {
    le32(sb, off::FEATURE_RO_COMPAT) & ro_compat::METADATA_CSUM != 0
}

/// `s_checksum` as it should read: CRC-32C of every byte before it.
pub fn checksum(sb: &[u8]) -> u32 {
    crc32c(!0, &sb[..off::CHECKSUM])
}

/// Recompute `s_checksum` after a change, when the volume carries one.
pub fn seal(sb: &mut [u8]) {
    if has_metadata_csum(sb) {
        let sum = checksum(sb);
        put_le32(sb, off::CHECKSUM, sum);
    }
}

pub fn verify(sb: &[u8]) -> bool {
    !has_metadata_csum(sb) || le32(sb, off::CHECKSUM) == checksum(sb)
}

/// Whether group `group` carries a backup superblock and descriptor table:
/// every group without `sparse_super`, otherwise 0, 1 and the powers of 3, 5
/// and 7.
pub fn group_has_super(sb: &[u8], group: u32) -> bool {
    has_backup(
        group,
        le32(sb, off::FEATURE_RO_COMPAT) & ro_compat::SPARSE_SUPER != 0,
    )
}

/// [`group_has_super`] given whether the volume is `sparse_super`.
pub fn has_backup(group: u32, sparse_super: bool) -> bool {
    if group <= 1 || !sparse_super {
        return true;
    }
    if group.is_multiple_of(2) {
        return false;
    }
    [3u32, 5, 7].iter().any(|&base| is_power_of(group, base))
}

fn is_power_of(mut n: u32, base: u32) -> bool {
    while n.is_multiple_of(base) {
        n /= base;
    }
    n == 1
}

/// The block count, its high half included under `64bit`.
pub fn blocks_count(sb: &[u8]) -> u64 {
    let lo = u64::from(le32(sb, off::BLOCKS_COUNT_LO));
    if le32(sb, off::FEATURE_INCOMPAT) & incompat::BIT64 != 0 {
        lo | (u64::from(le32(sb, off::BLOCKS_COUNT_HI)) << 32)
    } else {
        lo
    }
}

/// `s_desc_size`, which is 32 whenever `64bit` is off or the field is zero.
pub fn desc_size(sb: &[u8]) -> u16 {
    let size = le16(sb, off::DESC_SIZE);
    if le32(sb, off::FEATURE_INCOMPAT) & incompat::BIT64 == 0 || size == 0 {
        32
    } else {
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_backup_groups() {
        let mut sb = [0u8; SIZE];
        put_le32(&mut sb, off::FEATURE_RO_COMPAT, ro_compat::SPARSE_SUPER);
        let with: std::vec::Vec<u32> = (0..130).filter(|&g| group_has_super(&sb, g)).collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27, 49, 81, 125]);
        put_le32(&mut sb, off::FEATURE_RO_COMPAT, 0);
        assert!((0..10).all(|g| group_has_super(&sb, g)));
    }
}
