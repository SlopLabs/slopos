use super::Ext2Error;
use super::types::{BlockNum, InodeNum};

pub const EXT2_MAGIC: u16 = 0xEF53;
pub const EXT2_MIN_BLOCK_SIZE: u32 = 1024;
pub const EXT2_MAX_BLOCK_SIZE: u32 = 4096;
pub const EXT2_ROOT_INODE: InodeNum = InodeNum::ROOT;

pub const MODE_FIFO: u16 = 0x1000;
pub const MODE_CHARDEV: u16 = 0x2000;
pub const MODE_DIRECTORY: u16 = 0x4000;
pub const MODE_BLOCKDEV: u16 = 0x6000;
pub const MODE_FILE: u16 = 0x8000;
pub const MODE_SYMLINK: u16 = 0xA000;
pub const MODE_SOCKET: u16 = 0xC000;
pub const MODE_TYPE_MASK: u16 = 0xF000;

pub const DIR_FT_UNKNOWN: u8 = 0;
pub const DIR_FT_REG_FILE: u8 = 1;
pub const DIR_FT_DIR: u8 = 2;
pub const DIR_FT_CHRDEV: u8 = 3;
pub const DIR_FT_BLKDEV: u8 = 4;
pub const DIR_FT_FIFO: u8 = 5;
pub const DIR_FT_SOCK: u8 = 6;
pub const DIR_FT_SYMLINK: u8 = 7;

/// Maximum bytes storable inline in a fast symlink (i_block[0..14] = 60 bytes).
pub const FAST_SYMLINK_MAX: usize = 60;

/// `i_flags`: the inode refuses every mutation. This is the carrier for the
/// VFS seal — `lsattr`/`chattr` display it and `e2fsck` accepts it, so a
/// sealed binary reads as sealed to every other ext2 implementation.
pub const EXT2_IMMUTABLE_FL: u32 = 0x0000_0010;

/// `i_flags`: the directory's blocks carry an htree index inside records that
/// read as free — block 0's `..` has a `rec_len` covering a `dx_root`, and an
/// interior node is one free record spanning its whole block. That apparent
/// slack is exactly where the linear inserter places an entry, so the first
/// mutation of such a directory clears the flag instead; see
/// `Ext2Fs::deindex_directory`.
pub const EXT2_INDEX_FL: u32 = 0x0000_1000;

/// Permission and set-id bits of `i_mode`; the type nibble above them is not
/// a caller's to change.
pub const MODE_PERM_MASK: u16 = 0o7777;

/// `s_state`: the filesystem was unmounted cleanly.
pub const EXT2_VALID_FS: u16 = 1;
/// `s_state`: errors were detected, or the image is currently mounted.
pub const EXT2_ERROR_FS: u16 = 2;

/// `s_errors`: what a driver should do when it detects an inconsistency.
/// SlopOS behaves as this value whatever the field says, so the constant is
/// what a fixture writes and what `dumpe2fs` reports, never a selector.
pub const EXT2_ERRORS_RO: u16 = 2;

/// `s_last_orphan` byte offset: head of the singly-linked list of inodes whose
/// last name is gone while a descriptor still holds them. Each member carries
/// the next member's number in `i_dtime`, terminated by 0.
pub const S_LAST_ORPHAN_OFF: usize = 232;

/// The bookkeeping fields `e2fsck` reads and reports on.
///
/// Deliberately **not** part of [`Superblock`]: these move only at mount and
/// in the sub-block superblock write, whereas a `Superblock` is copied onto
/// every operation's stack frame and into every transaction snapshot.
/// `s_last_orphan` stayed, because an operation genuinely moves it.
#[derive(Debug, Copy, Clone, Default)]
pub struct SuperblockBookkeeping {
    /// Unix time of the last mount.
    pub mtime: u32,
    /// Unix time of the last write.
    pub wtime: u32,
    /// Mounts since the last full check.
    pub mnt_count: u16,
    /// Mounts `e2fsck` allows between checks; 0 disables the rule.
    pub max_mnt_count: u16,
    /// What a driver should do on an inconsistency. Read and reported only;
    /// see [`EXT2_ERRORS_RO`].
    pub errors: u16,
    /// Unix time of the last full check.
    pub lastcheck: u32,
    /// Seconds `e2fsck` allows between checks; 0 disables the rule.
    pub checkinterval: u32,
}

