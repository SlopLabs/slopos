//! Block group descriptors: 32 bytes, or `s_desc_size` (64) under `64bit`,
//! and the checksums of the two bitmaps each one describes.

use crate::bytes::{le16, le32, put_le16, put_le32};
use crate::crc::{crc16, crc32c};

pub const SIZE_32: usize = 32;
pub const SIZE_64: usize = 64;

pub mod off {
    pub const BLOCK_BITMAP_LO: usize = 0x00;
    pub const INODE_BITMAP_LO: usize = 0x04;
    pub const INODE_TABLE_LO: usize = 0x08;
    pub const FREE_BLOCKS_LO: usize = 0x0C;
    pub const FREE_INODES_LO: usize = 0x0E;
    pub const USED_DIRS_LO: usize = 0x10;
    pub const FLAGS: usize = 0x12;
    pub const BLOCK_BITMAP_CSUM_LO: usize = 0x18;
    pub const INODE_BITMAP_CSUM_LO: usize = 0x1A;
    pub const ITABLE_UNUSED_LO: usize = 0x1C;
    pub const CHECKSUM: usize = 0x1E;
    pub const BLOCK_BITMAP_HI: usize = 0x20;
    pub const INODE_BITMAP_HI: usize = 0x24;
    pub const INODE_TABLE_HI: usize = 0x28;
    pub const FREE_BLOCKS_HI: usize = 0x2C;
    pub const FREE_INODES_HI: usize = 0x2E;
    pub const USED_DIRS_HI: usize = 0x30;
    pub const ITABLE_UNUSED_HI: usize = 0x32;
    pub const BLOCK_BITMAP_CSUM_HI: usize = 0x38;
    pub const INODE_BITMAP_CSUM_HI: usize = 0x3A;
}

/// `bg_flags`: the inode bitmap and table were never initialised.
pub const INODE_UNINIT: u16 = 0x0001;
/// `bg_flags`: the block bitmap is not on disk; it is computed.
pub const BLOCK_UNINIT: u16 = 0x0002;
/// `bg_flags`: the inode table is zeroed.
pub const ITABLE_ZEROED: u16 = 0x0004;

/// One descriptor with its split halves joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Desc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks: u32,
    pub free_inodes: u32,
    pub used_dirs: u32,
    pub itable_unused: u32,
    pub flags: u16,
    pub block_bitmap_csum: u32,
    pub inode_bitmap_csum: u32,
}

impl Desc {
    /// `raw` is one descriptor, `raw.len()` its on-disk size.
    pub fn parse(raw: &[u8]) -> Self {
        let wide = raw.len() >= SIZE_64;
        let hi16 = |at: usize| if wide { u32::from(le16(raw, at)) } else { 0 };
        let hi32 = |at: usize| if wide { u64::from(le32(raw, at)) } else { 0 };
        Self {
            block_bitmap: u64::from(le32(raw, off::BLOCK_BITMAP_LO))
                | (hi32(off::BLOCK_BITMAP_HI) << 32),
            inode_bitmap: u64::from(le32(raw, off::INODE_BITMAP_LO))
                | (hi32(off::INODE_BITMAP_HI) << 32),
            inode_table: u64::from(le32(raw, off::INODE_TABLE_LO))
                | (hi32(off::INODE_TABLE_HI) << 32),
            free_blocks: u32::from(le16(raw, off::FREE_BLOCKS_LO))
                | (hi16(off::FREE_BLOCKS_HI) << 16),
            free_inodes: u32::from(le16(raw, off::FREE_INODES_LO))
                | (hi16(off::FREE_INODES_HI) << 16),
            used_dirs: u32::from(le16(raw, off::USED_DIRS_LO)) | (hi16(off::USED_DIRS_HI) << 16),
            itable_unused: u32::from(le16(raw, off::ITABLE_UNUSED_LO))
                | (hi16(off::ITABLE_UNUSED_HI) << 16),
            flags: le16(raw, off::FLAGS),
            block_bitmap_csum: u32::from(le16(raw, off::BLOCK_BITMAP_CSUM_LO))
                | (hi16(off::BLOCK_BITMAP_CSUM_HI) << 16),
            inode_bitmap_csum: u32::from(le16(raw, off::INODE_BITMAP_CSUM_LO))
                | (hi16(off::INODE_BITMAP_CSUM_HI) << 16),
        }
    }

