//! The jbd2 journal's on-disk blocks. Every field is big-endian.
//!
//! A transaction is descriptor blocks, each followed by the copies of the
//! blocks its tags name, then revoke blocks, then one commit block. The
//! journal superblock says where the oldest live transaction starts
//! (`s_start`, zero when nothing needs replaying) and which sequence number
//! it carries.

use crate::bytes::{be16, be32, be64, put_be16, put_be32, put_be64};
use crate::crc::crc32c;

pub const MAGIC: u32 = 0xC03B_3998;
pub const HEADER: usize = 12;

pub mod blocktype {
    pub const DESCRIPTOR: u32 = 1;
    pub const COMMIT: u32 = 2;
    pub const SUPERBLOCK_V1: u32 = 3;
    pub const SUPERBLOCK_V2: u32 = 4;
    pub const REVOKE: u32 = 5;
}

pub mod feature {
    pub const COMPAT_CHECKSUM: u32 = 0x1;
    pub const INCOMPAT_REVOKE: u32 = 0x1;
    pub const INCOMPAT_64BIT: u32 = 0x2;
    pub const INCOMPAT_ASYNC_COMMIT: u32 = 0x4;
    pub const INCOMPAT_CSUM_V2: u32 = 0x8;
    pub const INCOMPAT_CSUM_V3: u32 = 0x10;
    pub const INCOMPAT_FAST_COMMIT: u32 = 0x20;
}

pub mod tag_flag {
    /// The block began with [`super::MAGIC`]; its copy has those four bytes
    /// zeroed.
    pub const ESCAPE: u32 = 0x1;
    /// The tag carries no UUID of its own.
    pub const SAME_UUID: u32 = 0x2;
    pub const DELETED: u32 = 0x4;
    pub const LAST_TAG: u32 = 0x8;
}

/// `s_checksum_type` for CRC-32C.
pub const CHECKSUM_CRC32C: u8 = 4;

pub mod sb_off {
    pub const BLOCK_SIZE: usize = 0x0C;
    pub const MAXLEN: usize = 0x10;
    pub const FIRST: usize = 0x14;
    pub const SEQUENCE: usize = 0x18;
    pub const START: usize = 0x1C;
    pub const ERRNO: usize = 0x20;
    pub const FEATURE_COMPAT: usize = 0x24;
    pub const FEATURE_INCOMPAT: usize = 0x28;
    pub const FEATURE_RO_COMPAT: usize = 0x2C;
    pub const UUID: usize = 0x30;
    pub const NR_USERS: usize = 0x40;
    pub const CHECKSUM_TYPE: usize = 0x50;
    pub const NUM_FC_BLOCKS: usize = 0x54;
    pub const CHECKSUM: usize = 0xFC;
}

/// Bytes of the superblock its checksum covers.
pub const SUPERBLOCK_SIZE: usize = 1024;

const UUID_SIZE: usize = 16;

/// Why a journal cannot be used or replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalError {
    /// The superblock is not a jbd2 superblock, or contradicts itself or the
    /// filesystem it belongs to.
    BadSuperblock,
    /// The journal uses a feature this implementation does not write or
    /// replay.
    Unsupported,
}

/// Which per-block checksums the journal carries. Version 2, which no current
/// tool writes, is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Csum {
    None,
    V3,
}

/// Incompat features this implementation replays and writes.
pub const KNOWN_INCOMPAT: u32 =
    feature::INCOMPAT_REVOKE | feature::INCOMPAT_64BIT | feature::INCOMPAT_CSUM_V3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    pub block_size: u32,
    pub maxlen: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub errno: i32,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub uuid: [u8; UUID_SIZE],
}

