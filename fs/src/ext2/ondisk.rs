use super::Ext2Error;
use super::types::{BlockNum, InodeNum};
use slopos_ext4_core::bytes::{le16, le32, put_le16, put_le32};
use slopos_ext4_core::group::{self, Desc, DescCsum};
use slopos_ext4_core::inode::{self as ext4_inode, off as ioff};
use slopos_ext4_core::superblock::{
    self as ext4_sb, WRITABLE_COMPAT, WRITABLE_INCOMPAT, WRITABLE_RO_COMPAT, compat, incompat,
    off as soff, ro_compat,
};

pub use slopos_ext4_core::inode::InodeTime;

pub const EXT2_MAGIC: u16 = ext4_sb::MAGIC;
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

/// Longest target stored inline in `i_block`: every reader takes a symlink
/// shorter than the 60-byte area as fast and anything longer as slow.
pub const FAST_SYMLINK_MAX: usize = ext4_inode::BLOCK_BYTES - 1;

/// `i_flags`: the inode refuses every mutation. The carrier for the VFS
/// seal, so a sealed binary reads as sealed to `lsattr` and `e2fsck`.
pub const EXT2_IMMUTABLE_FL: u32 = ext4_inode::flags::IMMUTABLE;

/// `i_flags`: the directory's blocks carry an htree index inside records that
/// read as free — block 0's `..` has a `rec_len` covering a `dx_root`, and an
/// interior node is one free record spanning its whole block. That apparent
/// slack is exactly where the linear inserter places an entry, so the first
/// mutation of such a directory clears the flag instead; see
/// `Ext2Fs::deindex_directory`.
pub const EXT2_INDEX_FL: u32 = ext4_inode::flags::INDEX;

/// `i_flags`: `i_block` holds an extent tree rather than block pointers.
pub const EXT4_EXTENTS_FL: u32 = ext4_inode::flags::EXTENTS;

/// The flags `FS_IOC_GETFLAGS` shows: those SlopOS honours and those that
/// describe the inode's layout. Any other, such as sync, stays stored unshown.
pub const USER_VISIBLE_FL: u32 = ext4_inode::flags::IMMUTABLE
    | ext4_inode::flags::APPEND
    | ext4_inode::flags::NODUMP
    | ext4_inode::flags::NOATIME
    | ext4_inode::flags::INDEX
    | ext4_inode::flags::TOPDIR
    | ext4_inode::flags::HUGE_FILE
    | ext4_inode::flags::EXTENTS;

/// The flags `FS_IOC_SETFLAGS` may change: the seal, and two that only tools
/// read. Another would be stored and not honoured.
pub const USER_SETTABLE_FL: u32 =
    ext4_inode::flags::IMMUTABLE | ext4_inode::flags::NODUMP | ext4_inode::flags::NOATIME;

/// `i_flags`: `i_blocks` counts filesystem blocks, not 512-byte sectors.
pub const EXT4_HUGE_FILE_FL: u32 = ext4_inode::flags::HUGE_FILE;

/// Hard links a non-directory may carry, and a directory's ceiling:
/// `dir_nlink`'s uncounted 1 is for indexed directories, and SlopOS keeps
/// every directory it changes linear.
pub const EXT4_LINK_MAX: u16 = 65000;

/// Permission and set-id bits of `i_mode`; the type nibble above them is not
/// a caller's to change.
pub const MODE_PERM_MASK: u16 = 0o7777;

/// `s_state`: the filesystem was unmounted cleanly.
pub const EXT2_VALID_FS: u16 = ext4_sb::STATE_VALID;
/// `s_state`: errors were detected, or an unjournaled image is mounted.
pub const EXT2_ERROR_FS: u16 = ext4_sb::STATE_ERROR;

/// `s_errors`: what a driver should do when it detects an inconsistency.
/// SlopOS behaves as this value whatever the field says, so the constant is
/// what a fixture writes and what `dumpe2fs` reports, never a selector.
pub const EXT2_ERRORS_RO: u16 = 2;

