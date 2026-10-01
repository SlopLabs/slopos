//! Block device nodes, the `/dev/disk/by-*` links that name them by identity,
//! and the spellings a mount source or `root=` names a device with.
//!
//! A node's inode is never reused: a partition table re-read replaces a
//! disk's partition nodes, and a descriptor left open on a replaced node must
//! fail rather than reach whichever window now carries its name. Inodes count
//! up in registration order, which is the order the table holds nodes in, so
//! a listing resumes after a node by its inode however the table changed.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::fs::block_ioctl;
use slopos_ostd::sync::{IrqRwLock, IrqRwLockWriteGuard, LOCK_LEVEL_REGISTRY};
use slopos_ostd::{KArc, KVec, klog_info, lock_class};

use super::{DEV_NAME_MAX, Emit};
use crate::blockdev::BlockDevice;
use crate::partition::{PARTUUID_TEXT_MAX, PartUuid};
use crate::vfs::{FileStat, FileType, InodeId, VfsError, VfsResult};
use crate::volume_id::{self, LABEL_MAX, Unreadable, VolumeId};
use slopos_kernel_services::driver_runtime::{current_task_flags, current_task_is_privileged};

pub(super) const DISK_DIR: InodeId = 8;
pub(super) const BY_PARTUUID_DIR: InodeId = 9;
pub(super) const BY_UUID_DIR: InodeId = 10;
pub(super) const BY_LABEL_DIR: InodeId = 11;

const LINK_DIRS: [(&[u8], InodeId); 3] = [
    (b"by-label", BY_LABEL_DIR),
    (b"by-partuuid", BY_PARTUUID_DIR),
    (b"by-uuid", BY_UUID_DIR),
];

/// Node inodes count up from here, clear of every fixed devfs inode.
const NODE_INODE_BASE: InodeId = 1 << 16;
/// A link's inode is its node's with the link kind above this bit.
const LINK_KIND_SHIFT: u32 = 48;
const MAX_BLOCK_NODES: usize = 128;

/// Linux's `BLOCK_EXT_MAJOR`, which it numbers NVMe namespaces under with
/// dynamic minors; the minor here is the node's registration ordinal.
const BLOCK_MAJOR: u32 = 259;

/// What `mount` spells a device's node with.
const DEV_PATH_PREFIX: &[u8] = b"/dev/";

/// `../../` then the node name.
const LINK_TARGET_PREFIX: &[u8] = b"../../";

/// `NAME_MAX`: a longer escaped name is no directory entry.
const LINK_NAME_MAX: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockNodeKind {
    /// A whole disk of the block layer. `partitioned` when a table was found
    /// on it: its partitions carry the volumes, and whatever stale signature
    /// the disk's own first blocks hold names nothing.
    Disk {
        partitioned: bool,
    },
    Partition {
        uuid: PartUuid,
    },
    /// A device published outside the block layer, as a test fixture is: it
    /// has no table and takes no claims.
    Standalone,
}

/// A volume identity and the [`CONTENTS_GENERATION`] it was read at.
/// `Unreadable` stands only until [`UNREADABLE_RETRY_MS`] past `at_ms`: a
/// failing disk costs a lookup no read meanwhile, and a recovered one is
/// named again.
#[derive(Clone, Copy)]
struct Probed {
    generation: u64,
    volume: Result<Option<VolumeId>, Unreadable>,
    at_ms: u64,
}

const UNREADABLE_RETRY_MS: u64 = 5000;

/// Copied out of the registry whole, so the I/O a probe does and the
/// callbacks a listing runs never hold the registry lock. `capacity` is
/// cached because `BlockDevice::capacity` may take the device's own lock.
#[derive(Clone)]
struct BlockNode {
    name: [u8; DEV_NAME_MAX],
    name_len: usize,
    inode: InodeId,
    device: KArc<dyn BlockDevice + Send + Sync>,
    capacity: u64,
    kind: BlockNodeKind,
    probed: Option<Probed>,
}

