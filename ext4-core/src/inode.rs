//! Inode records: 128 bytes of the original layout, then `i_extra_isize`
//! bytes of extension in a larger record.

use crate::bytes::{le16, le32, put_le16, put_le32};
use crate::crc::crc32c;

pub const GOOD_OLD_SIZE: usize = 128;

pub mod off {
    pub const MODE: usize = 0x00;
    pub const UID: usize = 0x02;
    pub const SIZE_LO: usize = 0x04;
    pub const ATIME: usize = 0x08;
    pub const CTIME: usize = 0x0C;
    pub const MTIME: usize = 0x10;
    pub const DTIME: usize = 0x14;
    pub const GID: usize = 0x18;
    pub const LINKS_COUNT: usize = 0x1A;
    pub const BLOCKS_LO: usize = 0x1C;
    pub const FLAGS: usize = 0x20;
    pub const BLOCK: usize = 0x28;
    pub const GENERATION: usize = 0x64;
    pub const FILE_ACL_LO: usize = 0x68;
    pub const SIZE_HIGH: usize = 0x6C;
    pub const BLOCKS_HIGH: usize = 0x74;
    pub const FILE_ACL_HIGH: usize = 0x76;
    pub const UID_HIGH: usize = 0x78;
    pub const GID_HIGH: usize = 0x7A;
    pub const CHECKSUM_LO: usize = 0x7C;
    pub const EXTRA_ISIZE: usize = 0x80;
    pub const CHECKSUM_HI: usize = 0x82;
    pub const CTIME_EXTRA: usize = 0x84;
    pub const MTIME_EXTRA: usize = 0x88;
    pub const ATIME_EXTRA: usize = 0x8C;
    pub const CRTIME: usize = 0x90;
    pub const CRTIME_EXTRA: usize = 0x94;
}

/// Bytes of `i_block`: fifteen block pointers, or an extent tree root, or a
/// fast symlink's target.
pub const BLOCK_BYTES: usize = 60;

pub mod flags {
    pub const SYNC: u32 = 0x0000_0008;
    pub const IMMUTABLE: u32 = 0x0000_0010;
    pub const APPEND: u32 = 0x0000_0020;
    pub const NODUMP: u32 = 0x0000_0040;
    pub const NOATIME: u32 = 0x0000_0080;
    pub const ENCRYPT: u32 = 0x0000_0800;
    /// The directory carries an htree index.
    pub const INDEX: u32 = 0x0000_1000;
    pub const JOURNAL_DATA: u32 = 0x0000_4000;
    pub const DIRSYNC: u32 = 0x0001_0000;
    pub const TOPDIR: u32 = 0x0002_0000;
    /// `i_blocks` counts filesystem blocks rather than 512-byte sectors.
    pub const HUGE_FILE: u32 = 0x0004_0000;
    pub const EXTENTS: u32 = 0x0008_0000;
    pub const VERITY: u32 = 0x0010_0000;
    pub const EA_INODE: u32 = 0x0020_0000;
    pub const INLINE_DATA: u32 = 0x1000_0000;
    pub const PROJINHERIT: u32 = 0x2000_0000;
    pub const CASEFOLD: u32 = 0x4000_0000;
}

/// One timestamp as stored: the low 32 bits of the seconds in the base
/// record, and in the extension two more bits of seconds and the
/// nanoseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InodeTime {
    pub lo: u32,
    pub extra: u32,
}

const EPOCH_BITS: u32 = 2;
const EPOCH_MASK: u32 = (1 << EPOCH_BITS) - 1;

impl InodeTime {
    /// `secs` from the Unix epoch, in the 34-bit range the format carries
    /// (1901 to 2446), saturating outside it.
    pub fn new(secs: i64, nanos: u32) -> Self {
        let secs = secs.clamp(i64::from(i32::MIN), i64::from(i32::MIN) + (1 << 34) - 1);
        let lo = secs as u32;
        let epoch = ((secs - i64::from(lo as i32)) >> 32) as u32 & EPOCH_MASK;
        Self {
            lo,
            extra: epoch | (nanos.min(999_999_999) << EPOCH_BITS),
        }
    }

    pub fn secs(self) -> i64 {
        i64::from(self.lo as i32) + (i64::from(self.extra & EPOCH_MASK) << 32)
    }

    pub fn nanos(self) -> u32 {
        self.extra >> EPOCH_BITS
    }
}

/// Whether a record of `inode_size` bytes whose `i_extra_isize` is
/// `extra_isize` carries the field ending at byte `end`.
pub fn fits(inode_size: usize, extra_isize: u16, end: usize) -> bool {
    inode_size > GOOD_OLD_SIZE && GOOD_OLD_SIZE + usize::from(extra_isize) >= end
}

/// `i_extra_isize`, or zero in a 128-byte record.
pub fn extra_isize(raw: &[u8]) -> u16 {
    if raw.len() > GOOD_OLD_SIZE {
        le16(raw, off::EXTRA_ISIZE)
    } else {
        0
    }
}

/// The seed every checksum of this inode's own metadata starts from — the
/// record itself, its extent blocks and its directory blocks.
pub fn seed(fs_seed: u32, ino: u32, generation: u32) -> u32 {
    let s = crc32c(fs_seed, &ino.to_le_bytes());
    crc32c(s, &generation.to_le_bytes())
}