impl SuperblockBookkeeping {
    pub fn parse(data: &[u8; 1024]) -> Self {
        Self {
            mtime: le32(data, 44),
            wtime: le32(data, 48),
            mnt_count: le16(data, 52),
            max_mnt_count: le16(data, 54),
            errors: le16(data, 60),
            lastcheck: le32(data, 64),
            checkinterval: le32(data, 68),
        }
    }

    /// Whether the image is due a full check, by either of the two rules
    /// `e2fsck` applies. `false` when a rule is disabled (`0`), when no check
    /// was ever recorded, or when the boot established no wall clock.
    pub fn check_overdue(&self, now: Option<u32>) -> bool {
        if self.max_mnt_count > 0 && self.mnt_count >= self.max_mnt_count {
            return true;
        }
        let Some(now) = now else {
            return false;
        };
        self.lastcheck != 0
            && self.checkinterval != 0
            && now.saturating_sub(self.lastcheck) >= self.checkinterval
    }

    /// Record a mount into a raw superblock block.
    ///
    /// `s_lastcheck` is deliberately not written: it says when a *full check*
    /// last ran, and this kernel runs none.
    pub fn stamp_mount(data: &mut [u8; 1024], now: Option<u32>) {
        let mnt_count = le16(data, 52).saturating_add(1);
        put_le16(data, 52, mnt_count);
        if let Some(now) = now {
            put_le32(data, 44, now);
            put_le32(data, 48, now);
        }
    }

    /// Record a write into a raw superblock block. A clockless boot leaves the
    /// field as an earlier boot wrote it rather than resetting it to 1970.
    pub fn stamp_write(data: &mut [u8; 1024], now: Option<u32>) {
        if let Some(now) = now {
            put_le32(data, 48, now);
        }
    }
}

/// `s_feature_incompat` bits this implementation understands. An image
/// carrying any other incompat bit has a layout we cannot represent, so
/// mounting it read-write would corrupt it.
pub const SUPPORTED_INCOMPAT: u32 = INCOMPAT_FILETYPE;
/// Directory entries carry a file-type byte; `dir.rs` writes and reads one.
pub const INCOMPAT_FILETYPE: u32 = 0x0002;

/// `s_feature_ro_compat` bits this implementation understands. Anything else
/// is safe to *read* but not to write, so the mount is forced read-only.
pub const SUPPORTED_RO_COMPAT: u32 = RO_COMPAT_SPARSE_SUPER | RO_COMPAT_LARGE_FILE;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x0002;

pub const COMPAT_DIR_PREALLOC: u32 = 0x0001;
pub const COMPAT_IMAGIC_INODES: u32 = 0x0002;
/// An ext3-style journal inode. This kernel logs through its own preallocated
/// file instead, so the image's journal is neither replayed nor written.
pub const COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const COMPAT_EXT_ATTR: u32 = 0x0008;
pub const COMPAT_RESIZE_INODE: u32 = 0x0010;
/// Directories may carry an htree index; see [`EXT2_INDEX_FL`], which is the
/// per-inode statement that one actually does.
pub const COMPAT_DIR_INDEX: u32 = 0x0020;

/// `s_feature_compat` bits whose presence this implementation accounts for.
///
/// Ignoring an unknown COMPAT bit is correct ext2 semantics; ignoring one
/// without knowing you did is not. Naming the known set is what lets the
/// mount report which bits it passed over.
pub const SUPPORTED_COMPAT: u32 = COMPAT_DIR_PREALLOC
    | COMPAT_IMAGIC_INODES
    | COMPAT_EXT_ATTR
    | COMPAT_RESIZE_INODE
    | COMPAT_DIR_INDEX;

#[derive(Debug, Copy, Clone)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: BlockNum,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub magic: u16,
    pub state: u16,
    pub rev_level: u32,
    pub first_ino: u32,
    pub inode_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    /// Head of the orphan list, or 0 when empty.
    pub last_orphan: u32,
}

