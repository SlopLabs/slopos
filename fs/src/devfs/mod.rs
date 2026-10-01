mod block;
pub(crate) mod crash;

pub use block::{
    BlockIoctlReply, BlockNodeKind, devfs_block_contents_changed, devfs_block_ioctl,
    devfs_register_block_device, devfs_register_block_node, devfs_resolve_block_source,
    devfs_set_block_partitioned, devfs_standalone_block_device, devfs_unregister_block_node,
};
#[cfg(feature = "tests")]
pub(crate) use block::{block_read_entitled, block_write_entitled};
pub(crate) use block::{devfs_block_node_is, devfs_resolve_block_node};
pub use crash::{CrashRecord, CrashStoreOps, devfs_register_crash_store};

use crate::fileio::PTY_SLAVE_MAJOR;
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_abi::event::MAX_TTYS;
use slopos_abi::syscall::TtyIndex;
use slopos_kernel_services::syscall_services::tty;

const ROOT_INODE: InodeId = 1;
const NULL_INODE: InodeId = 2;
const ZERO_INODE: InodeId = 3;
const RANDOM_INODE: InodeId = 4;
const CONSOLE_INODE: InodeId = 5;
const KMSG_INODE: InodeId = 6;
const PTS_INODE: InodeId = 7;
/// `/dev/pts/<n>` is this plus `n`, the slave's terminal index.
const PTY_SLAVE_INODE_BASE: InodeId = 1024;

/// devfs's own name ceiling, independent of the VFS's 255: every node name is
/// kernel-registered and short.
pub const DEV_NAME_MAX: usize = 32;

struct DeviceEntry {
    name: [u8; DEV_NAME_MAX],
    name_len: usize,
    inode: InodeId,
    major: u32,
    minor: u32,
}

impl DeviceEntry {
    const fn new(name: &[u8], inode: InodeId, major: u32, minor: u32) -> Self {
        let mut entry = Self {
            name: [0; DEV_NAME_MAX],
            name_len: 0,
            inode,
            major,
            minor,
        };
        let len = if name.len() < DEV_NAME_MAX {
            name.len()
        } else {
            DEV_NAME_MAX
        };
        let mut i = 0;
        while i < len {
            entry.name[i] = name[i];
            i += 1;
        }
        entry.name_len = len;
        entry
    }
}

static DEVICES: [DeviceEntry; 5] = [
    DeviceEntry::new(b"null", NULL_INODE, 1, 3),
    DeviceEntry::new(b"zero", ZERO_INODE, 1, 5),
    DeviceEntry::new(b"random", RANDOM_INODE, 1, 8),
    DeviceEntry::new(b"console", CONSOLE_INODE, 5, 1),
    DeviceEntry::new(b"kmsg", KMSG_INODE, 1, 11),
];

/// The slave `/dev/pts/<name>` names, while it is allocated. Only the
/// canonical spelling: `01` is not `1`.
fn pty_slave_named(name: &[u8]) -> Option<u8> {
    let canonical =
        name.iter().all(u8::is_ascii_digit) && (name.first() != Some(&b'0') || name.len() == 1);
    let index = core::str::from_utf8(name).ok()?.parse::<u8>().ok();
    index.filter(|&n| canonical && pty_slave_live(n))
}

fn pty_slave_live(index: u8) -> bool {
    usize::from(index) < MAX_TTYS && tty::is_pty_slave(TtyIndex(index))
}

fn pty_slave_of(inode: InodeId) -> Option<u8> {
    let index = u8::try_from(inode.checked_sub(PTY_SLAVE_INODE_BASE)?).ok()?;
    pty_slave_live(index).then_some(index)
}

/// What a directory walk hands each entry: the cookie that resumes after it,
/// its name, inode and type. `false` stops the walk.
type Emit<'a> = &'a mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool;

/// Walk `entries` from `cookie`, the one at ordinal `i` resuming at `i + 1`.
/// Answers the cookie reached and whether the callback wants more.
fn walk_fixed<'a>(
    entries: impl IntoIterator<Item = (&'a [u8], InodeId, FileType)>,
    cookie: u64,
    callback: Emit<'_>,
) -> (u64, bool) {
    let mut next = cookie;
    let start = usize::try_from(cookie).unwrap_or(usize::MAX);
    for (ordinal, (name, inode, file_type)) in entries.into_iter().enumerate().skip(start) {
        next = ordinal as u64 + 1;
        if !callback(next, name, inode, file_type) {
            return (next, false);
        }
    }
    (next, true)
}

