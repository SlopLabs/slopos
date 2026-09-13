pub mod canon;
pub mod init;
pub mod mount;
pub mod ops;
pub mod orphan;
pub mod path;
pub mod statfs;
pub mod traits;

pub use canon::{CanonPath, canonicalise, canonicalise_at};
pub use init::{
    RootBacking, vfs_init_builtin_filesystems, vfs_init_builtin_filesystems_with,
    vfs_is_initialized,
};
pub use mount::{MAX_MOUNTS, MOUNT_RDONLY, Mounted, mount, mount_at, unmount, with_mount_table};
pub use ops::{
    ListCursor, VfsHandle, VfsOpenFlags, vfs_link, vfs_link_at, vfs_list, vfs_list_from,
    vfs_list_from_at, vfs_mkdir, vfs_mkdir_at, vfs_open, vfs_open_flags, vfs_open_flags_at,
    vfs_readlink_at, vfs_rename, vfs_rename_at, vfs_rmdir, vfs_rmdir_at, vfs_set_mode,
    vfs_set_mode_at, vfs_set_sealed, vfs_set_times, vfs_stat, vfs_stat_at, vfs_symlink,
    vfs_symlink_at, vfs_sync_all, vfs_unlink, vfs_unlink_at, vfs_utimens,
};
pub use path::{
    NameBuf, RESOLVE_FOLLOW, RESOLVE_MUST_BE_DIR, RESOLVE_NOFOLLOW_FINAL, ResolvedPath,
    resolve_parent_at, resolve_path, resolve_path_at, resolve_path_canon_at,
};
pub use statfs::{vfs_statfs, vfs_statfs_at};
pub use traits::{FileStat, FileSystem, FileType, FsStats, InodeId, VfsError, VfsResult};
