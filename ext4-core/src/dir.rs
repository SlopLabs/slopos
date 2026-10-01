//! Directory blocks: the checksum tail every leaf carries under
//! `metadata_csum`, and the shape of the htree blocks a reader that keeps
//! directories linear has to recognise.

use crate::bytes::{le16, le32, put_le16, put_le32};
use crate::crc::crc32c;

/// The tail: a record with no inode, no name and file type `0xDE`, whose
/// last four bytes are the block's checksum.
pub const TAIL_SIZE: usize = 12;
const TAIL_FILE_TYPE: u8 = 0xDE;

/// Bytes of a leaf block entries may use: all of it, less the tail under
/// checksums.
pub fn usable(block_size: usize, csum: bool) -> usize {
    if csum {
        block_size - TAIL_SIZE
    } else {
        block_size
    }
}

pub fn has_tail(block: &[u8]) -> bool {
    let at = block.len() - TAIL_SIZE;
    le32(block, at) == 0
        && usize::from(le16(block, at + 4)) == TAIL_SIZE
        && block[at + 6] == 0
        && block[at + 7] == TAIL_FILE_TYPE
}

/// Put an empty tail at the end of `block`; [`seal`] fills its checksum.
pub fn write_tail(block: &mut [u8]) {
    let at = block.len() - TAIL_SIZE;
    block[at..].fill(0);
    put_le16(block, at + 4, TAIL_SIZE as u16);
    block[at + 7] = TAIL_FILE_TYPE;
}

pub fn checksum(inode_seed: u32, block: &[u8]) -> u32 {
    crc32c(inode_seed, &block[..block.len() - TAIL_SIZE])
}

pub fn seal(inode_seed: u32, block: &mut [u8]) {
    let sum = checksum(inode_seed, block);
    let at = block.len() - 4;
    put_le32(block, at, sum);
}

pub fn verify(inode_seed: u32, block: &[u8]) -> bool {
    has_tail(block) && le32(block, block.len() - 4) == checksum(inode_seed, block)
}

/// Whether `block` is an htree interior node: one empty record spanning it,
/// the index behind.
pub fn is_htree_node(block: &[u8]) -> bool {
    le32(block, 0) == 0 && usize::from(le16(block, 4)) == block.len()
}

/// Rewrite an htree interior node as an empty leaf: one free record up to
/// the tail, and the tail.
pub fn linearize_node(block: &mut [u8], csum: bool) {
    let end = usable(block.len(), csum);
    block.fill(0);
    put_le16(block, 4, end as u16);
    if csum {
        write_tail(block);
    }
}

/// Whether `block` is an htree root: `.` and `..`, then the index's header
/// with its fixed length and zeroed reserved word.
pub fn is_htree_root(block: &[u8]) -> bool {
    block.len() >= 32
        && le16(block, 4) == 12
        && block[6] == 1
        && block[8] == b'.'
        && block[18] == 2
        && block[20..22] == *b".."
        && le32(block, 24) == 0
        && block[29] == 8
}

/// Rewrite an htree root ([`is_htree_root`]) as a linear first block: `.`
/// and `..` stay, and `..` spans the index's bytes up to the tail.
pub fn linearize_root(block: &mut [u8], csum: bool) {
    let end = usable(block.len(), csum);
    let dotdot = 12;
    let rec_len = end - dotdot;
    put_le16(block, dotdot + 4, rec_len as u16);
    let name_end = dotdot + 8 + 4;
    block[name_end..].fill(0);
    if csum {
        write_tail(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_round_trip() {
        let mut block = [0x55u8; 1024];
        write_tail(&mut block);
        assert!(has_tail(&block));
        seal(42, &mut block);
        assert!(verify(42, &block));
        block[100] ^= 1;
        assert!(!verify(42, &block));
    }

    #[test]
    fn linear_node_is_one_free_record() {
        let mut block = [0xAAu8; 4096];
        linearize_node(&mut block, true);
        assert_eq!(le32(&block, 0), 0);
        assert_eq!(usize::from(le16(&block, 4)), 4096 - TAIL_SIZE);
        assert!(has_tail(&block));
    }
}