impl BlockNode {
    fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

static BLOCK_NODES: IrqRwLock<KVec<BlockNode>> = IrqRwLock::new(
    KVec::new(),
    lock_class!("DEVFS_BLOCK_NODES", LOCK_LEVEL_REGISTRY),
);

static NEXT_NODE_INODE: AtomicU64 = AtomicU64::new(NODE_INODE_BASE);

/// Moves whenever a device's contents may have changed beneath its identity
/// links, so an identity read at an older value is read again. Only the
/// release of a write claim that wrote moves it, which an unprivileged task
/// cannot cause: past the first lookup the links cost it no device reads but
/// a failing disk's retry.
static CONTENTS_GENERATION: AtomicU64 = AtomicU64::new(0);

/// A writer has let go of a device, and what it left may be a new volume.
pub fn devfs_block_contents_changed() {
    CONTENTS_GENERATION.fetch_add(1, Ordering::Release);
}

/// Lowercase ASCII letters and digits, a letter first: every kernel block
/// device name has this shape, and a source without it is no device name.
fn valid_node_name(name: &[u8]) -> bool {
    matches!(name.first(), Some(b'a'..=b'z'))
        && name.len() <= DEV_NAME_MAX
        && name
            .iter()
            .all(|&b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// The table, write-locked with room for one more node: the lock disables
/// interrupts, so a push under it must never reach the allocator, and the
/// table grows outside it instead.
fn table_with_room() -> VfsResult<IrqRwLockWriteGuard<'static, KVec<BlockNode>>> {
    loop {
        let table = BLOCK_NODES.write();
        if table.len() < table.capacity() {
            return Ok(table);
        }
        if table.len() >= MAX_BLOCK_NODES {
            return Err(VfsError::NoSpace);
        }
        let want = (table.capacity() * 2).clamp(8, MAX_BLOCK_NODES);
        drop(table);
        let mut fresh = KVec::with_capacity(want).map_err(|_| VfsError::NoSpace)?;
        {
            let mut table = BLOCK_NODES.write();
            if table.capacity() < want {
                for node in table.drain(..) {
                    let _ = fresh.push(node);
                }
                core::mem::swap(&mut *table, &mut fresh);
            }
        }
    }
}

/// Publish `device` as `/dev/<name>`; `name` must be unique. The caller keeps
/// its own `KArc` clone, so one device can back both a mount and this node.
pub fn devfs_register_block_node(
    name: &[u8],
    device: KArc<dyn BlockDevice + Send + Sync>,
    kind: BlockNodeKind,
) -> VfsResult<InodeId> {
    if !valid_node_name(name) {
        return Err(VfsError::InvalidArgument);
    }
    // Outside the registry lock: BLOCK_NODES must never nest over a device's
    // own lock.
    let capacity = device.capacity();

    let mut table = table_with_room()?;
    if table.iter().any(|n| n.name() == name) {
        return Err(VfsError::AlreadyExists);
    }
    let inode = NEXT_NODE_INODE.fetch_add(1, Ordering::Relaxed);
    let mut stored = [0u8; DEV_NAME_MAX];
    stored[..name.len()].copy_from_slice(name);
    table
        .push(BlockNode {
            name: stored,
            name_len: name.len(),
            inode,
            device,
            capacity,
            kind,
            probed: None,
        })
        .map_err(|_| VfsError::NoSpace)?;
    drop(table);

    klog_info!(
        "DEVFS: /dev/{} ({} bytes)",
        core::str::from_utf8(name).unwrap_or("?"),
        capacity
    );
    Ok(inode)
}

/// A [`BlockNodeKind::Standalone`] node.
pub fn devfs_register_block_device(
    name: &[u8],
    device: KArc<dyn BlockDevice + Send + Sync>,
) -> VfsResult<InodeId> {
    devfs_register_block_node(name, device, BlockNodeKind::Standalone)
}

/// Withdraw `/dev/<name>`. A descriptor still open on it fails from here on.
pub fn devfs_unregister_block_node(name: &[u8]) -> VfsResult<()> {
    let removed = {
        let mut table = BLOCK_NODES.write();
        let at = table
            .iter()
            .position(|n| n.name() == name)
            .ok_or(VfsError::NotFound)?;
        table.remove(at)
    };
    // The device reference drops here, with the registry lock released.
    drop(removed);
    Ok(())
}

/// Record whether a disk now carries a partition table.
pub fn devfs_set_block_partitioned(name: &[u8], partitioned: bool) -> VfsResult<()> {
    let mut table = BLOCK_NODES.write();
    let node = table
        .iter_mut()
        .find(|n| n.name() == name)
        .ok_or(VfsError::NotFound)?;
    node.kind = BlockNodeKind::Disk { partitioned };
    Ok(())
}

/// The device of the [`BlockNodeKind::Standalone`] node `name`: nothing
/// claims one, so the caller holds it as it is.
pub fn devfs_standalone_block_device(name: &[u8]) -> Option<KArc<dyn BlockDevice + Send + Sync>> {
    let table = BLOCK_NODES.read();
    let device = table
        .iter()
        .find(|n| n.name() == name && n.kind == BlockNodeKind::Standalone)
        .map(|n| KArc::clone(&n.device));
    drop(table);
    device
}

/// The first node whose inode is `from` or later.
fn node_from(from: InodeId) -> Option<BlockNode> {
    let table = BLOCK_NODES.read();
    let at = table.partition_point(|n| n.inode < from);
    table.get(at).cloned()
}

fn node_by_inode(inode: InodeId) -> Option<BlockNode> {
    BLOCK_NODES
        .read()
        .iter()
        .find(|n| n.inode == inode)
        .cloned()
}

pub(super) fn is_node(inode: InodeId) -> bool {
    BLOCK_NODES.read().iter().any(|n| n.inode == inode)
}

pub(super) fn node_inode_for(name: &[u8]) -> Option<InodeId> {
    BLOCK_NODES
        .read()
        .iter()
        .find(|n| n.name() == name)
        .map(|n| n.inode)
}

/// The nodes from `cookie` on, as `/dev` lists them; each resumes after
/// itself at its inode plus one.
pub(super) fn walk_nodes(cookie: u64, callback: Emit<'_>) -> u64 {
    let mut next = cookie;
    while let Some(node) = node_from(next.max(NODE_INODE_BASE)) {
        next = node.inode + 1;
        if !callback(next, node.name(), node.inode, FileType::BlockDevice) {
            break;
        }
    }
    next
}

pub(super) fn node_stat(inode: InodeId) -> Option<FileStat> {
    let table = BLOCK_NODES.read();
    let node = table.iter().find(|n| n.inode == inode)?;
    Some(FileStat::new_block_device(
        inode,
        node.capacity,
        BLOCK_MAJOR,
        (inode - NODE_INODE_BASE) as u32,
    ))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    PartUuid = 1,
    Uuid = 2,
    Label = 3,
}

impl LinkKind {
    fn of_dir(dir: InodeId) -> Option<Self> {
        match dir {
            BY_PARTUUID_DIR => Some(Self::PartUuid),
            BY_UUID_DIR => Some(Self::Uuid),
            BY_LABEL_DIR => Some(Self::Label),
            _ => None,
        }
    }

    fn of_link(inode: InodeId) -> Option<(Self, InodeId)> {
        let kind = match inode >> LINK_KIND_SHIFT {
            1 => Self::PartUuid,
            2 => Self::Uuid,
            3 => Self::Label,
            _ => return None,
        };
        Some((kind, inode & ((1 << LINK_KIND_SHIFT) - 1)))
    }

    fn link_inode(self, node: InodeId) -> InodeId {
        node | ((self as InodeId) << LINK_KIND_SHIFT)
    }
}

/// The filesystem identity of `node`, if it carries a volume of its own.
#[inline(never)]
fn volume_of(node: &BlockNode) -> Option<VolumeId> {
    if node.kind == (BlockNodeKind::Disk { partitioned: true }) {
        return None;
    }
    let generation = CONTENTS_GENERATION.load(Ordering::Acquire);
    let now = slopos_kernel_services::clock::uptime_ms();
    if let Some(probed) = node.probed
        && probed.generation == generation
    {
        match probed.volume {
            Ok(volume) => return volume,
            Err(Unreadable) if now < probed.at_ms + UNREADABLE_RETRY_MS => return None,
            Err(Unreadable) => {}
        }
    }
    let volume = volume_id::probe(node.device.as_ref());
    if let Some(stored) = BLOCK_NODES
        .write()
        .iter_mut()
        .find(|n| n.inode == node.inode)
    {
        stored.probed = Some(Probed {
            generation,
            volume,
            at_ms: now,
        });
    }
    volume.ok().flatten()
}

/// The name a `/dev/disk/by-*` link carries for `node`: the value escaped as
/// udev escapes it — ASCII letters, digits, `#+-.:=@_` and every valid UTF-8
/// sequence kept, any other byte `\xHH`.
struct LinkName {
    buf: [u8; LINK_NAME_MAX],
    len: usize,
}

impl LinkName {
    fn new() -> Self {
        Self {
            buf: [0; LINK_NAME_MAX],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Become `raw` escaped; `false` for a name no directory entry can carry:
    /// empty, `.`, `..`, or past [`LINK_NAME_MAX`] once escaped. Filled in
    /// place: a name is a quarter page, too much to move about a stack.
    #[inline(never)]
    fn escape(&mut self, raw: &[u8]) -> bool {
        self.len = 0;
        self.escape_chunks(raw).is_some() && !matches!(self.as_bytes(), b"" | b"." | b"..")
    }

    fn escape_chunks(&mut self, raw: &[u8]) -> Option<()> {
        for chunk in raw.utf8_chunks() {
            for c in chunk.valid().chars() {
                if c.is_ascii_alphanumeric() || "#+-.:=@_".contains(c) || !c.is_ascii() {
                    self.push(c.encode_utf8(&mut [0; 4]).as_bytes())?;
                } else {
                    self.push_escaped(c as u8)?;
                }
            }
            for &byte in chunk.invalid() {
                self.push_escaped(byte)?;
            }
        }
        Some(())
    }

    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        self.buf
            .get_mut(self.len..self.len + bytes.len())?
            .copy_from_slice(bytes);
        self.len += bytes.len();
        Some(())
    }

    fn push_escaped(&mut self, byte: u8) -> Option<()> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        self.push(&[
            b'\\',
            b'x',
            HEX[usize::from(byte >> 4)],
            HEX[usize::from(byte & 0xF)],
        ])
    }
}

/// The raw identity value `kind` names `node` by, without escaping.
#[inline(never)]
fn identity_of(kind: LinkKind, node: &BlockNode, out: &mut [u8; LABEL_MAX]) -> Option<usize> {
    match kind {
        LinkKind::PartUuid => {
            let BlockNodeKind::Partition { uuid } = node.kind else {
                return None;
            };
            let mut text = [0u8; PARTUUID_TEXT_MAX];
            let len = uuid.format(&mut text);
            out[..len].copy_from_slice(&text[..len]);
            Some(len)
        }
        LinkKind::Uuid | LinkKind::Label => {
            let volume = volume_of(node)?;
            let value = if kind == LinkKind::Uuid {
                volume.uuid()?
            } else {
                volume.label()?
            };
            out[..value.len()].copy_from_slice(value);
            Some(value.len())
        }
    }
}

/// How a source spelling compares against an identity value.
#[derive(Clone, Copy)]
enum Match {
    /// `PARTUUID=` and `UUID=`: case does not matter.
    IgnoreCase,
    /// `LABEL=`: the bytes as the filesystem stores them.
    Exact,
    /// A `/dev/disk/by-*` link name: the escaped value.
    Escaped,
}

fn matches_identity(value: &[u8], wanted: &[u8], how: Match) -> bool {
    match how {
        Match::IgnoreCase => value.eq_ignore_ascii_case(wanted),
        Match::Exact => value == wanted,
        Match::Escaped => {
            let mut name = LinkName::new();
            name.escape(value) && name.as_bytes() == wanted
        }
    }
}

/// The first node, in registration order, `kind` names `wanted`.
#[inline(never)]
fn find_by_identity(kind: LinkKind, wanted: &[u8], how: Match) -> Option<BlockNode> {
    let mut from = NODE_INODE_BASE;
    while let Some(node) = node_from(from) {
        from = node.inode + 1;
        let mut value = [0u8; LABEL_MAX];
        if let Some(len) = identity_of(kind, &node, &mut value)
            && matches_identity(&value[..len], wanted, how)
        {
            return Some(node);
        }
    }
    None
}

pub(super) fn lookup_link(dir: InodeId, name: &[u8]) -> VfsResult<InodeId> {
    let kind = LinkKind::of_dir(dir).ok_or(VfsError::NotDirectory)?;
    find_by_identity(kind, name, Match::Escaped)
        .map(|node| kind.link_inode(node.inode))
        .ok_or(VfsError::NotFound)
}

/// `.`, `..`, then a link per node in inode order, each resuming after itself
/// at its node's inode plus one.
pub(super) fn walk_links(dir: InodeId, cookie: u64, callback: Emit<'_>) -> VfsResult<u64> {
    let kind = LinkKind::of_dir(dir).ok_or(VfsError::NotDirectory)?;
    let dots = [
        (&b"."[..], dir, FileType::Directory),
        (&b".."[..], DISK_DIR, FileType::Directory),
    ];
    let (mut next, more) = super::walk_fixed(dots, cookie, callback);
    if !more {
        return Ok(next);
    }
    let mut name = LinkName::new();
    while let Some(node) = node_from(next.max(NODE_INODE_BASE)) {
        next = node.inode + 1;
        if !link_name(kind, &node, &mut name) {
            continue;
        }
        if !callback(
            next,
            name.as_bytes(),
            kind.link_inode(node.inode),
            FileType::Symlink,
        ) {
            break;
        }
    }
    Ok(next)
}

/// The link `kind` names `node` by. Two volumes can share a label or a
/// cloned UUID, and the name belongs to the first, as a lookup resolves it.
#[inline(never)]
fn link_name(kind: LinkKind, node: &BlockNode, name: &mut LinkName) -> bool {
    let mut value = [0u8; LABEL_MAX];
    let Some(len) = identity_of(kind, node, &mut value) else {
        return false;
    };
    name.escape(&value[..len])
        && find_by_identity(kind, name.as_bytes(), Match::Escaped)
            .is_some_and(|first| first.inode == node.inode)
}

pub(super) fn walk_disk_dir(cookie: u64, callback: Emit<'_>) -> u64 {
    let entries = [(&b"."[..], DISK_DIR), (&b".."[..], super::ROOT_INODE)]
        .into_iter()
        .chain(LINK_DIRS)
        .map(|(name, inode)| (name, inode, FileType::Directory));
    super::walk_fixed(entries, cookie, callback).0
}

pub(super) fn lookup_disk_dir(name: &[u8]) -> VfsResult<InodeId> {
    match name {
        b"." => Ok(DISK_DIR),
        b".." => Ok(super::ROOT_INODE),
        _ => LINK_DIRS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, inode)| inode)
            .ok_or(VfsError::NotFound),
    }
}