/// `s_last_orphan` byte offset: head of the singly-linked list of inodes whose
/// last name is gone while a descriptor still holds them. Each member carries
/// the next member's number in `i_dtime`, terminated by 0.
pub const S_LAST_ORPHAN_OFF: usize = soff::LAST_ORPHAN;

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
    /// Mounts `e2fsck` allows between checks; 0 or -1 disables the rule.
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
            mtime: le32(data, soff::MTIME),
            wtime: le32(data, soff::WTIME),
            mnt_count: le16(data, soff::MNT_COUNT),
            max_mnt_count: le16(data, soff::MAX_MNT_COUNT),
            errors: le16(data, soff::ERRORS),
            lastcheck: le32(data, soff::LASTCHECK),
            checkinterval: le32(data, soff::CHECKINTERVAL),
        }
    }

    /// Whether the image is due a full check, by either of the two rules
    /// `e2fsck` applies. `false` when a rule is disabled (`0`, or `-1` for the
    /// mount count), when no check was ever recorded, or when the boot
    /// established no wall clock.
    pub fn check_overdue(&self, now: Option<u32>) -> bool {
        let max = self.max_mnt_count as i16;
        if max > 0 && self.mnt_count >= self.max_mnt_count {
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
        let mnt_count = le16(data, soff::MNT_COUNT).saturating_add(1);
        put_le16(data, soff::MNT_COUNT, mnt_count);
        if let Some(now) = now {
            put_le32(data, soff::MTIME, now);
            put_le32(data, soff::WTIME, now);
        }
    }

    /// Record a write into a raw superblock block. A clockless boot leaves the
    /// field as an earlier boot wrote it rather than resetting it to 1970.
    pub fn stamp_write(data: &mut [u8; 1024], now: Option<u32>) {
        if let Some(now) = now {
            put_le32(data, soff::WTIME, now);
        }
    }
}

/// `s_feature_compat` bits this implementation knows by name. Ignoring an
/// unknown COMPAT bit is correct ext2 semantics; ignoring one without knowing
/// you did is not.
const KNOWN_COMPAT: u32 = compat::DIR_PREALLOC
    | compat::IMAGIC_INODES
    | compat::HAS_JOURNAL
    | compat::EXT_ATTR
    | compat::RESIZE_INODE
    | compat::DIR_INDEX
    | compat::LAZY_BG
    | compat::EXCLUDE_BITMAP
    | compat::SPARSE_SUPER2
    | compat::FAST_COMMIT
    | compat::STABLE_INODES
    | compat::ORPHAN_FILE;

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
    /// Bytes per group descriptor: 32, or `s_desc_size` under `64bit`.
    pub desc_size: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    /// Head of the orphan list, or 0 when empty.
    pub last_orphan: u32,
    /// The journal's inode, when the volume has an internal one.
    pub journal_inum: u32,
    pub reserved_gdt_blocks: u16,
    pub log_groups_per_flex: u8,
    /// `i_extra_isize` the volume asks a new inode to get.
    pub want_extra_isize: u16,
    /// What every `metadata_csum` checksum starts from.
    pub csum_seed: u32,
    pub uuid: [u8; 16],
}

/// `s_r_blocks_count`: blocks only a privileged writer may consume.
///
/// Carried on the image so `mke2fs -m`, `tune2fs -m` and `dumpe2fs` all agree
/// with the kernel about the size of the reserve. Not a [`Superblock`] field,
/// for the reason [`SuperblockBookkeeping`] is not one either; it moves only
/// when `tune2fs` moves it, so
/// [`Ext2Geometry`](super::geometry::Ext2Geometry) reads it once at mount.
pub fn reserved_blocks_of(data: &[u8]) -> u32 {
    let lo = u64::from(le32(data, soff::R_BLOCKS_COUNT_LO));
    let hi = if le32(data, soff::FEATURE_INCOMPAT) & incompat::BIT64 != 0 {
        u64::from(le32(data, soff::R_BLOCKS_COUNT_HI))
    } else {
        0
    };
    u32::try_from(lo | (hi << 32)).unwrap_or(u32::MAX)
}

/// Restamp the checksum of a raw superblock a caller changed, when the
/// volume carries one. Every write of the superblock goes through here.
pub fn seal_superblock(data: &mut [u8]) {
    ext4_sb::seal(data);
}

