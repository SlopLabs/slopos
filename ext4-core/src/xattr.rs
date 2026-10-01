//! The external extended-attribute block an inode's `i_file_acl` names.
//! Several inodes may share one; freeing an inode gives up its reference.

use crate::bytes::{le32, put_le32};
use crate::crc::crc32c;

pub const MAGIC: u32 = 0xEA02_0000;

pub mod off {
    pub const MAGIC: usize = 0x00;
    pub const REFCOUNT: usize = 0x04;
    pub const BLOCKS: usize = 0x08;
    pub const CHECKSUM: usize = 0x10;
}

pub fn is_xattr_block(block: &[u8]) -> bool {
    le32(block, off::MAGIC) == MAGIC && le32(block, off::BLOCKS) == 1
}

pub fn refcount(block: &[u8]) -> u32 {
    le32(block, off::REFCOUNT)
}

pub fn set_refcount(block: &mut [u8], count: u32) {
    put_le32(block, off::REFCOUNT, count);
}

/// Seeded with the block's own number rather than an inode's, since the
/// block is shared.
pub fn checksum(fs_seed: u32, block_nr: u64, block: &[u8]) -> u32 {
    let mut crc = crc32c(fs_seed, &block_nr.to_le_bytes());
    crc = crc32c(crc, &block[..off::CHECKSUM]);
    crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &block[off::CHECKSUM + 4..])
}

pub fn seal(fs_seed: u32, block_nr: u64, block: &mut [u8]) {
    let sum = checksum(fs_seed, block_nr, block);
    put_le32(block, off::CHECKSUM, sum);
}

pub fn verify(fs_seed: u32, block_nr: u64, block: &[u8]) -> bool {
    le32(block, off::CHECKSUM) == checksum(fs_seed, block_nr, block)
}
