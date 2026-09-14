#![no_std]
// `AllocError` is `KBox::try_init`'s error carrier: the block cache is built
// field by field into its heap slot so no whole-struct rvalue lands on a
// caller's frame.
#![feature(allocator_api)]
#![forbid(unsafe_code)]

/// Longest path the VFS resolves, NUL excluded. Matches
/// [`slopos_abi::fs::USER_PATH_MAX`].
pub const MAX_PATH_LEN: usize = slopos_abi::fs::USER_PATH_MAX;
/// Longest single component. ext2's on-disk ceiling.
pub const MAX_NAME_LEN: usize = slopos_abi::fs::USER_NAME_MAX;
/// Symlink expansions one resolution may perform before `ELOOP`. Linux's
/// post-4.2 whole-path budget, not a per-component recursion limit.
pub const MAX_SYMLINK_FOLLOWS: u32 = 40;

pub mod blockdev;
pub mod cpio;
pub mod devfs;
pub mod ext2;
pub mod ext2_vfs;
pub mod fileio;
pub mod filemap;
pub mod fsreport;
pub mod partition;
pub mod pipe;
pub mod pipe_file_ops;
pub mod ramfs;
pub mod verity;
pub mod vfs;
pub mod vfs_file_ops;

#[cfg(feature = "tests")]
pub mod tests;

#[cfg(test)]
extern crate std;

pub use blockdev::*;
pub use cpio::{CpioError, unpack_cpio_into_root};
pub use devfs::DevFs;
pub use ext2::*;
pub use ext2_vfs::{Ext2Mount, Ext2MountInfo, ext2_vfs_shutdown_sync};
pub use fileio::*;
pub use ramfs::RamFs;
pub use vfs::{
    FileStat, FileSystem, FileType, InodeId, RootBacking, VfsError, VfsResult, mount,
    vfs_claim_block_device, vfs_ext2_mount_named, vfs_ext2_unmount_named,
    vfs_init_builtin_filesystems, vfs_init_builtin_filesystems_with, vfs_is_initialized,
    vfs_register_block_claim,
};
