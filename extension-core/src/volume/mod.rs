//! The filesystem an extension's image holds: ext4 in SlopOS's profile less
//! the journal, with the `read-only` feature, 4 KiB blocks and every inode
//! owned by uid 0. [`write`] lays one out from a tree and [`read`] walks
//! one, refusing anything the kernel could serve differently from what it
//! read: only directories, regular files and symlinks, no name twice in a
//! directory, no inode at two names.

pub mod read;
pub mod write;

use slopos_ext4_core::superblock::{compat, incompat, ro_compat};

pub const BLOCK: usize = slopos_verity_core::BLOCK;
const BLOCKS_PER_GROUP: u64 = 8 * BLOCK as u64;
const INODE_SIZE: usize = 256;
const INODES_PER_BLOCK: u32 = (BLOCK / INODE_SIZE) as u32;
const EXTRA_ISIZE: u16 = 32;
const DESC_SIZE: usize = slopos_ext4_core::group::SIZE_64;
const ROOT: u32 = 2;
const LOST_FOUND: u32 = 11;
/// Where `mke2fs` and `e2fsck` look for `lost+found`, which the volume
/// keeps empty at its root.
pub const LOST_FOUND_NAME: &[u8] = b"lost+found";
/// The longest symlink target `i_block` holds; a longer one takes a block.
const FAST_SYMLINK_MAX: usize = slopos_ext4_core::inode::BLOCK_BYTES - 1;
/// Offset of `s_block_group_nr`, which a backup superblock sets to its group.
const BLOCK_GROUP_NR: usize = 0x5A;

/// What [`write`] sets. [`read`] takes a volume `mke2fs` made too, with the
/// compat features that change nothing a reader sees, and none other.
const COMPAT: u32 = 0;
const INCOMPAT: u32 = incompat::FILETYPE | incompat::EXTENTS | incompat::BIT64 | incompat::FLEX_BG;
const RO_COMPAT: u32 = ro_compat::SPARSE_SUPER
    | ro_compat::LARGE_FILE
    | ro_compat::HUGE_FILE
    | ro_compat::DIR_NLINK
    | ro_compat::EXTRA_ISIZE
    | ro_compat::METADATA_CSUM
    | ro_compat::READONLY;
const READ_COMPAT: u32 =
    compat::HAS_JOURNAL | compat::EXT_ATTR | compat::RESIZE_INODE | compat::DIR_INDEX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Directory,
    File,
    Symlink,
}

impl Kind {
    const fn mode(self) -> u16 {
        match self {
            Kind::Directory => 0o040000,
            Kind::File => 0o100000,
            Kind::Symlink => 0o120000,
        }
    }

    fn of_mode(mode: u16) -> Option<Kind> {
        [Kind::Directory, Kind::File, Kind::Symlink]
            .into_iter()
            .find(|kind| mode & 0o170000 == kind.mode())
    }

    /// The type a directory entry records it as.
    const fn dirent(self) -> u8 {
        match self {
            Kind::File => 1,
            Kind::Directory => 2,
            Kind::Symlink => 7,
        }
    }
}

/// A name a directory may hold: not empty, not `.` or `..`, no `/` and no
/// NUL, at most 255 bytes.
pub fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != b"."
        && name != b".."
        && !name.iter().any(|&b| b == b'/' || b == 0)
}

#[cfg(test)]
mod tests;