    /// Write the fields into `raw`, leaving the exclude bitmap, the reserved
    /// bytes and the checksum ([`seal`]) untouched.
    pub fn encode(&self, raw: &mut [u8]) {
        put_le32(raw, off::BLOCK_BITMAP_LO, self.block_bitmap as u32);
        put_le32(raw, off::INODE_BITMAP_LO, self.inode_bitmap as u32);
        put_le32(raw, off::INODE_TABLE_LO, self.inode_table as u32);
        put_le16(raw, off::FREE_BLOCKS_LO, self.free_blocks as u16);
        put_le16(raw, off::FREE_INODES_LO, self.free_inodes as u16);
        put_le16(raw, off::USED_DIRS_LO, self.used_dirs as u16);
        put_le16(raw, off::FLAGS, self.flags);
        put_le16(
            raw,
            off::BLOCK_BITMAP_CSUM_LO,
            self.block_bitmap_csum as u16,
        );
        put_le16(
            raw,
            off::INODE_BITMAP_CSUM_LO,
            self.inode_bitmap_csum as u16,
        );
        put_le16(raw, off::ITABLE_UNUSED_LO, self.itable_unused as u16);
        if raw.len() >= SIZE_64 {
            put_le32(raw, off::BLOCK_BITMAP_HI, (self.block_bitmap >> 32) as u32);
            put_le32(raw, off::INODE_BITMAP_HI, (self.inode_bitmap >> 32) as u32);
            put_le32(raw, off::INODE_TABLE_HI, (self.inode_table >> 32) as u32);
            put_le16(raw, off::FREE_BLOCKS_HI, (self.free_blocks >> 16) as u16);
            put_le16(raw, off::FREE_INODES_HI, (self.free_inodes >> 16) as u16);
            put_le16(raw, off::USED_DIRS_HI, (self.used_dirs >> 16) as u16);
            put_le16(
                raw,
                off::ITABLE_UNUSED_HI,
                (self.itable_unused >> 16) as u16,
            );
            put_le16(
                raw,
                off::BLOCK_BITMAP_CSUM_HI,
                (self.block_bitmap_csum >> 16) as u16,
            );
            put_le16(
                raw,
                off::INODE_BITMAP_CSUM_HI,
                (self.inode_bitmap_csum >> 16) as u16,
            );
        }
    }
}

/// Which checksum a volume's descriptors carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DescCsum {
    None,
    /// `gdt_csum`: CRC-16 seeded with the volume UUID.
    Crc16 {
        uuid: [u8; 16],
    },
    /// `metadata_csum`: the low half of a CRC-32C from the volume seed.
    Crc32c {
        seed: u32,
    },
}