pub(super) fn is_dir(inode: InodeId) -> bool {
    matches!(
        inode,
        DISK_DIR | BY_PARTUUID_DIR | BY_UUID_DIR | BY_LABEL_DIR
    )
}

/// The node a live link points at: one that still carries an identity of
/// the kind the link names it by.
fn link_node(inode: InodeId) -> Option<BlockNode> {
    let (kind, node) = LinkKind::of_link(inode)?;
    let node = node_by_inode(node)?;
    identity_of(kind, &node, &mut [0; LABEL_MAX])?;
    Some(node)
}

pub(super) fn is_link(inode: InodeId) -> bool {
    link_node(inode).is_some()
}

pub(super) fn link_stat(inode: InodeId) -> Option<FileStat> {
    let node = link_node(inode)?;
    let mut stat = FileStat::new_file(inode, (LINK_TARGET_PREFIX.len() + node.name_len) as u64);
    stat.file_type = FileType::Symlink;
    stat.mode = 0o777;
    Some(stat)
}

pub(super) fn readlink(inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
    let node = link_node(inode).ok_or(VfsError::NotFound)?;
    let target_len = LINK_TARGET_PREFIX.len() + node.name_len;
    let len = target_len.min(buf.len());
    let mut target = [0u8; LINK_TARGET_PREFIX.len() + DEV_NAME_MAX];
    target[..LINK_TARGET_PREFIX.len()].copy_from_slice(LINK_TARGET_PREFIX);
    target[LINK_TARGET_PREFIX.len()..target_len].copy_from_slice(node.name());
    buf[..len].copy_from_slice(&target[..len]);
    Ok(len)
}