/// `s_r_blocks_count`: blocks only a privileged writer may consume.
///
/// Carried on the image so `mke2fs -m`, `tune2fs -m` and `dumpe2fs` all agree
/// with the kernel about the size of the reserve. Not a [`Superblock`] field,
/// for the reason [`SuperblockBookkeeping`] is not one either; it moves only
/// when `tune2fs` moves it, so
/// [`Ext2Geometry`](super::geometry::Ext2Geometry) reads it once at mount.
pub fn reserved_blocks_of(data: &[u8]) -> u32 {
    le32(data, 8)
}

/// `s_volume_name`: 16 bytes, NUL-padded, defined from revision 1 on.
const S_VOLUME_NAME_OFF: usize = 120;
const S_VOLUME_NAME_LEN: usize = 16;

/// How much of the superblock [`volume_label_of`] reads.
pub const SUPERBLOCK_LABEL_SPAN: usize = S_VOLUME_NAME_OFF + S_VOLUME_NAME_LEN;

/// The volume label `mke2fs -L`/`e2label` wrote, trailing NULs dropped. `None`
/// when `data` (the superblock's first [`SUPERBLOCK_LABEL_SPAN`] bytes) is not
/// ext2 or predates the field.
pub fn volume_label_of(data: &[u8; SUPERBLOCK_LABEL_SPAN]) -> Option<&[u8]> {
    if le16(data, 56) != EXT2_MAGIC || le32(data, 76) < 1 {
        return None;
    }
    let name = &data[S_VOLUME_NAME_OFF..SUPERBLOCK_LABEL_SPAN];
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    Some(&name[..len])
}

impl Superblock {
    pub fn parse(data: &[u8]) -> Result<Self, Ext2Error> {
        if data.len() < 1024 {
            return Err(Ext2Error::InvalidSuperblock);
        }
        let sb = Self {
            inodes_count: le32(data, 0),
            blocks_count: le32(data, 4),
            free_blocks_count: le32(data, 12),
            free_inodes_count: le32(data, 16),
            first_data_block: BlockNum(le32(data, 20)),
            log_block_size: le32(data, 24),
            blocks_per_group: le32(data, 32),
            inodes_per_group: le32(data, 40),
            magic: le16(data, 56),
            state: le16(data, 58),
            rev_level: le32(data, 76),
            first_ino: le32(data, 84),
            inode_size: le16(data, 88),
            feature_compat: le32(data, 92),
            feature_incompat: le32(data, 96),
            feature_ro_compat: le32(data, 100),
            last_orphan: le32(data, S_LAST_ORPHAN_OFF),
        };
        if sb.magic != EXT2_MAGIC {
            return Err(Ext2Error::InvalidSuperblock);
        }
        // Reject degenerate geometry: a zero divisor reaching block_group(),
        // local_index() or groups_count() faults on the first inode lookup.
        if sb.inodes_per_group == 0 || sb.blocks_per_group == 0 || sb.inodes_count == 0 {
            return Err(Ext2Error::InvalidSuperblock);
        }
        // A layout we cannot represent (extents, 64-bit block numbers,
        // metadata checksums) must not be mounted read-write: we would write
        // records every other implementation then reads as corruption.
        let unsupported_incompat = sb.feature_incompat & !SUPPORTED_INCOMPAT;
        if sb.rev_level >= 1 && unsupported_incompat != 0 {
            return Err(Ext2Error::UnsupportedFeature);
        }
        // The inode table is indexed by multiplying this out; an inode that
        // does not fit its own block makes that arithmetic address outside
        // the block it just read.
        let block_size = sb.block_size()?;
        let inode_size = sb.effective_inode_size() as u32;
        if inode_size < 128 || !inode_size.is_power_of_two() || inode_size > block_size {
            return Err(Ext2Error::InvalidSuperblock);
        }
        Ok(sb)
    }

    /// `s_feature_compat` bits set on the image that this implementation does
    /// not account for. Ignoring them is correct; not saying so is not.
    pub fn unsupported_compat(&self) -> u32 {
        if self.rev_level < 1 {
            return 0;
        }
        self.feature_compat & !SUPPORTED_COMPAT
    }