impl Superblock {
    pub fn parse(jsb: &[u8]) -> Result<Self, JournalError> {
        if jsb.len() < SUPERBLOCK_SIZE || be32(jsb, 0) != MAGIC {
            return Err(JournalError::BadSuperblock);
        }
        let v2 = match be32(jsb, 4) {
            blocktype::SUPERBLOCK_V1 => false,
            blocktype::SUPERBLOCK_V2 => true,
            _ => return Err(JournalError::BadSuperblock),
        };
        let mut uuid = [0u8; UUID_SIZE];
        uuid.copy_from_slice(&jsb[sb_off::UUID..sb_off::UUID + UUID_SIZE]);
        let sb = Self {
            block_size: be32(jsb, sb_off::BLOCK_SIZE),
            maxlen: be32(jsb, sb_off::MAXLEN),
            first: be32(jsb, sb_off::FIRST),
            sequence: be32(jsb, sb_off::SEQUENCE),
            start: be32(jsb, sb_off::START),
            errno: be32(jsb, sb_off::ERRNO) as i32,
            compat: if v2 {
                be32(jsb, sb_off::FEATURE_COMPAT)
            } else {
                0
            },
            incompat: if v2 {
                be32(jsb, sb_off::FEATURE_INCOMPAT)
            } else {
                0
            },
            ro_compat: if v2 {
                be32(jsb, sb_off::FEATURE_RO_COMPAT)
            } else {
                0
            },
            uuid,
        };
        if sb.first == 0 || sb.first >= sb.maxlen || sb.start >= sb.maxlen {
            return Err(JournalError::BadSuperblock);
        }
        let csum_v2_or_v3 = sb.incompat & (feature::INCOMPAT_CSUM_V2 | feature::INCOMPAT_CSUM_V3);
        if csum_v2_or_v3 != 0
            && (jsb[sb_off::CHECKSUM_TYPE] != CHECKSUM_CRC32C || !verify_superblock(jsb))
        {
            return Err(JournalError::BadSuperblock);
        }
        Ok(sb)
    }

    pub fn csum(&self) -> Csum {
        if self.incompat & feature::INCOMPAT_CSUM_V3 != 0 {
            Csum::V3
        } else {
            Csum::None
        }
    }

    /// Whether every feature it declares is one this implementation honours.
    pub fn supported(&self) -> bool {
        self.compat & feature::COMPAT_CHECKSUM == 0
            && self.incompat & !KNOWN_INCOMPAT == 0
            && self.ro_compat == 0
    }

    pub fn format(&self) -> Format {
        Format {
            block_size: self.block_size as usize,
            csum: self.csum(),
            bit64: self.incompat & feature::INCOMPAT_64BIT != 0,
            seed: crc32c(!0, &self.uuid),
            uuid: self.uuid,
        }
    }

    /// The journal block after `block`, wrapping past the end of the log.
    pub fn next(&self, block: u32) -> u32 {
        if block + 1 >= self.maxlen {
            self.first
        } else {
            block + 1
        }
    }
}

pub fn superblock_checksum(jsb: &[u8]) -> u32 {
    let mut crc = crc32c(!0, &jsb[..sb_off::CHECKSUM]);
    crc = crc32c(crc, &[0; 4]);
    crc32c(crc, &jsb[sb_off::CHECKSUM + 4..SUPERBLOCK_SIZE])
}

fn verify_superblock(jsb: &[u8]) -> bool {
    be32(jsb, sb_off::CHECKSUM) == superblock_checksum(jsb)
}

/// Record where the live log starts and the sequence it expects there, and
/// restamp the checksum. `start` zero says nothing needs replaying.
pub fn set_log_state(jsb: &mut [u8], sequence: u32, start: u32) {
    put_be32(jsb, sb_off::SEQUENCE, sequence);
    put_be32(jsb, sb_off::START, start);
    seal_superblock(jsb);
}

/// Turn on the features a writer's blocks will rely on. A version-1
/// superblock has no feature words, so it is promoted.
pub fn set_features(jsb: &mut [u8], incompat: u32) {
    put_be32(jsb, 4, blocktype::SUPERBLOCK_V2);
    let current = be32(jsb, sb_off::FEATURE_INCOMPAT);
    put_be32(jsb, sb_off::FEATURE_INCOMPAT, current | incompat);
    if incompat & (feature::INCOMPAT_CSUM_V2 | feature::INCOMPAT_CSUM_V3) != 0 {
        jsb[sb_off::CHECKSUM_TYPE] = CHECKSUM_CRC32C;
    }
    seal_superblock(jsb);
}

pub fn seal_superblock(jsb: &mut [u8]) {
    let incompat = be32(jsb, sb_off::FEATURE_INCOMPAT);
    if incompat & (feature::INCOMPAT_CSUM_V2 | feature::INCOMPAT_CSUM_V3) != 0 {
        let sum = superblock_checksum(jsb);
        put_be32(jsb, sb_off::CHECKSUM, sum);
    }
}

/// How the journal's blocks are laid out, from its superblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub block_size: usize,
    pub csum: Csum,
    pub bit64: bool,
    /// CRC-32C of the journal UUID: where every block checksum starts.
    pub seed: u32,
    pub uuid: [u8; UUID_SIZE],
}