/// The node name a mount source, `root=` or `mount=` names: `/dev/<name>` or
/// `<name>`; `PARTUUID=`, `UUID=` or `LABEL=`; or a `/dev/disk/by-*` link.
/// The name is copied into `out`; the answer is its length.
pub fn devfs_resolve_block_source(spec: &[u8], out: &mut [u8; DEV_NAME_MAX]) -> VfsResult<usize> {
    devfs_resolve_block_node(spec, out).map(|(len, _)| len)
}

/// [`devfs_resolve_block_source`], with the node's inode: a table re-read can
/// give the name to another window, which the inode tells apart.
pub(crate) fn devfs_resolve_block_node(
    spec: &[u8],
    out: &mut [u8; DEV_NAME_MAX],
) -> VfsResult<(usize, InodeId)> {
    const LINKS: [(&[u8], LinkKind); 3] = [
        (b"/dev/disk/by-partuuid/", LinkKind::PartUuid),
        (b"/dev/disk/by-uuid/", LinkKind::Uuid),
        (b"/dev/disk/by-label/", LinkKind::Label),
    ];
    const TAGS: [(&[u8], LinkKind, Match); 3] = [
        (b"PARTUUID=", LinkKind::PartUuid, Match::IgnoreCase),
        (b"UUID=", LinkKind::Uuid, Match::IgnoreCase),
        (b"LABEL=", LinkKind::Label, Match::Exact),
    ];
    let found = if let Some((rest, kind)) = LINKS
        .iter()
        .find_map(|&(prefix, kind)| Some((spec.strip_prefix(prefix)?, kind)))
    {
        find_by_identity(kind, rest, Match::Escaped)
    } else if let Some((value, kind, how)) = TAGS
        .iter()
        .find_map(|&(tag, kind, how)| Some((spec.strip_prefix(tag)?, kind, how)))
    {
        if value.is_empty() {
            return Err(VfsError::InvalidArgument);
        }
        find_by_identity(kind, value, how)
    } else {
        let name = spec.strip_prefix(DEV_PATH_PREFIX).unwrap_or(spec);
        if !valid_node_name(name) {
            return Err(VfsError::InvalidArgument);
        }
        let inode = node_inode_for(name).ok_or(VfsError::NotFound)?;
        out[..name.len()].copy_from_slice(name);
        return Ok((name.len(), inode));
    };
    let node = found.ok_or(VfsError::NotFound)?;
    out[..node.name_len].copy_from_slice(node.name());
    Ok((node.name_len, node.inode))
}

