//! Superblock-derived geometry, validated once at mount.
//!
//! Every quantity the rest of the ext2 code computes from the superblock is
//! derived here, checked against the format's own invariants, and thereafter
//! trusted. `GroupIdx` can only be minted through this type, so a group index
//! that was never bounded against `groups_count` is not a value the rest of
//! the crate can construct.

use super::Ext2Error;
use super::ondisk::{GroupDesc, InodeFormat, Superblock};
use super::types::{BlockNum, GroupIdx, InodeNum};
use slopos_ext4_core::group::DescCsum;
use slopos_ext4_core::superblock::{incompat, ro_compat};
use slopos_ostd::process::AccountId;

/// A group-descriptor location proven to lie inside the descriptor table.
#[derive(Debug, Copy, Clone)]
pub struct GroupDescLoc {
    block: BlockNum,
    within: u32,
}

impl GroupDescLoc {
    #[inline]
    pub fn block(self) -> BlockNum {
        self.block
    }

    #[inline]
    pub fn within(self) -> usize {
        self.within as usize
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Ext2Geometry {
    block_size: u32,
    inode_size: u16,
    desc_size: u32,
    desc_csum: DescCsum,
    csum_seed: Option<u32>,
    feature_incompat: u32,
    feature_ro_compat: u32,
    reserved_gdt_blocks: u32,
    log_groups_per_flex: u8,
    want_extra_isize: u16,
    blocks_count: u32,
    inodes_count: u32,
    first_data_block: BlockNum,
    blocks_per_group: u32,
    inodes_per_group: u32,
    groups_count: u32,
    desc_per_block: u32,
    gdt_first_block: BlockNum,
    gdt_blocks: u32,
    itable_blocks_per_group: u32,
    ptrs_per_block: u32,
    reserved_blocks: u32,
    /// Who a block allocation is charged to. Beside the reserve because both
    /// are properties of the *caller* rather than of the image.
    account: AccountId,
}

impl Ext2Geometry {
    pub fn derive(sb: &Superblock) -> Result<Self, Ext2Error> {
        let block_size = sb.block_size()?;
        let inode_size = sb.effective_inode_size();

        if sb.blocks_per_group == 0 || sb.inodes_per_group == 0 || sb.inodes_count == 0 {
            return Err(Ext2Error::InvalidSuperblock);
        }

        // A group's block and inode bitmaps each occupy exactly one block, so
        // neither count can exceed what one block of bits can address.
        let bits_per_block = block_size
            .checked_mul(8)
            .ok_or(Ext2Error::InvalidSuperblock)?;
        if sb.blocks_per_group > bits_per_block || sb.inodes_per_group > bits_per_block {
            return Err(Ext2Error::InvalidSuperblock);
        }

        let itable_bytes = sb
            .inodes_per_group
            .checked_mul(inode_size as u32)
            .ok_or(Ext2Error::InvalidSuperblock)?;
        let itable_blocks_per_group = itable_bytes.div_ceil(block_size);

        let first_data_block = sb.first_data_block.raw();
        let expected_first = if block_size == 1024 { 1 } else { 0 };
        if first_data_block != expected_first {
            return Err(Ext2Error::InvalidSuperblock);
        }

        if sb.blocks_count <= first_data_block {
            return Err(Ext2Error::InvalidSuperblock);
        }

        let groups_count = (sb.blocks_count - first_data_block).div_ceil(sb.blocks_per_group);
        if groups_count == 0 {
            return Err(Ext2Error::InvalidSuperblock);
        }

        // An inode number is turned into a group index by dividing by
        // inodes_per_group; if the declared inode count exceeds what the
        // groups can hold, that division yields a group past the table.
        let inode_capacity = sb
            .inodes_per_group
            .checked_mul(groups_count)
            .ok_or(Ext2Error::InvalidSuperblock)?;
        if sb.inodes_count > inode_capacity {
            return Err(Ext2Error::InvalidSuperblock);
        }

        let desc_size = u32::from(sb.desc_size);
        let desc_per_block = block_size / desc_size;
        let gdt_first_block = first_data_block
            .checked_add(1)
            .ok_or(Ext2Error::InvalidSuperblock)?;
        let gdt_blocks = groups_count.div_ceil(desc_per_block);
        let gdt_end = gdt_first_block
            .checked_add(gdt_blocks)
            .ok_or(Ext2Error::InvalidSuperblock)?;
        if gdt_end > sb.blocks_count {
            return Err(Ext2Error::InvalidSuperblock);
        }

        Ok(Self {
            block_size,
            inode_size,
            desc_size,
            desc_csum: sb.desc_csum(),
            csum_seed: sb.metadata_csum().then_some(sb.csum_seed),
            feature_incompat: sb.feature_incompat,
            feature_ro_compat: sb.feature_ro_compat,
            reserved_gdt_blocks: u32::from(sb.reserved_gdt_blocks),
            log_groups_per_flex: sb.log_groups_per_flex,
            want_extra_isize: sb.want_extra_isize,
            blocks_count: sb.blocks_count,
            inodes_count: sb.inodes_count,
            first_data_block: BlockNum(first_data_block),
            blocks_per_group: sb.blocks_per_group,
            inodes_per_group: sb.inodes_per_group,
            groups_count,
            desc_per_block,
            gdt_first_block: BlockNum(gdt_first_block),
            gdt_blocks,
            itable_blocks_per_group,
            ptrs_per_block: block_size / 4,
            reserved_blocks: 0,
            account: AccountId::NONE,
        })
    }

    /// Install `s_r_blocks_count`, clamped to the volume. See
    /// [`ondisk::reserved_blocks_of`](super::ondisk::reserved_blocks_of).
    #[inline]
    pub fn with_reserve(mut self, reserved_blocks: u32) -> Self {
        self.reserved_blocks = reserved_blocks.min(self.blocks_count);
        self
    }

    /// Charge allocations through this handle to `account`.
    ///
    /// Per call, because the mount is shared. [`AccountId::NONE`] — a kernel
    /// thread's writeback — names no row, so it is charged to nobody rather
    /// than to a bystander.
    #[inline]
    pub fn with_account(mut self, account: AccountId) -> Self {
        self.account = account;
        self
    }

    #[inline]
    pub fn account(self) -> AccountId {
        self.account
    }

    /// Blocks an unprivileged allocation must leave free. `s_r_blocks_count`
    /// as the image declares it, clamped to the volume; zero on a handle whose
    /// caller is entitled to spend the reserve.
    #[inline]
    pub fn reserved_blocks(self) -> u32 {
        self.reserved_blocks
    }

    /// Inodes an unprivileged allocation must leave free.
    ///
    /// ext2 carries no `s_r_inodes_count`, so this is the block reserve's ratio
    /// applied to the inode table: an image reserving 5% of its blocks reserves
    /// 5% of its inodes. Without it a process that creates empty files denies
    /// `/sbin/init` a new inode with every block still free — the same
    /// exhaustion the block reserve exists to stop, through the other table.
    #[inline]
    pub fn reserved_inodes(self) -> u32 {
        if self.reserved_blocks == 0 || self.blocks_count == 0 {
            return 0;
        }
        let ratio = self.reserved_blocks as u64 * self.inodes_count as u64;
        (ratio / self.blocks_count as u64) as u32
    }

    #[inline]
    pub fn block_size(self) -> u32 {
        self.block_size
    }

    #[inline]
    pub fn inode_size(self) -> u16 {
        self.inode_size
    }

    #[inline]
    pub fn ptrs_per_block(self) -> u32 {
        self.ptrs_per_block
    }

    #[inline]
    pub fn desc_size(self) -> usize {
        self.desc_size as usize
    }

    #[inline]
    pub fn desc_csum(self) -> DescCsum {
        self.desc_csum
    }

    /// The `metadata_csum` seed, or `None` on a volume without checksums.
    #[inline]
    pub fn csum_seed(self) -> Option<u32> {
        self.csum_seed
    }

    /// Whether the uninitialised-group flags mean anything: only a volume
    /// that checksums its descriptors may leave a group unwritten.
    #[inline]
    pub fn uninit_groups(self) -> bool {
        self.desc_csum.enabled()
    }

    #[inline]
    pub fn inode_format(self) -> InodeFormat {
        InodeFormat {
            huge_file: self.feature_ro_compat & ro_compat::HUGE_FILE != 0,
            block_size: self.block_size,
        }
    }

    /// Whether new inodes map their blocks with an extent tree.
    #[inline]
    pub fn extents(self) -> bool {
        self.feature_incompat & incompat::EXTENTS != 0
    }

    /// `i_extra_isize` a new inode gets: the volume's wish, held to what the
    /// record has room for.
    #[inline]
    pub fn new_extra_isize(self) -> u16 {
        let room = self.inode_size.saturating_sub(128);
        self.want_extra_isize.max(32).min(room)
    }

    #[inline]
    pub fn reserved_gdt_blocks(self) -> u32 {
        self.reserved_gdt_blocks
    }

    /// Groups whose bitmaps and inode tables `flex_bg` packs together.
    #[inline]
    pub fn groups_per_flex(self) -> u32 {
        if self.feature_incompat & incompat::FLEX_BG == 0 {
            return 1;
        }
        1u32.checked_shl(u32::from(self.log_groups_per_flex))
            .unwrap_or(1)
    }

    /// Whether `group` carries a superblock copy and descriptor table.
    pub fn group_has_super(self, group: GroupIdx) -> bool {
        let sparse = self.feature_ro_compat & ro_compat::SPARSE_SUPER != 0;
        slopos_ext4_core::superblock::has_backup(group.raw(), sparse)
    }

    #[inline]
    pub fn groups_count(self) -> u32 {
        self.groups_count
    }

    #[inline]
    pub fn blocks_count(self) -> u32 {
        self.blocks_count
    }

    #[inline]
    pub fn inodes_count(self) -> u32 {
        self.inodes_count
    }

    #[inline]
    pub fn inodes_per_group(self) -> u32 {
        self.inodes_per_group
    }

    #[inline]
    pub fn blocks_per_group(self) -> u32 {
        self.blocks_per_group
    }

    #[inline]
    pub fn first_data_block(self) -> BlockNum {
        self.first_data_block
    }

    #[inline]
    pub fn gdt_blocks(self) -> u32 {
        self.gdt_blocks
    }

    #[inline]
    pub fn itable_blocks_per_group(self) -> u32 {
        self.itable_blocks_per_group
    }

    /// The only constructor of `GroupIdx`.
    #[inline]
    pub fn group(self, raw: u32) -> Option<GroupIdx> {
        if raw < self.groups_count {
            Some(GroupIdx::new_unchecked_internal(raw))
        } else {
            None
        }
    }

    pub fn locate_inode(self, ino: InodeNum) -> Option<(GroupIdx, u32)> {
        let raw = ino.raw();
        if raw == 0 || raw > self.inodes_count {
            return None;
        }
        let zero_based = raw - 1;
        let group = self.group(zero_based / self.inodes_per_group)?;
        Some((group, zero_based % self.inodes_per_group))
    }

    pub fn locate_block(self, blk: BlockNum) -> Option<(GroupIdx, u32)> {
        let raw = blk.raw();
        let first = self.first_data_block.raw();
        if raw < first || raw >= self.blocks_count {
            return None;
        }
        let offset = raw - first;
        let group = self.group(offset / self.blocks_per_group)?;
        Some((group, offset % self.blocks_per_group))
    }

    /// Total on a valid `GroupIdx`: `group` is below `groups_count`, and the
    /// descriptor table was proven to span `groups_count` entries at derive.
    #[inline]
    pub fn group_desc_loc(self, group: GroupIdx) -> GroupDescLoc {
        let raw = group.raw();
        GroupDescLoc {
            block: BlockNum(self.gdt_first_block.raw() + raw / self.desc_per_block),
            within: (raw % self.desc_per_block) * self.desc_size,
        }
    }

    #[inline]
    pub fn checked_block(self, raw: u32) -> Option<BlockNum> {
        if raw >= self.first_data_block.raw() && raw < self.blocks_count {
            Some(BlockNum(raw))
        } else {
            None
        }
    }

    /// [`Self::checked_block`], refusing the primary superblock and descriptor
    /// table, which a file, tree, bitmap or inode table pointing there overwrites.
    #[inline]
    pub fn checked_owned_block(self, raw: u32) -> Option<BlockNum> {
        let fixed_end = self.gdt_first_block.raw() + self.gdt_blocks + self.reserved_gdt_blocks;
        self.checked_block(raw).filter(|_| raw >= fixed_end)
    }

    /// A descriptor's three block pointers name blocks inside the volume, and
    /// the inode table it points at fits within it. Without this the pointers
    /// are attacker-chosen block numbers that `write_inode_num` writes through.
    pub fn validate_desc(&self, _group: GroupIdx, d: &GroupDesc) -> Result<(), Ext2Error> {
        self.checked_owned_block(d.block_bitmap.raw())
            .ok_or(Ext2Error::InvalidBlock)?;
        self.checked_owned_block(d.inode_bitmap.raw())
            .ok_or(Ext2Error::InvalidBlock)?;
        let itable = self
            .checked_owned_block(d.inode_table.raw())
            .ok_or(Ext2Error::InvalidBlock)?;
        let itable_end = itable
            .raw()
            .checked_add(self.itable_blocks_per_group)
            .ok_or(Ext2Error::InvalidBlock)?;
        if itable_end > self.blocks_count {
            return Err(Ext2Error::InvalidBlock);
        }
        Ok(())
    }

    #[inline]
    pub fn block_of(self, group: GroupIdx, bit: u32) -> Option<BlockNum> {
        if bit >= self.blocks_per_group {
            return None;
        }
        let base = group.raw().checked_mul(self.blocks_per_group)?;
        let raw = base
            .checked_add(bit)?
            .checked_add(self.first_data_block.raw())?;
        self.checked_block(raw)
    }

    #[inline]
    pub fn inode_of(self, group: GroupIdx, bit: u32) -> Option<InodeNum> {
        if bit >= self.inodes_per_group {
            return None;
        }
        let base = group.raw().checked_mul(self.inodes_per_group)?;
        let raw = base.checked_add(bit)?.checked_add(1)?;
        if raw == 0 || raw > self.inodes_count {
            return None;
        }
        Some(InodeNum(raw))
    }
}