impl DescCsum {
    pub fn enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// The checksum `raw`, group `group`'s descriptor, should carry: over the
/// group number and the descriptor with its own checksum field read as zero.
pub fn checksum(kind: DescCsum, group: u32, raw: &[u8]) -> u16 {
    let group = group.to_le_bytes();
    let after = off::CHECKSUM + 2;
    match kind {
        DescCsum::None => 0,
        DescCsum::Crc32c { seed } => {
            let mut crc = crc32c(seed, &group);
            crc = crc32c(crc, &raw[..off::CHECKSUM]);
            crc = crc32c(crc, &[0, 0]);
            crc = crc32c(crc, &raw[after..]);
            crc as u16
        }
        DescCsum::Crc16 { uuid } => {
            let mut crc = crc16(!0, &uuid);
            crc = crc16(crc, &group);
            crc = crc16(crc, &raw[..off::CHECKSUM]);
            if raw.len() > after {
                crc = crc16(crc, &raw[after..]);
            }
            crc
        }
    }
}

pub fn seal(kind: DescCsum, group: u32, raw: &mut [u8]) {
    if kind.enabled() {
        let sum = checksum(kind, group, raw);
        put_le16(raw, off::CHECKSUM, sum);
    }
}

pub fn verify(kind: DescCsum, group: u32, raw: &[u8]) -> bool {
    !kind.enabled() || le16(raw, off::CHECKSUM) == checksum(kind, group, raw)
}

/// The checksum of a block bitmap (`bits` = clusters per group) or an inode
/// bitmap (`bits` = inodes per group), over its first `bits / 8` bytes.
pub fn bitmap_checksum(seed: u32, bitmap: &[u8], bits: u32) -> u32 {
    let len = (bits as usize / 8).min(bitmap.len());
    crc32c(seed, &bitmap[..len])
}

/// The checksum of a bitmap whose bit `bit` just flipped, from the one it had
/// before. The change is an XOR, so a stored low half updates on its own.
pub fn flip_bitmap_checksum(sum: u32, bit: u32, bits: u32) -> u32 {
    let byte = bit as usize / 8;
    sum ^ crate::crc::crc32c_byte_delta(byte, 1 << (bit % 8), bits as usize / 8)
}

/// The part of a bitmap checksum a descriptor of `desc_size` stores.
pub fn stored_bitmap_csum(sum: u32, desc_size: usize) -> u32 {
    if desc_size >= SIZE_64 {
        sum
    } else {
        sum & 0xFFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flipped_bit_restamps_like_the_whole_bitmap() {
        let mut bitmap = [0u8; 4096];
        let mut x = 0x2545_F491u32;
        for b in bitmap.iter_mut() {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            *b = x as u8;
        }
        let bits = 32768;
        let mut sum = bitmap_checksum(7, &bitmap, bits);
        for bit in [0u32, 1, 7, 8, 4095, 16384, 32767] {
            bitmap[bit as usize / 8] ^= 1 << (bit % 8);
            sum = flip_bitmap_checksum(sum, bit, bits);
            assert_eq!(sum, bitmap_checksum(7, &bitmap, bits), "bit {bit}");
        }
        let low = stored_bitmap_csum(sum, 32);
        bitmap[9] ^= 1 << 3;
        sum = bitmap_checksum(7, &bitmap, bits);
        assert_eq!(
            stored_bitmap_csum(flip_bitmap_checksum(low, 75, bits), 32),
            stored_bitmap_csum(sum, 32)
        );
    }

    #[test]
    fn bits_of_a_partial_last_byte_are_not_checksummed() {
        let mut bitmap = [0x5Au8; 4];
        let bits = 20;
        let sum = bitmap_checksum(7, &bitmap, bits);
        bitmap[2] ^= 1;
        assert_eq!(flip_bitmap_checksum(sum, 16, bits), sum);
        assert_eq!(bitmap_checksum(7, &bitmap, bits), sum);
    }

    #[test]
    fn wide_round_trip() {
        let d = Desc {
            block_bitmap: 0x1_0000_0010,
            inode_bitmap: 0x20,
            inode_table: 0x30,
            free_blocks: 0x1_2345,
            free_inodes: 0x6789,
            used_dirs: 3,
            itable_unused: 0x1_0000,
            flags: INODE_UNINIT | ITABLE_ZEROED,
            block_bitmap_csum: 0xDEAD_BEEF,
            inode_bitmap_csum: 0x1234_5678,
        };
        let mut raw = [0u8; SIZE_64];
        d.encode(&mut raw);
        assert_eq!(Desc::parse(&raw), d);
        let mut narrow = [0u8; SIZE_32];
        d.encode(&mut narrow);
        let back = Desc::parse(&narrow);
        assert_eq!(back.block_bitmap, 0x10);
        assert_eq!(back.free_blocks, 0x2345);
        assert_eq!(back.block_bitmap_csum, 0xBEEF);
    }

    #[test]
    fn seal_then_verify() {
        let mut raw = [7u8; SIZE_64];
        let kind = DescCsum::Crc32c { seed: 0x1234 };
        seal(kind, 9, &mut raw);
        assert!(verify(kind, 9, &raw));
        assert!(!verify(kind, 10, &raw));
        raw[3] ^= 1;
        assert!(!verify(kind, 9, &raw));
    }
}