/// `.` and `..`, then each live slave, which resumes after itself at its
/// index plus three: slaves come and go, and an ordinal would shift.
fn walk_pts(cookie: u64, callback: Emit<'_>) -> u64 {
    let dots = [
        (&b"."[..], PTS_INODE, FileType::Directory),
        (&b".."[..], ROOT_INODE, FileType::Directory),
    ];
    let (mut next, more) = walk_fixed(dots, cookie, callback);
    if !more {
        return next;
    }
    let first = u8::try_from(next.saturating_sub(2)).unwrap_or(u8::MAX);
    for index in (first..MAX_TTYS as u8).filter(|&n| pty_slave_live(n)) {
        let mut digits = [0u8; 3];
        let mut at = digits.len();
        let mut rest = index;
        loop {
            at -= 1;
            digits[at] = b'0' + rest % 10;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        next = u64::from(index) + 3;
        let inode = PTY_SLAVE_INODE_BASE + InodeId::from(index);
        if !callback(next, &digits[at..], inode, FileType::CharDevice) {
            break;
        }
    }
    next
}

/// The fixed entries at cookies 1 to 9, `crash` at 10 while a crash store is
/// registered, then the block nodes by inode.
fn walk_root(cookie: u64, callback: Emit<'_>) -> u64 {
    let fixed = [
        (&b"."[..], ROOT_INODE, FileType::Directory),
        (&b".."[..], ROOT_INODE, FileType::Directory),
    ]
    .into_iter()
    .chain(
        DEVICES
            .iter()
            .map(|dev| (&dev.name[..dev.name_len], dev.inode, FileType::CharDevice)),
    )
    .chain([
        (&b"pts"[..], PTS_INODE, FileType::Directory),
        (&b"disk"[..], block::DISK_DIR, FileType::Directory),
    ])
    .chain(crash::registered().map(|_| (&b"crash"[..], crash::CRASH_DIR, FileType::Directory)));
    match walk_fixed(fixed, cookie, callback) {
        (next, true) => block::walk_nodes(next, callback),
        (next, false) => next,
    }
}

/// Not a ZST deliberately: filesystem identity is the address of the `static`
/// (`vfs::traits::same_filesystem`), and Rust does not promise two distinct
/// zero-sized statics distinct addresses.
pub struct DevFs(#[expect(dead_code, reason = "gives the static an address of its own")] u8);

impl DevFs {
    pub const fn new() -> Self {
        Self(0)
    }
}

impl Default for DevFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for DevFs {
    fn name(&self) -> &'static str {
        "devfs"
    }

    fn root_inode(&self) -> InodeId {
        ROOT_INODE
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        if parent == PTS_INODE {
            return match name {
                b"." => Ok(PTS_INODE),
                b".." => Ok(ROOT_INODE),
                _ => pty_slave_named(name)
                    .map(|n| PTY_SLAVE_INODE_BASE + InodeId::from(n))
                    .ok_or(VfsError::NotFound),
            };
        }
        if parent == block::DISK_DIR {
            return block::lookup_disk_dir(name);
        }
        if parent == crash::CRASH_DIR
            && let Some(store) = crash::registered()
        {
            return crash::lookup(store, name);
        }
        if block::is_dir(parent) {
            return match name {
                b"." => Ok(parent),
                b".." => Ok(block::DISK_DIR),
                _ => block::lookup_link(parent, name),
            };
        }
        if parent != ROOT_INODE {
            return Err(VfsError::NotDirectory);
        }

        if name == b"." || name == b".." {
            return Ok(ROOT_INODE);
        }
        if name == b"pts" {
            return Ok(PTS_INODE);
        }
        if name == b"disk" {
            return Ok(block::DISK_DIR);
        }
        if name == b"crash" && crash::registered().is_some() {
            return Ok(crash::CRASH_DIR);
        }

        for dev in &DEVICES {
            if dev.name_len == name.len() && &dev.name[..dev.name_len] == name {
                return Ok(dev.inode);
            }
        }

        block::node_inode_for(name).ok_or(VfsError::NotFound)
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        if let Some(stat) = crash::registered().and_then(|store| crash::stat(store, inode)) {
            return Ok(stat);
        }
        if inode == ROOT_INODE || inode == PTS_INODE || block::is_dir(inode) {
            return Ok(FileStat::new_directory(inode));
        }
        if let Some(index) = pty_slave_of(inode) {
            return Ok(FileStat::new_char_device(
                inode,
                PTY_SLAVE_MAJOR,
                u32::from(index),
            ));
        }

        for dev in &DEVICES {
            if dev.inode == inode {
                return Ok(FileStat::new_char_device(inode, dev.major, dev.minor));
            }
        }

        block::node_stat(inode)
            .or_else(|| block::link_stat(inode))
            .ok_or(VfsError::NotFound)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        if let Some(store) = crash::registered()
            && crash::is_record(store, inode)
        {
            return crash::read(store, inode, offset, buf, block::raw_block_entitled());
        }
        match inode {
            NULL_INODE => Ok(0),

            // Served by offset so a plain `cat /dev/kmsg` streams to EOF.
            KMSG_INODE => Ok(slopos_ostd::klog::klog_read(offset as usize, buf)),

            ZERO_INODE => {
                buf.fill(0);
                Ok(buf.len())
            }

            RANDOM_INODE => {
                let mut pos = 0;
                while pos < buf.len() {
                    let val = slopos_kernel_services::platform::rng_next();
                    let bytes = val.to_le_bytes();
                    let chunk = (buf.len() - pos).min(8);
                    buf[pos..pos + chunk].copy_from_slice(&bytes[..chunk]);
                    pos += chunk;
                }
                Ok(pos)
            }

            CONSOLE_INODE => Ok(0),

            ROOT_INODE | PTS_INODE | crash::CRASH_DIR => Err(VfsError::IsDirectory),
            _ if block::is_dir(inode) => Err(VfsError::IsDirectory),

            _ => block::block_read(inode, offset, buf),
        }
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if crash::registered().is_some_and(|store| crash::is_record(store, inode)) {
            return Err(VfsError::ReadOnly);
        }
        match inode {
            // kmsg is read-only; writes are discarded so a stray redirect does
            // not error.
            NULL_INODE | ZERO_INODE | KMSG_INODE => Ok(buf.len()),

            // Entropy injection is not meaningful with a seeded CSPRNG.
            RANDOM_INODE => Ok(buf.len()),

            CONSOLE_INODE => Ok(buf.len()),

            ROOT_INODE | PTS_INODE | crash::CRASH_DIR => Err(VfsError::IsDirectory),
            _ if block::is_dir(inode) => Err(VfsError::IsDirectory),

            _ if block::is_node(inode) => block::block_write(inode, offset, buf),

            _ => Err(VfsError::NotFound),
        }
    }

    fn create(&self, _parent: InodeId, _name: &[u8], _file_type: FileType) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        if parent == crash::CRASH_DIR
            && let Some(store) = crash::registered()
        {
            return crash::unlink(store, name, block::raw_block_entitled());
        }
        Err(VfsError::ReadOnly)
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let mut seen = 0;
        let mut count = 0;
        self.readdir_cookie(inode, 0, &mut |_, name, ino, file_type| {
            seen += 1;
            if seen <= offset {
                return true;
            }
            let more = callback(name, ino, file_type);
            count += usize::from(more);
            more
        })?;
        Ok(count)
    }

    /// Cookies name positions that survive nodes and slaves coming and going,
    /// so a listing paged across a table re-read neither skips nor repeats.
    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        match inode {
            ROOT_INODE => Ok(walk_root(cookie, callback)),
            PTS_INODE => Ok(walk_pts(cookie, callback)),
            block::DISK_DIR => Ok(block::walk_disk_dir(cookie, callback)),
            crash::CRASH_DIR => crash::registered()
                .map(|store| crash::walk(store, cookie, callback))
                .ok_or(VfsError::NotDirectory),
            _ if block::is_dir(inode) => block::walk_links(inode, cookie, callback),
            _ => Err(VfsError::NotDirectory),
        }
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    fn sync_inode(&self, inode: InodeId, _data_only: bool) -> VfsResult<()> {
        if block::is_node(inode) {
            return block::block_flush(inode);
        }
        Ok(())
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        if !block::is_link(inode) {
            return Err(VfsError::InvalidArgument);
        }
        block::readlink(inode, buf)
    }
}