/// Whether `/dev/<name>` is still the node `inode`.
pub(crate) fn devfs_block_node_is(name: &[u8], inode: InodeId) -> bool {
    node_inode_for(name) == Some(inode)
}

/// Whether the running task may touch a device beneath every filesystem: a
/// kernel thread, `TASK_FLAG_SYSTEM`, or the holder of `TASK_FLAG_MOUNT`, who
/// may already graft any device onto the namespace.
fn raw_block_entitled() -> bool {
    current_task_is_privileged() || current_task_flags() & slopos_abi::task::TASK_FLAG_MOUNT != 0
}

/// Serve bytes from a registered block device. A short read is EOF to the
/// VFS, so this shortens only at the end of the device.
///
/// Requires [`raw_block_entitled`]: a raw read bypasses every filesystem
/// permission check above it, and ext2 does not zero a block it frees, so an
/// unprivileged reader could recover any unlinked file's contents.
pub(super) fn block_read(inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
    block_read_entitled(inode, offset, buf, raw_block_entitled())
}

/// `entitled` is [`raw_block_entitled`] on every production path; it is a
/// parameter only so a test can reach the refusal, which a kernel thread
/// cannot otherwise do.
pub(crate) fn block_read_entitled(
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
    entitled: bool,
) -> VfsResult<usize> {
    let node = node_by_inode(inode).ok_or(VfsError::NotFound)?;
    if !entitled {
        return Err(VfsError::PermissionDenied);
    }
    if offset >= node.capacity || buf.is_empty() {
        return Ok(0);
    }
    let want = (node.capacity - offset).min(buf.len() as u64) as usize;
    node.device
        .read_at(offset, &mut buf[..want])
        .map_err(|_| VfsError::IoError)?;
    Ok(want)
}