    /// Whether the image must be mounted read-only: it carries a
    /// read-only-compatible feature this implementation does not write.
    pub fn requires_readonly(&self) -> bool {
        self.rev_level >= 1 && (self.feature_ro_compat & !SUPPORTED_RO_COMPAT) != 0
    }

    /// The superblock fields an operation may legitimately move. Everything
    /// else in the 1024-byte block is left as read.
    ///
    /// `s_state` is deliberately absent: the dirty stamp and the clean stamp
    /// are barriered sub-block writes of their own, and a whole-superblock
    /// write-back must not carry a stale copy back over one.
    pub fn encode_mutable_fields(&self, data: &mut [u8]) {
        put_le32(data, 12, self.free_blocks_count);
        put_le32(data, 16, self.free_inodes_count);
        put_le32(data, 100, self.feature_ro_compat);
        put_le32(data, S_LAST_ORPHAN_OFF, self.last_orphan);
    }

    pub fn block_size(&self) -> Result<u32, Ext2Error> {
        let size = EXT2_MIN_BLOCK_SIZE
            .checked_shl(self.log_block_size)
            .ok_or(Ext2Error::UnsupportedBlockSize)?;
        if size < EXT2_MIN_BLOCK_SIZE || size > EXT2_MAX_BLOCK_SIZE {
            return Err(Ext2Error::UnsupportedBlockSize);
        }
        Ok(size)
    }

