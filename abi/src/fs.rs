//! Filesystem ABI types shared between kernel and userland.

use core::ffi::CStr;

/// The command search path of a program whose environment names none: the
/// system's directories first, so nothing installed under `/usr/local`
/// shadows them, then `/usr/local`'s, then the commands the owner granted
/// extensions, which shadow neither.
pub const DEFAULT_PATH: &CStr = c"/bin:/sbin:/usr/local/bin:/opt/bin";

/// Longest path a syscall accepts, NUL included. Linux's `PATH_MAX`. Too
/// large for a kernel frame, so the syscall layer stages paths on the heap.
pub const USER_PATH_MAX: usize = 4096;

/// Longest single path component, NUL excluded. ext2's on-disk ceiling:
/// `name_len` is one byte.
pub const USER_NAME_MAX: usize = 255;

/// Entries one `fs_list` call may return. Not a bound on a directory: the
/// call carries a cursor, so a larger directory is read in successive calls
/// rather than truncated at this number.
pub const USER_FS_MAX_ENTRIES: u32 = 64;

pub const FS_TYPE_FILE: u8 = 0;
pub const FS_TYPE_DIRECTORY: u8 = 1;
pub const FS_TYPE_CHARDEV: u8 = 2;
pub const FS_TYPE_SYMLINK: u8 = 3;
pub const FS_TYPE_BLOCKDEV: u8 = 4;
pub const FS_TYPE_UNKNOWN: u8 = 0xFF;

/// `mount(2)` flags, Linux values.
pub const MS_RDONLY: u32 = 1;
/// `umount2(2)`: detach the mount now and tear it down when it goes idle.
pub const MNT_DETACH: u32 = 2;

/// `statfs(2)` `f_flags` bits. Linux values.
pub const ST_RDONLY: u64 = 1;
pub const ST_NOSUID: u64 = 2;

/// `f_type` magics, as reported by Linux for the same filesystems.
pub const EXT2_SUPER_MAGIC: u64 = 0xEF53;
pub const RAMFS_MAGIC: u64 = 0x8584_58F6;
/// The base and the install medium's, a filesystem Linux has no name for.
pub const BASEFS_MAGIC: u64 = 0x534c_4253;
/// An extension's activation at `/opt/<name>`: its image's volume, served
/// only as the kernel verifies it.
pub const EXTENSION_MAGIC: u64 = 0x534c_4558;

/// POSIX file open flags (access mode in low 2 bits, modifiers above).
pub const O_RDONLY: u32 = 0;
pub const O_WRONLY: u32 = 1;
pub const O_RDWR: u32 = 2;
pub const O_ACCMODE: u32 = 3;
pub const O_CREAT: u32 = 0x40;
pub const O_EXCL: u32 = 0x80;
pub const O_TRUNC: u32 = 0x200;
pub const O_APPEND: u32 = 0x400;
/// Every write commits the file's data before returning. The values are the
/// Linux x86-64 ones so a port needs no translation table; note that there
/// `O_SYNC` subsumes `O_DSYNC`, which is why the two are not disjoint bits.
pub const O_DSYNC: u32 = 0x1000;
pub const O_SYNC: u32 = 0x101000;
pub const O_DIRECTORY: u32 = 0o200_000;
pub const O_NOFOLLOW: u32 = 0o400_000;

/// Directory entry returned by the fs_list syscall.
///
/// Every hole is a named field: `copy_to_user` copies `size_of::<Self>()`
/// bytes, so an implicit one would carry kernel stack to userland.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsEntry {
    /// Entry name as UTF-8 bytes (null-terminated)
    pub name: [u8; USER_NAME_MAX + 1],
    pub type_: u8,
    pub _pad: [u8; 7],
    pub size: u64,
}

const _: () = assert!(
    core::mem::size_of::<UserFsEntry>() == 272,
    "UserFsEntry must carry no implicit padding"
);

impl UserFsEntry {
    pub const fn new() -> Self {
        Self {
            name: [0; USER_NAME_MAX + 1],
            type_: 0,
            _pad: [0; 7],
            size: 0,
        }
    }

    pub fn name_str(&self) -> &str {
        let len = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..len]).unwrap_or("<invalid>")
    }

    pub fn is_directory(&self) -> bool {
        self.type_ == FS_TYPE_DIRECTORY
    }

    pub fn is_file(&self) -> bool {
        self.type_ == FS_TYPE_FILE
    }
}

impl Default for UserFsEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// A device number in the `dev_t` layout `major` and `minor` decode on the
/// Linux ABI: twelve bits of major and eight of minor low, the rest above.
pub const fn makedev(major: u32, minor: u32) -> u64 {
    let (major, minor) = (major as u64, minor as u64);
    ((major & 0xFFF) << 8) | ((major & !0xFFF) << 32) | (minor & 0xFF) | ((minor & !0xFF) << 12)
}

/// `stat(2)` output. Field order, widths and padding are the Linux x86-64
/// `struct stat` ones, so a libc port needs no translation table.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFsStat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_nlink: u64,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub _pad0: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atim: crate::syscall::types::Timespec,
    pub st_mtim: crate::syscall::types::Timespec,
    pub st_ctim: crate::syscall::types::Timespec,
    pub _reserved: [i64; 3],
}