/// Write bytes to a registered block device through its exclusive write
/// claim, taken for this one call. A device something holds — a mount above
/// all — refuses with `Busy`: a write behind a filesystem's cache would race
/// its writeback. A write reaching past the end is shortened there, and one
/// starting at the end has no room.
pub(super) fn block_write(inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
    block_write_entitled(inode, offset, buf, raw_block_entitled())
}

pub(crate) fn block_write_entitled(
    inode: InodeId,
    offset: u64,
    buf: &[u8],
    entitled: bool,
) -> VfsResult<usize> {
    let node = node_by_inode(inode).ok_or(VfsError::NotFound)?;
    if !entitled {
        return Err(VfsError::PermissionDenied);
    }
    if buf.is_empty() {
        return Ok(0);
    }
    if offset >= node.capacity {
        return Err(VfsError::NoSpace);
    }
    let want = (node.capacity - offset).min(buf.len() as u64) as usize;
    crate::vfs::init::vfs_claim_block_node(node.name(), node.inode)?
        .write_at(offset, &buf[..want])
        .map_err(|_| VfsError::IoError)?;
    Ok(want)
}

/// Push a block node's writes to the medium. A flush changes no byte, so it
/// needs no claim, and a mounted disk's node answers it too.
pub(super) fn block_flush(inode: InodeId) -> VfsResult<()> {
    let node = node_by_inode(inode).ok_or(VfsError::NotFound)?;
    if !raw_block_entitled() {
        return Err(VfsError::PermissionDenied);
    }
    node.device.flush().map_err(|_| VfsError::IoError)
}