impl Format {
    pub fn tag_size(&self) -> usize {
        match self.csum {
            Csum::V3 => 16,
            _ if self.bit64 => 12,
            _ => 8,
        }
    }

    fn tail_size(&self) -> usize {
        if self.csum == Csum::None { 0 } else { 4 }
    }

    /// Tags one descriptor holds when only the first carries the UUID.
    pub fn tags_per_descriptor(&self) -> usize {
        (self.block_size - HEADER - UUID_SIZE - self.tail_size()) / self.tag_size()
    }

    fn revoke_record(&self) -> usize {
        if self.bit64 { 8 } else { 4 }
    }

    pub fn revokes_per_block(&self) -> usize {
        (self.block_size - REVOKE_RECORDS - self.tail_size()) / self.revoke_record()
    }

    fn tag_at(&self, i: usize) -> usize {
        HEADER + i * self.tag_size() + if i > 0 { UUID_SIZE } else { 0 }
    }

    /// The checksum a data block's tag carries: the transaction's sequence
    /// and the block as it sits in the journal, escape included.
    pub fn data_checksum(&self, sequence: u32, data: &[u8]) -> u32 {
        let crc = crc32c(self.seed, &sequence.to_be_bytes());
        crc32c(crc, data)
    }

    fn block_tail_checksum(&self, block: &[u8]) -> u32 {
        let tail = block.len() - 4;
        let crc = crc32c(self.seed, &block[..tail]);
        crc32c(crc, &[0; 4])
    }

    fn seal_tail(&self, block: &mut [u8]) {
        if self.csum != Csum::None {
            let sum = self.block_tail_checksum(block);
            let at = block.len() - 4;
            put_be32(block, at, sum);
        }
    }

    fn verify_tail(&self, block: &[u8]) -> bool {
        self.csum == Csum::None || be32(block, block.len() - 4) == self.block_tail_checksum(block)
    }
}

/// The 12-byte header every journal block but a data copy starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub blocktype: u32,
    pub sequence: u32,
}

pub fn block_header(block: &[u8]) -> Option<BlockHeader> {
    (be32(block, 0) == MAGIC).then(|| BlockHeader {
        blocktype: be32(block, 4),
        sequence: be32(block, 8),
    })
}

fn put_header(block: &mut [u8], blocktype: u32, sequence: u32) {
    put_be32(block, 0, MAGIC);
    put_be32(block, 4, blocktype);
    put_be32(block, 8, sequence);
}

/// A data block whose first word reads as the journal magic, which a scan
/// would take for a journal block.
pub fn needs_escape(data: &[u8]) -> bool {
    be32(data, 0) == MAGIC
}

pub fn escape(data: &mut [u8]) {
    data[..4].fill(0);
}

pub fn unescape(data: &mut [u8]) {
    put_be32(data, 0, MAGIC);
}

/// One tag: the filesystem block a data copy belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub block: u64,
    pub flags: u32,
    pub checksum: u32,
}

/// Start a descriptor block of `sequence`.
pub fn begin_descriptor(block: &mut [u8], sequence: u32) {
    block.fill(0);
    put_header(block, blocktype::DESCRIPTOR, sequence);
}

/// Write tag `i` of `count`. The first is followed by the journal's UUID;
/// the rest say they share it, and the last says it is last.
pub fn put_tag(fmt: &Format, block: &mut [u8], i: usize, count: usize, tag: Tag) {
    let at = fmt.tag_at(i);
    let mut flags = tag.flags & (tag_flag::ESCAPE | tag_flag::DELETED);
    if i > 0 {
        flags |= tag_flag::SAME_UUID;
    }
    if i + 1 == count {
        flags |= tag_flag::LAST_TAG;
    }
    put_be32(block, at, tag.block as u32);
    if i == 0 {
        let uuid = at + fmt.tag_size();
        block[uuid..uuid + UUID_SIZE].copy_from_slice(&fmt.uuid);
    }
    match fmt.csum {
        Csum::V3 => {
            put_be32(block, at + 4, flags);
            put_be32(block, at + 8, (tag.block >> 32) as u32);
            put_be32(block, at + 12, tag.checksum);
        }
        Csum::None => {
            put_be16(block, at + 4, 0);
            put_be16(block, at + 6, flags as u16);
            if fmt.bit64 {
                put_be32(block, at + 8, (tag.block >> 32) as u32);
            }
        }
    }
}

/// Close a descriptor: its tail checksum, once every tag is in place.
pub fn seal_descriptor(fmt: &Format, block: &mut [u8]) {
    fmt.seal_tail(block);
}