const _: () = assert!(
    core::mem::size_of::<UserFsStat>() == 144,
    "UserFsStat must match the Linux x86-64 struct stat"
);

// The three timestamps, pinned separately because a consumer reads them by
// offset rather than by field: cargo's whole fingerprint model is `st_mtim`,
// so a layout slip here is a build system that stops noticing edits rather
// than a compile error.
const _: () = assert!(core::mem::offset_of!(UserFsStat, st_atim) == 72);
const _: () = assert!(core::mem::offset_of!(UserFsStat, st_mtim) == 88);
const _: () = assert!(core::mem::offset_of!(UserFsStat, st_ctim) == 104);

/// Block device ioctls, with Linux's numbers and argument types.
pub mod block_ioctl {
    /// `int *`: 1 when the device refuses writes.
    pub const BLKROGET: u32 = 0x125E;
    /// No argument: re-read the disk's partition table.
    pub const BLKRRPART: u32 = 0x125F;
    /// `unsigned long *`: the size in 512-byte sectors.
    pub const BLKGETSIZE: u32 = 0x1260;
    /// `int *`: the logical block size.
    pub const BLKSSZGET: u32 = 0x1268;
    /// `unsigned int *`: the physical block size, which no driver here learns
    /// apart from the logical one, so the logical block size.
    pub const BLKPBSZGET: u32 = 0x127B;
    /// `u64 *`: the size in bytes.
    pub const BLKGETSIZE64: u32 = 0x8008_1272;
}

/// The inode-flag ioctls `lsattr` and `chattr` use, with Linux's numbers. The
/// argument is an `int *` holding the flags.
pub mod inode_flags_ioctl {
    pub const FS_IOC_GETFLAGS: u32 = 0x8008_6601;
    pub const FS_IOC_SETFLAGS: u32 = 0x4008_6602;
}

/// The inode flags those ioctls carry: ext4's on-disk encoding, which Linux
/// reports for every filesystem.
pub mod inode_flags {
    pub const FS_IMMUTABLE_FL: u32 = 0x0000_0010;
    pub const FS_APPEND_FL: u32 = 0x0000_0020;
    pub const FS_NODUMP_FL: u32 = 0x0000_0040;
    pub const FS_NOATIME_FL: u32 = 0x0000_0080;
    pub const FS_EXTENT_FL: u32 = 0x0008_0000;
}

/// `st_mode` type field and the values it takes. Linux/POSIX numbering.
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFSOCK: u32 = 0o140_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFBLK: u32 = 0o060_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFIFO: u32 = 0o010_000;

/// The mode's special bits. Set-user-ID and set-group-ID on a file are stored
/// and never honoured at exec.
pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;

impl UserFsStat {
    pub fn file_kind(&self) -> u32 {
        self.st_mode & S_IFMT
    }

    pub fn is_directory(&self) -> bool {
        self.file_kind() == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.file_kind() == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.file_kind() == S_IFLNK
    }
}

/// The directories of a boot's base, served from the boot module and mounted
/// read-only over whatever `/` is, so the system is the booted base and the
/// root holds what the machine wrote.
pub const BASE_DIRS: [&str; 6] = [
    "/bin",
    "/sbin",
    "/lib",
    "/usr/bin",
    "/usr/share",
    "/etc/ssl",
];

/// The first person, whom the desktop runs as: their uid, their group's gid,
/// and their home.
pub const SESSION_UID: u32 = 1000;
pub const SESSION_GID: u32 = 1000;
pub const SESSION_HOME: &str = "/home/user";

/// The uids the user database gives, as Debian's policy divides them: 1 to
/// 99 are the base's own system users, then extensions' users, then people,
/// each with a group of their own whose gid is their uid.
pub const EXTENSION_UIDS: core::ops::RangeInclusive<u32> = 100..=999;
pub const PERSON_UIDS: core::ops::RangeInclusive<u32> = SESSION_UID..=59_999;
pub const NOBODY_UID: u32 = 65_534;

/// The group whose members administer the machine.
pub const ADMIN_GROUP: &str = "admin";
pub const ADMIN_GID: u32 = 10;

/// A directory every root carries, who owns it and its mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootDir {
    pub path: &'static str,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
}

const fn system(path: &'static str, mode: u16) -> RootDir {
    RootDir {
        path,
        uid: 0,
        gid: 0,
        mode,
    }
}

const fn session(path: &'static str) -> RootDir {
    RootDir {
        path,
        uid: SESSION_UID,
        gid: SESSION_GID,
        mode: 0o755,
    }
}

/// Every root's directories, parents first: the system's state is uid 0's
/// and what the session writes is its own. `/tmp` and `/run` are memory the
/// kernel mounts, and the base directories its seal.
pub const ROOT_LAYOUT: [RootDir; 15] = [
    system("/", 0o755),
    system("/etc", 0o755),
    system("/var", 0o755),
    system("/var/lib", 0o755),
    system("/var/lib/slopos", 0o755),
    system("/var/log", 0o755),
    system("/var/tmp", 0o1777),
    system("/root", 0o700),
    system("/home", 0o755),
    session(SESSION_HOME),
    system("/media", 0o755),
    system("/usr", 0o755),
    session("/usr/local"),
    system("/opt", 0o755),
    system("/opt/bin", 0o755),
];