    pub fn effective_inode_size(&self) -> u16 {
        if self.inode_size == 0 {
            128
        } else {
            self.inode_size
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub struct GroupDesc {
    pub block_bitmap: BlockNum,
    pub inode_bitmap: BlockNum,
    pub inode_table: BlockNum,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

/// On-disk size of one block group descriptor in ext2 rev 0/1.
pub const GROUP_DESC_SIZE: usize = 32;

impl GroupDesc {
    pub fn parse(data: &[u8; GROUP_DESC_SIZE]) -> Self {
        Self {
            block_bitmap: BlockNum(le32(data, 0)),
            inode_bitmap: BlockNum(le32(data, 4)),
            inode_table: BlockNum(le32(data, 8)),
            free_blocks_count: le16(data, 12),
            free_inodes_count: le16(data, 14),
            used_dirs_count: le16(data, 16),
        }
    }

    pub fn encode(&self, data: &mut [u8]) {
        put_le32(data, 0, self.block_bitmap.raw());
        put_le32(data, 4, self.inode_bitmap.raw());
        put_le32(data, 8, self.inode_table.raw());
        put_le16(data, 12, self.free_blocks_count);
        put_le16(data, 14, self.free_inodes_count);
        put_le16(data, 16, self.used_dirs_count);
    }
}

/// Byte offset of `i_size_high` inside an inode record. Named `i_dir_acl` for
/// every other file type, which is why only a regular file's is read.
const I_SIZE_HIGH_OFF: usize = 108;

#[derive(Debug, Copy, Clone)]
pub struct Inode {
    pub mode: u16,
    pub uid: u16,
    /// Held as 64 bits because a regular file's is 64 bits on disk (`i_size`
    /// plus `i_size_high`); every other type's high half is `i_dir_acl`.
    pub size: u64,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub block: [BlockNum; 15],
}

impl Inode {
    pub fn parse(data: &[u8]) -> Self {
        let mut block = [BlockNum::ZERO; 15];
        let mut offset = 40usize;
        for slot in &mut block {
            *slot = BlockNum(le32(data, offset));
            offset += 4;
        }
        let mode = le16(data, 0);
        let size_low = le32(data, 4) as u64;
        let size = if mode & MODE_TYPE_MASK == MODE_FILE && data.len() > I_SIZE_HIGH_OFF + 4 {
            size_low | ((le32(data, I_SIZE_HIGH_OFF) as u64) << 32)
        } else {
            size_low
        };
        Self {
            mode,
            uid: le16(data, 2),
            size,
            atime: le32(data, 8),
            ctime: le32(data, 12),
            mtime: le32(data, 16),
            dtime: le32(data, 20),
            gid: le16(data, 24),
            links_count: le16(data, 26),
            blocks: le32(data, 28),
            flags: le32(data, 32),
            block,
        }
    }

    pub fn encode(&self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            *byte = 0;
        }
        put_le16(data, 0, self.mode);
        put_le16(data, 2, self.uid);
        put_le32(data, 4, self.size as u32);
        put_le32(data, 8, self.atime);
        put_le32(data, 12, self.ctime);
        put_le32(data, 16, self.mtime);
        put_le32(data, 20, self.dtime);
        put_le16(data, 24, self.gid);
        put_le16(data, 26, self.links_count);
        put_le32(data, 28, self.blocks);
        put_le32(data, 32, self.flags);
        let mut offset = 40usize;
        for blk in &self.block {
            put_le32(data, offset, blk.raw());
            offset += 4;
        }
        if self.is_regular_file() && data.len() > I_SIZE_HIGH_OFF + 4 {
            put_le32(data, I_SIZE_HIGH_OFF, (self.size >> 32) as u32);
        }
    }

    /// Whether the record needs `RO_COMPAT_LARGE_FILE` set in the superblock:
    /// an implementation without it reads a 4 GiB-plus file as truncated, so
    /// ext2 makes the flag the price of writing one.
    pub fn needs_large_file_feature(&self) -> bool {
        self.is_regular_file() && self.size > u32::MAX as u64
    }

    /// The inode refuses every mutation (`EXT2_IMMUTABLE_FL`).
    pub fn is_immutable(&self) -> bool {
        self.flags & EXT2_IMMUTABLE_FL != 0
    }

    pub fn file_type_mode(&self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }

    pub fn is_directory(&self) -> bool {
        self.file_type_mode() == MODE_DIRECTORY
    }

    pub fn is_regular_file(&self) -> bool {
        self.file_type_mode() == MODE_FILE
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type_mode() == MODE_SYMLINK
    }

    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.blocks == 0 && (self.size as usize) <= FAST_SYMLINK_MAX
    }

    /// The directory's blocks hide an htree index (`EXT2_INDEX_FL`).
    pub fn is_indexed(&self) -> bool {
        self.flags & EXT2_INDEX_FL != 0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct DirEntry<'a> {
    pub inode: InodeNum,
    pub file_type: u8,
    pub name: &'a [u8],
    /// This record's own byte offset from the start of the directory's data.
    /// Stable under an unrelated insert or unlink, which is what makes it a
    /// key the name index can hold.
    pub offset: u64,
}

/// Minimum size of a directory entry record (header only, no name).
pub const DIR_ENTRY_HEADER_SIZE: usize = 8;

pub fn dir_entry_size(name_len: usize) -> usize {
    (DIR_ENTRY_HEADER_SIZE + name_len + 3) & !3
}

/// Write a directory entry into `data`. Caller must ensure `data.len() >= rec_len`.
pub fn write_dir_entry(
    data: &mut [u8],
    inode: InodeNum,
    name: &[u8],
    file_type: u8,
    rec_len: usize,
) {
    put_le32(data, 0, inode.raw());
    put_le16(data, 4, rec_len as u16);
    data[6] = name.len() as u8;
    data[7] = file_type;
    for byte in data[DIR_ENTRY_HEADER_SIZE..rec_len].iter_mut() {
        *byte = 0;
    }
    let name_end = DIR_ENTRY_HEADER_SIZE + name.len();
    data[DIR_ENTRY_HEADER_SIZE..name_end].copy_from_slice(name);
}

pub fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return None;
    }
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 1 {
        return None;
    }
    let trimmed = &path[..end];
    let mut idx = trimmed.len();
    while idx > 0 && trimmed[idx - 1] != b'/' {
        idx -= 1;
    }
    if idx == 0 {
        return None;
    }
    let parent = if idx == 1 {
        &trimmed[..1]
    } else {
        &trimmed[..idx - 1]
    };
    let name = &trimmed[idx..];
    if name.is_empty() {
        return None;
    }
    Some((parent, name))
}

fn le16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn le32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn put_le16(data: &mut [u8], offset: usize, val: u16) {
    data[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

fn put_le32(data: &mut [u8], offset: usize, val: u32) {
    data[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}