/// Walks a descriptor block's tags in order, up to the one marked last or
/// the last the block has room for, whichever comes first.
#[derive(Debug, Clone, Copy)]
pub struct TagCursor {
    at: usize,
    done: bool,
}

impl TagCursor {
    pub fn new() -> Self {
        Self {
            at: HEADER,
            done: false,
        }
    }

    pub fn next(&mut self, fmt: &Format, block: &[u8]) -> Option<Tag> {
        if self.done || self.at + fmt.tag_size() > block.len() - fmt.tail_size() {
            self.done = true;
            return None;
        }
        let at = self.at;
        let low = u64::from(be32(block, at));
        let (flags, high, checksum) = match fmt.csum {
            Csum::V3 => (
                be32(block, at + 4),
                be32(block, at + 8),
                be32(block, at + 12),
            ),
            Csum::None => (
                u32::from(be16(block, at + 6)),
                if fmt.bit64 { be32(block, at + 8) } else { 0 },
                0,
            ),
        };
        self.at += fmt.tag_size();
        if flags & tag_flag::SAME_UUID == 0 {
            self.at += UUID_SIZE;
        }
        if flags & tag_flag::LAST_TAG != 0 {
            self.done = true;
        }
        Some(Tag {
            block: low | if fmt.bit64 { u64::from(high) << 32 } else { 0 },
            flags,
            checksum,
        })
    }
}

impl Default for TagCursor {
    fn default() -> Self {
        Self::new()
    }
}

/// Every tag of a descriptor block, in order; answers how many.
pub fn tags(fmt: &Format, block: &[u8], f: &mut dyn FnMut(Tag)) -> usize {
    let mut cursor = TagCursor::new();
    let mut count = 0usize;
    while let Some(tag) = cursor.next(fmt, block) {
        f(tag);
        count += 1;
    }
    count
}

pub fn verify_descriptor(fmt: &Format, block: &[u8]) -> bool {
    fmt.verify_tail(block)
}

/// Whether a data copy matches the checksum its tag recorded.
pub fn verify_data(fmt: &Format, tag: &Tag, sequence: u32, data: &[u8]) -> bool {
    fmt.csum == Csum::None || tag.checksum == fmt.data_checksum(sequence, data)
}

const COMMIT_CHKSUM: usize = 0x10;
const COMMIT_SEC: usize = 0x30;
const COMMIT_NSEC: usize = 0x38;

/// A commit block closing transaction `sequence`, stamped with the wall
/// clock's `(sec, nsec)`.
pub fn encode_commit(fmt: &Format, block: &mut [u8], sequence: u32, sec: u64, nsec: u32) {
    block.fill(0);
    put_header(block, blocktype::COMMIT, sequence);
    put_be64(block, COMMIT_SEC, sec);
    put_be32(block, COMMIT_NSEC, nsec);
    if fmt.csum != Csum::None {
        let sum = crc32c(fmt.seed, block);
        put_be32(block, COMMIT_CHKSUM, sum);
    }
}

pub fn verify_commit(fmt: &Format, block: &[u8]) -> bool {
    if fmt.csum == Csum::None {
        return true;
    }
    let mut crc = crc32c(fmt.seed, &block[..COMMIT_CHKSUM]);
    crc = crc32c(crc, &[0; 4]);
    crc = crc32c(crc, &block[COMMIT_CHKSUM + 4..]);
    be32(block, COMMIT_CHKSUM) == crc
}

/// The commit time's seconds, which is all a replay compares.
pub fn commit_seconds(block: &[u8]) -> u64 {
    be64(block, COMMIT_SEC)
}

const REVOKE_COUNT: usize = HEADER;
const REVOKE_RECORDS: usize = REVOKE_COUNT + 4;

/// A revoke block of `sequence` listing `blocks`, which must fit.
pub fn encode_revoke(fmt: &Format, block: &mut [u8], sequence: u32, blocks: &[u64]) {
    begin_revoke(block, sequence);
    for (i, &b) in blocks.iter().enumerate() {
        put_revoke(fmt, block, i, b);
    }
    finish_revoke(fmt, block, blocks.len());
}

/// Start a revoke block of `sequence`, to be filled one record at a time.
pub fn begin_revoke(block: &mut [u8], sequence: u32) {
    block.fill(0);
    put_header(block, blocktype::REVOKE, sequence);
}