/// The directories whose contents are the session's, whatever lies below
/// them; each itself is as [`ROOT_LAYOUT`] has it, or the session's.
pub const SESSION_TREES: [&str; 3] = ["/home", "/src", "/usr/local"];

/// What init records once a root's session trees are the session's.
pub const OWNERS_RECORD: &str = "/var/lib/slopos/owners";

/// The keys the owner trusts with extensions and what each one installed
/// keeps, uid 0's and sealed by the mechanisms that write it.
pub const EXTENSION_STORE: &str = "/var/lib/slopos/extensions";

/// The people the machine knows, which administer it and their passwords'
/// hashes, uid 0's alone and sealed by the mechanisms that write it.
pub const ACCOUNTS_STORE: &str = "/var/lib/slopos/accounts";

/// Directories the kernel keeps where they are from boot: neither one nor a
/// directory on the way to one is renamed or removed, and no mount covers
/// one, one on the way to it or anything beneath it. `/etc` holds the user
/// database and `/var/lib/slopos` the system's records.
pub const KEPT_DIRS: [&str; 4] = ["/etc", "/var/lib/slopos", EXTENSION_STORE, ACCOUNTS_STORE];

pub const AT_FDCWD: i32 = -100;
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
pub const AT_REMOVEDIR: u32 = 0x200;
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;
/// An empty `path` names the descriptor itself.
pub const AT_EMPTY_PATH: u32 = 0x1000;
/// Check with the effective ids rather than the real ones. Shares 0x200 with
/// [`AT_REMOVEDIR`] exactly as Linux does; no call takes both.
pub const AT_EACCESS: u32 = 0x200;

/// `utimensat` per-field sentinels, in `tv_nsec`.
pub const UTIME_NOW: i64 = (1 << 30) - 1;
pub const UTIME_OMIT: i64 = (1 << 30) - 2;

/// `access(2)` mode bits.
pub const F_OK: u32 = 0;
pub const X_OK: u32 = 1;
pub const W_OK: u32 = 2;
pub const R_OK: u32 = 4;

/// A `getdents64(2)` record header. The name follows it as
/// `d_reclen - size_of::<UserDirent64>()` NUL-terminated bytes — tail padding
/// puts that at 24, deliberately not Linux's 19.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserDirent64 {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
}

/// `d_type` values. Linux `DT_*`, which are `S_IFMT >> 12`.
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

/// `readv`/`writev` segment descriptor. Linux `struct iovec`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserIovec {
    pub iov_base: u64,
    pub iov_len: u64,
}

/// Segments one vectored call may carry. Linux's `UIO_MAXIOV`.
pub const UIO_MAXIOV: usize = 1024;

/// `fcntl(2)` record-lock description. Linux x86-64 `struct flock`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserFlock {
    pub l_type: i16,
    pub l_whence: i16,
    pub _pad: [u8; 4],
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
    pub _pad2: [u8; 4],
}

const _: () = assert!(
    core::mem::size_of::<UserFlock>() == 32,
    "UserFlock must match the Linux x86-64 struct flock"
);

/// Caller-provided entry buffer for the fs_list syscall.
///
/// `cursor` makes the call resumable: zero it to start, pass it back
/// unmodified to continue, and stop when [`FS_LIST_CURSOR_END`] comes back.
/// A directory larger than `max_entries` is therefore listed in full rather
/// than silently cut off at the buffer.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct UserFsList {
    pub entries: *mut UserFsEntry,
    pub max_entries: u32,
    /// Actual number of entries returned
    pub count: u32,
    /// Opaque resumption point, updated by the kernel on return. Its meaning
    /// belongs to the filesystem; userland must only carry it back verbatim.
    pub cursor: u64,
}

/// `cursor` value meaning the directory has been listed to its end.
pub const FS_LIST_CURSOR_END: u64 = u64::MAX;

impl Default for UserFsList {
    fn default() -> Self {
        Self {
            entries: core::ptr::null_mut(),
            max_entries: 0,
            count: 0,
            cursor: 0,
        }
    }
}

/// Filesystem statistics returned by `statfs(2)` and `fstatfs(2)`.
///
/// Field order, widths and the trailing `_spare` words are the Linux x86-64
/// `struct statfs` ones — every member a `long`.
#[repr(C)]
#[derive(Default, Copy, Clone)]
pub struct UserStatfs {
    pub f_type: u64,
    pub f_bsize: u64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: u64,
    pub f_namelen: u64,
    pub f_frsize: u64,
    pub f_flags: u64,
    pub _spare: [u64; 4],
}

const _: () = assert!(
    core::mem::size_of::<UserStatfs>() == 120,
    "UserStatfs must carry no implicit padding"
);

/// The longest `mount(2)` filesystem-type name the kernel accepts.
pub const MOUNT_FSTYPE_MAX: usize = 32;