/// What a block ioctl hands back to the caller's argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockIoctlReply {
    Nothing,
    Int(i32),
    UInt(u32),
    U64(u64),
}

/// Answer a block ioctl on the devfs inode `inode`; `None` when `inode` is no
/// block node, so the caller can fall through to its other handlers.
pub fn devfs_block_ioctl(inode: InodeId, request: u32) -> Option<VfsResult<BlockIoctlReply>> {
    let node = node_by_inode(inode)?;
    let block_size = node.device.logical_block_size();
    Some(match request {
        block_ioctl::BLKGETSIZE64 => Ok(BlockIoctlReply::U64(node.capacity)),
        block_ioctl::BLKGETSIZE => Ok(BlockIoctlReply::U64(node.capacity / 512)),
        block_ioctl::BLKSSZGET => Ok(BlockIoctlReply::Int(block_size as i32)),
        block_ioctl::BLKPBSZGET => Ok(BlockIoctlReply::UInt(block_size)),
        block_ioctl::BLKROGET => Ok(BlockIoctlReply::Int(i32::from(
            node.device.write_protected(),
        ))),
        block_ioctl::BLKRRPART => {
            if !raw_block_entitled() {
                return Some(Err(VfsError::PermissionDenied));
            }
            crate::vfs::init::vfs_reread_partitions(node.name()).map(|()| BlockIoctlReply::Nothing)
        }
        _ => Err(VfsError::NotSupported),
    })
}