/// Record `i` of a revoke block; `i` must be below
/// [`Format::revokes_per_block`].
pub fn put_revoke(fmt: &Format, block: &mut [u8], i: usize, revoked: u64) {
    let at = REVOKE_RECORDS + i * fmt.revoke_record();
    if fmt.bit64 {
        put_be64(block, at, revoked);
    } else {
        put_be32(block, at, revoked as u32);
    }
}

/// Close a revoke block of `count` records: its length and checksum.
pub fn finish_revoke(fmt: &Format, block: &mut [u8], count: usize) {
    let used = REVOKE_RECORDS + count * fmt.revoke_record();
    put_be32(block, REVOKE_COUNT, used as u32);
    fmt.seal_tail(block);
}

/// Every block a revoke block names; `None` when its count overruns it.
pub fn revoked(fmt: &Format, block: &[u8], f: &mut dyn FnMut(u64)) -> Option<usize> {
    let used = be32(block, REVOKE_COUNT) as usize;
    let rec = fmt.revoke_record();
    if used < REVOKE_RECORDS || used > block.len() - fmt.tail_size() {
        return None;
    }
    let count = (used - REVOKE_RECORDS) / rec;
    for i in 0..count {
        let at = REVOKE_RECORDS + i * rec;
        f(if fmt.bit64 {
            be64(block, at)
        } else {
            u64::from(be32(block, at))
        });
    }
    Some(count)
}

pub fn verify_revoke(fmt: &Format, block: &[u8]) -> bool {
    fmt.verify_tail(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(csum: Csum, bit64: bool) -> Format {
        Format {
            block_size: 1024,
            csum,
            bit64,
            seed: 0x1357,
            uuid: *b"journal-uuid-16b",
        }
    }

    #[test]
    fn tags_round_trip_every_layout() {
        for csum in [Csum::None, Csum::V3] {
            for bit64 in [false, true] {
                let f = fmt(csum, bit64);
                let mut block = [0u8; 1024];
                begin_descriptor(&mut block, 77);
                let count = f.tags_per_descriptor();
                for i in 0..count {
                    let b = if bit64 {
                        0x1_0000_0000 + i as u64
                    } else {
                        i as u64 + 5
                    };
                    put_tag(
                        &f,
                        &mut block,
                        i,
                        count,
                        Tag {
                            block: b,
                            flags: tag_flag::ESCAPE * (i as u32 % 2),
                            checksum: 0xABCD_0000 + i as u32,
                        },
                    );
                }
                seal_descriptor(&f, &mut block);
                assert!(verify_descriptor(&f, &block));
                let mut seen = std::vec::Vec::new();
                assert_eq!(tags(&f, &block, &mut |t| seen.push(t)), count);
                for (i, t) in seen.iter().enumerate() {
                    let b = if bit64 {
                        0x1_0000_0000 + i as u64
                    } else {
                        i as u64 + 5
                    };
                    assert_eq!(t.block, b);
                    assert_eq!(
                        t.flags & tag_flag::ESCAPE,
                        tag_flag::ESCAPE * (i as u32 % 2)
                    );
                }
            }
        }
    }

    /// Other writers end a full descriptor where its room ends, with no tag
    /// marked last.
    #[test]
    fn a_full_descriptor_needs_no_last_tag() {
        let f = fmt(Csum::V3, true);
        let mut block = [0u8; 1024];
        begin_descriptor(&mut block, 1);
        let count = f.tags_per_descriptor();
        for i in 0..count {
            let tag = Tag {
                block: i as u64,
                flags: 0,
                checksum: 0,
            };
            put_tag(&f, &mut block, i, count + 1, tag);
        }
        assert_eq!(tags(&f, &block, &mut |_| {}), count);
    }

    #[test]
    fn revoke_round_trip() {
        let f = fmt(Csum::V3, true);
        let mut block = [0u8; 1024];
        let blocks: std::vec::Vec<u64> = (0..f.revokes_per_block() as u64).collect();
        encode_revoke(&f, &mut block, 5, &blocks);
        assert!(verify_revoke(&f, &block));
        let mut seen = std::vec::Vec::new();
        assert_eq!(
            revoked(&f, &block, &mut |b| seen.push(b)),
            Some(blocks.len())
        );
        assert_eq!(seen, blocks);
    }

    #[test]
    fn commit_checksum() {
        let f = fmt(Csum::V3, false);
        let mut block = [0u8; 1024];
        encode_commit(&f, &mut block, 3, 1_700_000_000, 5);
        assert!(verify_commit(&f, &block));
        block[200] = 1;
        assert!(!verify_commit(&f, &block));
    }
}