impl Superblock {
    pub fn parse(data: &[u8]) -> Result<Self, Ext2Error> {
        if data.len() < 1024 || le16(data, soff::MAGIC) != EXT2_MAGIC {
            return Err(Ext2Error::InvalidSuperblock);
        }
        let rev_level = le32(data, soff::REV_LEVEL);
        let feature_incompat = le32(data, soff::FEATURE_INCOMPAT);
        if rev_level >= 1 && feature_incompat & !WRITABLE_INCOMPAT != 0 {
            return Err(Ext2Error::UnsupportedFeature);
        }
        if ext4_sb::has_metadata_csum(data)
            && (data[soff::CHECKSUM_TYPE] != ext4_sb::CHECKSUM_CRC32C || !ext4_sb::verify(data))
        {
            return Err(Ext2Error::InvalidSuperblock);
        }
        // Block numbers are 32 bits inside this kernel: 16 TiB at 4 KiB.
        let blocks_count = u32::try_from(ext4_sb::blocks_count(data))
            .map_err(|_| Ext2Error::UnsupportedFeature)?;
        let free_lo = u64::from(le32(data, soff::FREE_BLOCKS_COUNT_LO));
        let free_hi = if feature_incompat & incompat::BIT64 != 0 {
            u64::from(le32(data, soff::FREE_BLOCKS_COUNT_HI))
        } else {
            0
        };
        let free_blocks_count = u32::try_from(free_lo | (free_hi << 32))
            .unwrap_or(u32::MAX)
            .min(blocks_count);
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&data[soff::UUID..soff::UUID + 16]);
        let sb = Self {
            inodes_count: le32(data, soff::INODES_COUNT),
            blocks_count,
            free_blocks_count,
            free_inodes_count: le32(data, soff::FREE_INODES_COUNT),
            first_data_block: BlockNum(le32(data, soff::FIRST_DATA_BLOCK)),
            log_block_size: le32(data, soff::LOG_BLOCK_SIZE),
            blocks_per_group: le32(data, soff::BLOCKS_PER_GROUP),
            inodes_per_group: le32(data, soff::INODES_PER_GROUP),
            magic: EXT2_MAGIC,
            state: le16(data, soff::STATE),
            rev_level,
            first_ino: le32(data, soff::FIRST_INO),
            inode_size: le16(data, soff::INODE_SIZE),
            desc_size: ext4_sb::desc_size(data),
            feature_compat: le32(data, soff::FEATURE_COMPAT),
            feature_incompat,
            feature_ro_compat: le32(data, soff::FEATURE_RO_COMPAT),
            last_orphan: le32(data, S_LAST_ORPHAN_OFF),
            journal_inum: le32(data, soff::JOURNAL_INUM),
            reserved_gdt_blocks: le16(data, soff::RESERVED_GDT_BLOCKS),
            log_groups_per_flex: data[soff::LOG_GROUPS_PER_FLEX],
            want_extra_isize: le16(data, soff::WANT_EXTRA_ISIZE),
            csum_seed: ext4_sb::csum_seed(data),
            uuid,
        };
        // Reject degenerate geometry: a zero divisor reaching block_group(),
        // local_index() or groups_count() faults on the first inode lookup.
        if sb.inodes_per_group == 0 || sb.blocks_per_group == 0 || sb.inodes_count == 0 {
            return Err(Ext2Error::InvalidSuperblock);
        }
        let desc = u32::from(sb.desc_size);
        if desc < group::SIZE_32 as u32 || !desc.is_power_of_two() || desc > 1024 {
            return Err(Ext2Error::InvalidSuperblock);
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
    /// not know. Ignoring them is correct; not saying so is not.
    pub fn unsupported_compat(&self) -> u32 {
        if self.rev_level < 1 {
            return 0;
        }
        self.feature_compat & !KNOWN_COMPAT
    }

    /// Whether the image must be mounted read-only: it carries a feature this
    /// implementation does not write beside.
    pub fn requires_readonly(&self) -> bool {
        self.rev_level >= 1
            && (self.feature_ro_compat & !WRITABLE_RO_COMPAT != 0
                || self.feature_compat & !WRITABLE_COMPAT != 0)
    }

    pub fn has_journal(&self) -> bool {
        self.feature_compat & compat::HAS_JOURNAL != 0
    }

    /// The journal may hold transactions not yet written home.
    pub fn needs_recovery(&self) -> bool {
        self.feature_incompat & incompat::RECOVER != 0
    }

    pub fn metadata_csum(&self) -> bool {
        self.feature_ro_compat & ro_compat::METADATA_CSUM != 0
    }

    /// How group descriptors are checksummed. `metadata_csum` supersedes
    /// `gdt_csum` when an image claims both.
    pub fn desc_csum(&self) -> DescCsum {
        if self.metadata_csum() {
            DescCsum::Crc32c {
                seed: self.csum_seed,
            }
        } else if self.feature_ro_compat & ro_compat::GDT_CSUM != 0 {
            DescCsum::Crc16 { uuid: self.uuid }
        } else {
            DescCsum::None
        }
    }

    /// Whether the clean stamp is on the medium: `s_state` valid with no
    /// error recorded and, on a journaled volume, nothing to replay.
    pub fn is_clean(&self) -> bool {
        self.state & EXT2_VALID_FS != 0 && self.state & EXT2_ERROR_FS == 0 && !self.needs_recovery()
    }

    /// The superblock fields an operation may legitimately move. Everything
    /// else in the 1024-byte block is left as read; the caller seals it.
    ///
    /// `s_state` and `needs_recovery` are deliberately absent: the dirty and
    /// clean stamps are barriered sub-block writes of their own, and a
    /// whole-superblock write-back must not carry a stale copy back over one.
    pub fn encode_mutable_fields(&self, data: &mut [u8]) {
        put_le32(data, soff::FREE_BLOCKS_COUNT_LO, self.free_blocks_count);
        if self.feature_incompat & incompat::BIT64 != 0 {
            put_le32(data, soff::FREE_BLOCKS_COUNT_HI, 0);
        }
        put_le32(data, soff::FREE_INODES_COUNT, self.free_inodes_count);
        put_le32(data, soff::FEATURE_RO_COMPAT, self.feature_ro_compat);
        put_le32(data, S_LAST_ORPHAN_OFF, self.last_orphan);
    }

    pub fn block_size(&self) -> Result<u32, Ext2Error> {
        let size = EXT2_MIN_BLOCK_SIZE
            .checked_shl(self.log_block_size)
            .ok_or(Ext2Error::UnsupportedBlockSize)?;
        if !(EXT2_MIN_BLOCK_SIZE..=EXT2_MAX_BLOCK_SIZE).contains(&size) {
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

/// One group descriptor, its block numbers held to this kernel's 32 bits.
#[derive(Debug, Copy, Clone)]
pub struct GroupDesc {
    pub block_bitmap: BlockNum,
    pub inode_bitmap: BlockNum,
    pub inode_table: BlockNum,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
    pub itable_unused: u32,
    pub flags: u16,
    pub block_bitmap_csum: u32,
    pub inode_bitmap_csum: u32,
}

impl GroupDesc {
    /// `raw` is the descriptor's on-disk bytes, 32 or `s_desc_size` of them.
    pub fn parse(raw: &[u8]) -> Result<Self, Ext2Error> {
        let d = Desc::parse(raw);
        let block = |b: u64| {
            u32::try_from(b)
                .map(BlockNum)
                .map_err(|_| Ext2Error::InvalidBlock)
        };
        Ok(Self {
            block_bitmap: block(d.block_bitmap)?,
            inode_bitmap: block(d.inode_bitmap)?,
            inode_table: block(d.inode_table)?,
            free_blocks_count: d.free_blocks,
            free_inodes_count: d.free_inodes,
            used_dirs_count: d.used_dirs,
            itable_unused: d.itable_unused,
            flags: d.flags,
            block_bitmap_csum: d.block_bitmap_csum,
            inode_bitmap_csum: d.inode_bitmap_csum,
        })
    }

    /// Write every field into `raw`, leaving the bytes this type does not
    /// carry; the checksum is the caller's.
    pub fn encode(&self, raw: &mut [u8]) {
        Desc {
            block_bitmap: u64::from(self.block_bitmap.raw()),
            inode_bitmap: u64::from(self.inode_bitmap.raw()),
            inode_table: u64::from(self.inode_table.raw()),
            free_blocks: self.free_blocks_count,
            free_inodes: self.free_inodes_count,
            used_dirs: self.used_dirs_count,
            itable_unused: self.itable_unused,
            flags: self.flags,
            block_bitmap_csum: self.block_bitmap_csum,
            inode_bitmap_csum: self.inode_bitmap_csum,
        }
        .encode(raw);
    }
}

/// What reading and writing an inode record needs to know about the volume.
#[derive(Debug, Copy, Clone)]
pub struct InodeFormat {
    /// `i_blocks` has a high half (`huge_file`).
    pub huge_file: bool,
    pub block_size: u32,
}

#[derive(Debug, Copy, Clone)]
pub struct Inode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    /// `i_size` with `i_size_high` for a regular file; for every other type
    /// the high word is left as the record holds it.
    pub size: u64,
    pub atime: InodeTime,
    pub ctime: InodeTime,
    pub mtime: InodeTime,
    pub crtime: InodeTime,
    /// A deletion time, or the next orphan's number while on the list.
    pub dtime: u32,
    pub links_count: u16,
    /// 512-byte sectors, whatever unit the record stores them in.
    pub blocks: u64,
    pub flags: u32,
    /// Block pointers, an extent tree root or a fast symlink's target.
    pub block: [BlockNum; 15],
    pub generation: u32,
    /// The extended-attribute block, or 0.
    pub file_acl: u64,
}

impl Inode {
    /// An all-zero record of no type, the starting point of a new inode.
    pub const EMPTY: Self = Self {
        mode: 0,
        uid: 0,
        gid: 0,
        size: 0,
        atime: InodeTime { lo: 0, extra: 0 },
        ctime: InodeTime { lo: 0, extra: 0 },
        mtime: InodeTime { lo: 0, extra: 0 },
        crtime: InodeTime { lo: 0, extra: 0 },
        dtime: 0,
        links_count: 0,
        blocks: 0,
        flags: 0,
        block: [BlockNum::ZERO; 15],
        generation: 0,
        file_acl: 0,
    };

    pub fn parse(data: &[u8], fmt: InodeFormat) -> Self {
        let mut block = [BlockNum::ZERO; 15];
        for (i, slot) in block.iter_mut().enumerate() {
            *slot = BlockNum(le32(data, ioff::BLOCK + 4 * i));
        }
        let mode = le16(data, ioff::MODE);
        let size_low = u64::from(le32(data, ioff::SIZE_LO));
        let size = if mode & MODE_TYPE_MASK == MODE_FILE {
            size_low | (u64::from(le32(data, ioff::SIZE_HIGH)) << 32)
        } else {
            size_low
        };
        let flags = le32(data, ioff::FLAGS);
        let blocks_hi = if fmt.huge_file {
            u64::from(le16(data, ioff::BLOCKS_HIGH))
        } else {
            0
        };
        let raw_blocks = u64::from(le32(data, ioff::BLOCKS_LO)) | (blocks_hi << 32);
        let blocks = if fmt.huge_file && flags & EXT4_HUGE_FILE_FL != 0 {
            raw_blocks.saturating_mul(u64::from(fmt.block_size / 512))
        } else {
            raw_blocks
        };
        Self {
            mode,
            uid: u32::from(le16(data, ioff::UID)) | (u32::from(le16(data, ioff::UID_HIGH)) << 16),
            gid: u32::from(le16(data, ioff::GID)) | (u32::from(le16(data, ioff::GID_HIGH)) << 16),
            size,
            atime: ext4_inode::time(data, ioff::ATIME, ioff::ATIME_EXTRA),
            ctime: ext4_inode::time(data, ioff::CTIME, ioff::CTIME_EXTRA),
            mtime: ext4_inode::time(data, ioff::MTIME, ioff::MTIME_EXTRA),
            crtime: ext4_inode::time(data, ioff::CRTIME, ioff::CRTIME_EXTRA),
            dtime: le32(data, ioff::DTIME),
            links_count: le16(data, ioff::LINKS_COUNT),
            blocks,
            flags,
            block,
            generation: le32(data, ioff::GENERATION),
            file_acl: u64::from(le32(data, ioff::FILE_ACL_LO))
                | (u64::from(le16(data, ioff::FILE_ACL_HIGH)) << 32),
        }
    }

    /// Write the fields this type carries over the record they came from,
    /// leaving the rest — `i_extra_isize`, in-inode attributes, the version
    /// and project words — as they are. The checksum is the caller's.
    pub fn encode_into(&self, data: &mut [u8], fmt: InodeFormat) -> Result<(), Ext2Error> {
        let raw_blocks = if fmt.huge_file && self.flags & EXT4_HUGE_FILE_FL != 0 {
            self.blocks / u64::from(fmt.block_size / 512)
        } else {
            self.blocks
        };
        let limit = if fmt.huge_file {
            1u64 << 48
        } else {
            1u64 << 32
        };
        if raw_blocks >= limit {
            return Err(Ext2Error::InvalidRange);
        }
        put_le16(data, ioff::MODE, self.mode);
        put_le16(data, ioff::UID, self.uid as u16);
        put_le16(data, ioff::UID_HIGH, (self.uid >> 16) as u16);
        put_le32(data, ioff::SIZE_LO, self.size as u32);
        if self.is_regular_file() {
            put_le32(data, ioff::SIZE_HIGH, (self.size >> 32) as u32);
        }
        ext4_inode::put_time(data, ioff::ATIME, ioff::ATIME_EXTRA, self.atime);
        ext4_inode::put_time(data, ioff::CTIME, ioff::CTIME_EXTRA, self.ctime);
        ext4_inode::put_time(data, ioff::MTIME, ioff::MTIME_EXTRA, self.mtime);
        ext4_inode::put_time(data, ioff::CRTIME, ioff::CRTIME_EXTRA, self.crtime);
        put_le32(data, ioff::DTIME, self.dtime);
        put_le16(data, ioff::GID, self.gid as u16);
        put_le16(data, ioff::GID_HIGH, (self.gid >> 16) as u16);
        put_le16(data, ioff::LINKS_COUNT, self.links_count);
        put_le32(data, ioff::BLOCKS_LO, raw_blocks as u32);
        if fmt.huge_file {
            put_le16(data, ioff::BLOCKS_HIGH, (raw_blocks >> 32) as u16);
        }
        put_le32(data, ioff::FLAGS, self.flags);
        for (i, blk) in self.block.iter().enumerate() {
            put_le32(data, ioff::BLOCK + 4 * i, blk.raw());
        }
        put_le32(data, ioff::GENERATION, self.generation);
        put_le32(data, ioff::FILE_ACL_LO, self.file_acl as u32);
        put_le16(data, ioff::FILE_ACL_HIGH, (self.file_acl >> 32) as u16);
        Ok(())
    }

    /// `i_block` as the bytes it is on disk: an extent root or a symlink.
    pub fn block_bytes(&self) -> [u8; ext4_inode::BLOCK_BYTES] {
        let mut out = [0u8; ext4_inode::BLOCK_BYTES];
        for (i, blk) in self.block.iter().enumerate() {
            out[4 * i..4 * i + 4].copy_from_slice(&blk.raw().to_le_bytes());
        }
        out
    }

    pub fn set_block_bytes(&mut self, bytes: &[u8; ext4_inode::BLOCK_BYTES]) {
        for (i, blk) in self.block.iter_mut().enumerate() {
            *blk = BlockNum(le32(bytes, 4 * i));
        }
    }

    /// Whether the record needs `RO_COMPAT_LARGE_FILE` set in the superblock:
    /// an implementation without it reads a 4 GiB-plus file as truncated, so
    /// ext2 makes the flag the price of writing one.
    pub fn needs_large_file_feature(&self) -> bool {
        self.is_regular_file() && self.size > u32::MAX as u64
    }

    /// Whether a directory can take no more subdirectories: it is at
    /// [`EXT4_LINK_MAX`], or uncounted, a count this kernel cannot keep.
    pub fn subdir_links_full(&self) -> bool {
        self.links_count >= EXT4_LINK_MAX || self.is_uncounted_directory()
    }

    /// `dir_nlink`'s 1: more subdirectories than the count holds.
    pub fn is_uncounted_directory(&self) -> bool {
        self.is_directory() && self.links_count == 1
    }

    /// A directory gained a subdirectory's `..`; [`Self::subdir_links_full`]
    /// said it had room.
    pub fn link_subdir(&mut self) {
        self.links_count = self.links_count.saturating_add(1);
    }

    /// A directory lost a subdirectory's `..`. An uncounted one stays
    /// uncounted, and none drops below what its name and `.` hold.
    pub fn unlink_subdir(&mut self) {
        if self.links_count > 2 {
            self.links_count -= 1;
        }
    }

    /// The inode refuses every mutation (`EXT2_IMMUTABLE_FL`).
    pub fn is_immutable(&self) -> bool {
        self.flags & EXT2_IMMUTABLE_FL != 0
    }

    /// The inode refuses every change: it is immutable, or append-only,
    /// which another system set and this kernel holds to the stronger rule.
    pub fn is_sealed(&self) -> bool {
        self.flags & (EXT2_IMMUTABLE_FL | ext4_inode::flags::APPEND) != 0
    }

    pub fn uses_extents(&self) -> bool {
        self.flags & EXT4_EXTENTS_FL != 0
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

    /// A target short enough to live in `i_block`, which is how every ext2
    /// and ext4 reader tells the two kinds apart.
    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.size > 0 && (self.size as usize) <= FAST_SYMLINK_MAX
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