/// The record's checksum: over every byte with both checksum halves read as
/// zero. The high half only exists when `i_extra_isize` reaches it.
pub fn checksum(fs_seed: u32, ino: u32, raw: &[u8]) -> u32 {
    let generation = le32(raw, off::GENERATION);
    let mut crc = seed(fs_seed, ino, generation);
    crc = crc32c(crc, &raw[..off::CHECKSUM_LO]);
    crc = crc32c(crc, &[0, 0]);
    crc = crc32c(
        crc,
        &raw[off::CHECKSUM_LO + 2..GOOD_OLD_SIZE.min(raw.len())],
    );
    if raw.len() > GOOD_OLD_SIZE {
        let hi_end = off::CHECKSUM_HI + 2;
        if fits(raw.len(), extra_isize(raw), hi_end) {
            crc = crc32c(crc, &raw[GOOD_OLD_SIZE..off::CHECKSUM_HI]);
            crc = crc32c(crc, &[0, 0]);
            crc = crc32c(crc, &raw[hi_end..]);
        } else {
            crc = crc32c(crc, &raw[GOOD_OLD_SIZE..]);
        }
    }
    crc
}

fn has_checksum_hi(raw: &[u8]) -> bool {
    fits(raw.len(), extra_isize(raw), off::CHECKSUM_HI + 2)
}

pub fn seal(fs_seed: u32, ino: u32, raw: &mut [u8]) {
    let sum = checksum(fs_seed, ino, raw);
    put_le16(raw, off::CHECKSUM_LO, sum as u16);
    if has_checksum_hi(raw) {
        put_le16(raw, off::CHECKSUM_HI, (sum >> 16) as u16);
    }
}

pub fn verify(fs_seed: u32, ino: u32, raw: &[u8]) -> bool {
    let sum = checksum(fs_seed, ino, raw);
    if le16(raw, off::CHECKSUM_LO) != sum as u16 {
        return false;
    }
    !has_checksum_hi(raw) || le16(raw, off::CHECKSUM_HI) == (sum >> 16) as u16
}

/// Whether the record has room for the field ending at byte `end`.
fn carries(raw: &[u8], end: usize) -> bool {
    end <= GOOD_OLD_SIZE || fits(raw.len(), extra_isize(raw), end)
}

/// Read a timestamp: the base word at `base`, the extension at `extra` when
/// the record carries it. A creation time a record has no room for reads as
/// unset.
pub fn time(raw: &[u8], base: usize, extra: usize) -> InodeTime {
    if !carries(raw, base + 4) {
        return InodeTime::default();
    }
    let extra_word = if carries(raw, extra + 4) {
        le32(raw, extra)
    } else {
        0
    };
    InodeTime {
        lo: le32(raw, base),
        extra: extra_word,
    }
}

/// Write a timestamp. A record with no room for the extension keeps whole
/// seconds, held to the signed 32-bit range rather than wrapped; one with no
/// room for the field at all keeps nothing.
pub fn put_time(raw: &mut [u8], base: usize, extra: usize, t: InodeTime) {
    if !carries(raw, base + 4) {
        return;
    }
    if carries(raw, extra + 4) {
        put_le32(raw, base, t.lo);
        put_le32(raw, extra, t.extra);
    } else {
        let secs = t.secs().clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        put_le32(raw, base, secs as i32 as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_round_trips_across_2038() {
        for secs in [
            0i64,
            1,
            0x7FFF_FFFF,
            0x8000_0000,
            0xFFFF_FFFF,
            0x1_0000_0000,
            -1,
        ] {
            let t = InodeTime::new(secs, 123_456_789);
            assert_eq!(t.secs(), secs, "{secs}");
            assert_eq!(t.nanos(), 123_456_789);
        }
        assert_eq!(InodeTime::new(1_700_000_000, 0).lo, 1_700_000_000);
        assert_eq!(InodeTime::new(1_700_000_000, 0).extra, 0);
    }

    #[test]
    fn checksum_covers_the_extension() {
        let mut raw = [0u8; 256];
        put_le16(&mut raw, off::EXTRA_ISIZE, 32);
        put_le16(&mut raw, off::MODE, 0x81A4);
        seal(0xABCD, 12, &mut raw);
        assert!(verify(0xABCD, 12, &raw));
        assert!(!verify(0xABCD, 13, &raw));
        raw[0xA0] = 1;
        assert!(!verify(0xABCD, 12, &raw));
    }

    #[test]
    fn a_record_without_the_extension_clamps() {
        let mut raw = [0u8; 128];
        put_time(
            &mut raw,
            off::MTIME,
            off::MTIME_EXTRA,
            InodeTime::new(1 << 33, 7),
        );
        assert_eq!(
            time(&raw, off::MTIME, off::MTIME_EXTRA).secs(),
            i64::from(i32::MAX)
        );
        let mut wide = [0u8; 256];
        put_le16(&mut wide, off::EXTRA_ISIZE, 32);
        put_time(
            &mut wide,
            off::MTIME,
            off::MTIME_EXTRA,
            InodeTime::new(1 << 33, 7),
        );
        let back = time(&wide, off::MTIME, off::MTIME_EXTRA);
        assert_eq!((back.secs(), back.nanos()), (1 << 33, 7));
    }

    #[test]
    fn short_extension_leaves_hi_unchecked() {
        let mut raw = [0u8; 256];
        put_le16(&mut raw, off::EXTRA_ISIZE, 0);
        seal(1, 2, &mut raw);
        assert_eq!(le16(&raw, off::CHECKSUM_HI), 0);
        assert!(verify(1, 2, &raw));
    }
}
