use core::sync::atomic::{AtomicU8, Ordering};

use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{InitFlag, LockClassKey, OnceLock, register_class_eagerly};
use slopos_ostd::{KArc, KBox, lock_class};

use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::devfs::{
    DEV_NAME_MAX, DevFs, devfs_block_node_is, devfs_resolve_block_node,
    devfs_standalone_block_device,
};
use crate::ext2_vfs::{Ext2Mount, Ext2MountInfo};
use crate::ramfs::RamFs;
use crate::vfs::mount::{MOUNT_RDONLY, mount, mount_at, unmount, with_mount_table};
use crate::vfs::orphan::{drain_releasable, forget_filesystem, has_open_refs};
use crate::vfs::traits::{FileSystem, same_filesystem};
use crate::vfs::{InodeId, VfsError, VfsResult};

static VFS_INIT: InitFlag = InitFlag::new();

static RAMFS_ROOT_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_ROOT", LOCK_LEVEL_RESOURCE));
static RAMFS_TMP_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_TMP", LOCK_LEVEL_RESOURCE));
static RAMFS_SHM_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_SHM", LOCK_LEVEL_RESOURCE));
static DEVFS_STATIC: DevFs = DevFs::new();

/// How many ramfs instances `mount(2)` may have outstanding at once.
pub const RAMFS_POOL_LEN: usize = 4;

/// Instances `mount(2)` can hand out for `fstype="ramfs"`.
///
/// One `lock_class!` site per instance, never one expansion repeated: the
/// macro keys a class on `(name, file:line:column)`, and a path walk crossing
/// a mount holds one mount's lock while taking the next one's — a shared
/// class reads that legal nesting as unordered.
static RAMFS_POOL: [RamFs; RAMFS_POOL_LEN] = [
    RamFs::new_const(lock_class!("RAMFS_POOL_0", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_1", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_2", LOCK_LEVEL_RESOURCE)),
    RamFs::new_const(lock_class!("RAMFS_POOL_3", LOCK_LEVEL_RESOURCE)),
];

const POOL_FREE: u8 = 0;
const POOL_BOUND: u8 = 1;
/// Unmounted lazily while a descriptor still named it. Contents are kept —
/// the descriptor holds a `&'static dyn FileSystem` — until a later claim
/// finds the reference gone; nothing polls, because only a claim needs the
/// slot back.
const POOL_RETIRED: u8 = 2;

static RAMFS_POOL_STATE: [AtomicU8; RAMFS_POOL_LEN] =
    [const { AtomicU8::new(POOL_FREE) }; RAMFS_POOL_LEN];

/// How many ext2 instances may be attached at once.
///
/// At or below both [`crate::vfs::MAX_MOUNTS`] and the block layer's device
/// ceiling: a slot with no device to put in it buys nothing.
pub const EXT2_POOL_LEN: usize = 4;

/// One `lock_class!` site per instance `mount(2)` hands out for `fstype`
/// `ext2`, `ext3` or `ext4`, for the reason [`RAMFS_POOL`] gives; slot 0 keeps
/// the historical `CACHED_EXT2` name so the class the boot phase registers
/// survives.
const EXT2_POOL_CLASSES: [&LockClassKey; EXT2_POOL_LEN] = [
    lock_class!("CACHED_EXT2", LOCK_LEVEL_RESOURCE),
    lock_class!("EXT2_POOL_1", LOCK_LEVEL_RESOURCE),
    lock_class!("EXT2_POOL_2", LOCK_LEVEL_RESOURCE),
    lock_class!("EXT2_POOL_3", LOCK_LEVEL_RESOURCE),
];

static EXT2_POOL: [Ext2Mount; EXT2_POOL_LEN] = [
    Ext2Mount::new_const(EXT2_POOL_CLASSES[0]),
    Ext2Mount::new_const(EXT2_POOL_CLASSES[1]),
    Ext2Mount::new_const(EXT2_POOL_CLASSES[2]),
    Ext2Mount::new_const(EXT2_POOL_CLASSES[3]),
];

/// A mount's sleeping lock registers its class only when it contends, and
/// which slot a mount lands in is timing too, so the whole pool registers on
/// the first claim: the class count is then the same on every run.
static EXT2_POOL_CLASSES_REGISTERED: InitFlag = InitFlag::new();

static EXT2_POOL_STATE: [AtomicU8; EXT2_POOL_LEN] =
    [const { AtomicU8::new(POOL_FREE) }; EXT2_POOL_LEN];

fn ramfs_pool_slot_of(fs: &'static dyn FileSystem) -> Option<usize> {
    RAMFS_POOL
        .iter()
        .position(|candidate| same_filesystem(candidate, fs))
}

fn ext2_pool_slot_of(fs: &'static dyn FileSystem) -> Option<usize> {
    EXT2_POOL
        .iter()
        .position(|candidate| same_filesystem(candidate, fs))
}

/// Reclaim every retired instance whose last reference has gone.
///
/// The `POOL_RETIRED -> POOL_BOUND` step makes the cleanup exclusive: a slot
/// published free before it is reset could be claimed in between, and this
/// call would then drop the *new* mount's records.
fn reclaim_retired_slots() {
    for (idx, state) in RAMFS_POOL_STATE.iter().enumerate() {
        let instance: &'static dyn FileSystem = &RAMFS_POOL[idx];
        if state.load(Ordering::Acquire) != POOL_RETIRED
            || has_open_refs(instance)
            || state
                .compare_exchange(
                    POOL_RETIRED,
                    POOL_BOUND,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            continue;
        }
        forget_filesystem(instance);
        RAMFS_POOL[idx].reset();
        state.store(POOL_FREE, Ordering::Release);
    }
}

/// Claim a pooled ramfs for a `mount(2)`, or `None` when all of them are in
/// use. The instance is reset before it is handed out, so a mount never sees
/// the previous one's files.
pub fn vfs_ramfs_pool_claim() -> Option<&'static RamFs> {
    // Swept first rather than as a fallback: holding a genuinely free retired
    // instance back until the pool is exhausted loses capacity for the boot.
    reclaim_retired_slots();

    for (idx, state) in RAMFS_POOL_STATE.iter().enumerate() {
        if state
            .compare_exchange(POOL_FREE, POOL_BOUND, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            RAMFS_POOL[idx].reset();
            return Some(&RAMFS_POOL[idx]);
        }
    }

    None
}

/// Give back the pooled instance `fs` names, answering whether it was one.
///
/// `retire` when a lazy unmount left a live descriptor behind: resetting the
/// instance under a reader would hand that reader an empty filesystem.
pub fn vfs_ramfs_pool_release(fs: &'static dyn FileSystem, retire: bool) -> bool {
    let Some(idx) = ramfs_pool_slot_of(fs) else {
        return false;
    };
    crate::filemap::forget_filesystem(fs);
    if retire {
        RAMFS_POOL_STATE[idx].store(POOL_RETIRED, Ordering::Release);
    } else {
        RAMFS_POOL[idx].reset();
        RAMFS_POOL_STATE[idx].store(POOL_FREE, Ordering::Release);
    }
    true
}

/// Run `f` over every ext2 instance with a device attached, one at a time: the
/// flusher, the shutdown sweep and the reclaim tier all come through here, and
/// none of them may hold two mounts' locks at once.
pub(crate) fn ext2_pool_for_each_bound(f: &mut dyn FnMut(&'static Ext2Mount)) {
    for (idx, state) in EXT2_POOL_STATE.iter().enumerate() {
        if state.load(Ordering::Acquire) == POOL_FREE {
            continue;
        }
        let instance = &EXT2_POOL[idx];
        if instance.is_initialized() {
            f(instance);
        }
    }
}

/// Whether any attached instance wants the flusher before its timer. The
/// flusher's park predicate, so it reads atomics and takes no lock.
pub(crate) fn ext2_pool_needs_flusher() -> bool {
    EXT2_POOL.iter().any(|mount| mount.needs_flusher_now())
}

/// Detach every retired ext2 instance whose last reference has gone. The
/// `POOL_RETIRED -> POOL_BOUND` step makes the teardown exclusive, exactly as
/// it does for [`reclaim_retired_slots`].
fn reclaim_retired_ext2_slots() {
    for (idx, state) in EXT2_POOL_STATE.iter().enumerate() {
        let instance: &'static dyn FileSystem = &EXT2_POOL[idx];
        if state.load(Ordering::Acquire) != POOL_RETIRED
            || has_open_refs(instance)
            || state
                .compare_exchange(
                    POOL_RETIRED,
                    POOL_BOUND,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            continue;
        }
        forget_filesystem(instance);
        // A teardown that could not take the mount lock leaves the device
        // attached; the slot goes back to retired so the next claim tries
        // again from a task that is not dying.
        let next = if EXT2_POOL[idx].detach() {
            POOL_FREE
        } else {
            POOL_RETIRED
        };
        state.store(next, Ordering::Release);
    }
}

/// Claim an ext2 instance for a mount, or `None` when all of them are in use.
/// It comes back with no device attached; the caller owes it one.
pub fn vfs_ext2_pool_claim() -> Option<&'static Ext2Mount> {
    if EXT2_POOL_CLASSES_REGISTERED.init_once() {
        EXT2_POOL_CLASSES
            .iter()
            .for_each(|class| register_class_eagerly(class));
    }
    reclaim_retired_ext2_slots();

    for (idx, state) in EXT2_POOL_STATE.iter().enumerate() {
        if state
            .compare_exchange(POOL_FREE, POOL_BOUND, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(&EXT2_POOL[idx]);
        }
    }

    None
}

/// Give back the ext2 instance `fs` names, answering whether it was one.
///
/// The detach drops the block device — and with it the exclusive write token
/// — so the same disk can be mounted again. `retire` when a lazy unmount left
/// a live descriptor behind: detaching under a reader would answer it `EIO`.
pub fn vfs_ext2_pool_release(fs: &'static dyn FileSystem, retire: bool) -> bool {
    let Some(idx) = ext2_pool_slot_of(fs) else {
        return false;
    };
    // The instance's address is what the page sets key on, and the next
    // mount of the slot reuses it for another volume.
    crate::filemap::forget_filesystem(fs);
    let torn_down = !retire && EXT2_POOL[idx].detach();
    if torn_down {
        EXT2_POOL_STATE[idx].store(POOL_FREE, Ordering::Release);
    } else {
        EXT2_POOL_STATE[idx].store(POOL_RETIRED, Ordering::Release);
    }
    true
}

/// The ext2 instance already in the mount table, which is what `mount(2)`
/// with an empty `source` means by "the one this boot attached" — whether
/// boot put it at `/` or demoted it to `/mnt`.
pub fn vfs_ext2_mounted_instance() -> Option<&'static Ext2Mount> {
    let mut found: Option<usize> = None;
    with_mount_table(|table| {
        table.for_each_mount(&mut |mounted| {
            if found.is_none() {
                found = ext2_pool_slot_of(mounted);
            }
        });
    });
    found.map(|idx| &EXT2_POOL[idx])
}

/// What the block layer does for devfs and `mount(2)`. `slopos-fs` cannot
/// name a driver — the crate graph runs fs -> core -> drivers -> boot — so
/// boot installs the block layer's entry points behind this.
pub struct BlockLayerOps {
    /// An exclusive writable handle on the named device or partition.
    pub claim: fn(&[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>>,
    /// A read-only handle, shared with other readers and held against a
    /// writer.
    pub claim_read: fn(&[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>>,
    /// Re-read the named disk's partition table.
    pub reread: fn(&[u8]) -> VfsResult<()>,
}

static BLOCK_LAYER: OnceLock<&'static BlockLayerOps> = OnceLock::new();

pub fn vfs_register_block_layer(ops: &'static BlockLayerOps) {
    BLOCK_LAYER.call_once(|| ops);
}

/// An exclusive writable handle on the block device `source` names — `vdb`,
/// `/dev/nvme0n1p2`, `PARTUUID=…`, `LABEL=…`. Dropping it releases the
/// claim.
///
/// `NotSupported` when no block layer registered, which is what a kernel
/// built without one looks like.
pub fn vfs_claim_block_device(source: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let mut name = [0u8; DEV_NAME_MAX];
    vfs_claim_block_source(source, &mut name).map(|(device, _)| device)
}

/// [`vfs_claim_block_device`], with the name of the node it claimed copied
/// into `name`; the answer carries the name's length.
pub fn vfs_claim_block_source(
    source: &[u8],
    name: &mut [u8; DEV_NAME_MAX],
) -> VfsResult<(KBox<dyn BlockDevice + Send + Sync>, usize)> {
    let (len, inode) = devfs_resolve_block_node(source, name)?;
    vfs_claim_block_node(&name[..len], inode).map(|device| (device, len))
}

/// The write claim on the block node `inode`, published as `name`.
pub(crate) fn vfs_claim_block_node(
    name: &[u8],
    inode: InodeId,
) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let ops = BLOCK_LAYER.get().ok_or(VfsError::NotSupported)?;
    held_to_node(name, inode, ops.claim)
}

/// A claim is taken by name, and a table re-read may have given the name to
/// another window since the node was found; nothing re-reads a disk while a
/// claim is held, so once taken it is checked.
fn held_to_node(
    name: &[u8],
    inode: InodeId,
    claim: fn(&[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>>,
) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let device = claim(name)?;
    if !devfs_block_node_is(name, inode) {
        return Err(VfsError::NotFound);
    }
    Ok(device)
}

/// Re-read the partition table of the disk `/dev/<name>` is.
pub fn vfs_reread_partitions(name: &[u8]) -> VfsResult<()> {
    let ops = BLOCK_LAYER.get().ok_or(VfsError::NotSupported)?;
    (ops.reread)(name)
}

/// A standalone node's device with every write refused, which is all that
/// keeps a bug past a read-only mount's own gate off it: nothing claims it.
struct ReadOnlyBlockDevice(KArc<dyn BlockDevice + Send + Sync>);

impl BlockDevice for ReadOnlyBlockDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.0.read_at(offset, buffer)
    }

    fn write_at(&self, _offset: u64, _buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn write_vectored(&self, _offset: u64, _segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        Err(BlockDeviceError::WriteProtected)
    }

    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.0.logical_block_size()
    }

    fn write_protected(&self) -> bool {
        true
    }
}

/// The read-only view of the device `source` names, for an `MS_RDONLY` mount.
fn read_only_block_device(source: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let mut resolved = [0u8; DEV_NAME_MAX];
    let (len, inode) = devfs_resolve_block_node(source, &mut resolved)?;
    let name = &resolved[..len];
    if let Some(device) = devfs_standalone_block_device(name) {
        let view: KBox<dyn BlockDevice + Send + Sync> =
            KBox::try_new(ReadOnlyBlockDevice(device)).map_err(|_| VfsError::NoSpace)?;
        return Ok(view);
    }
    let ops = BLOCK_LAYER.get().ok_or(VfsError::NotSupported)?;
    held_to_node(name, inode, ops.claim_read)
}

/// Attach the block device `source` names to a pooled ext2 instance and mount
/// it at `target`. `source` takes every spelling
/// [`devfs_resolve_block_source`] reads; probe order renames devices, so a
/// `UUID=`, `PARTUUID=` or `LABEL=` is the stable one.
///
/// `read_only` is the caller's *intent*, and an `MS_RDONLY` mount needs both
/// halves of it: the write-refusing device view, and the instance's own
/// refusal, which does not follow from the view.
pub fn vfs_ext2_mount_named(
    source: &[u8],
    target: &[u8],
    read_only: bool,
) -> VfsResult<Ext2MountInfo> {
    // The slot first, the device only once one is held: a slot retired by a
    // lazy unmount still owns its device claim and *claiming* a slot is what
    // sweeps it, so resolving the device first meets that stale claim and
    // answers `Busy`.
    let fs = vfs_ext2_pool_claim().ok_or(VfsError::NoSpace)?;
    let device = match if read_only {
        read_only_block_device(source)
    } else {
        vfs_claim_block_device(source)
    } {
        Ok(device) => device,
        Err(e) => {
            vfs_ext2_pool_release(fs, false);
            return Err(e);
        }
    };
    let info = match fs.attach(device, read_only) {
        Ok(info) => info,
        Err(e) => {
            vfs_ext2_pool_release(fs, false);
            return Err(e);
        }
    };
    let flags = if read_only || info.read_only {
        MOUNT_RDONLY
    } else {
        0
    };
    if let Err(e) = mount(target, fs, flags) {
        vfs_ext2_pool_release(fs, false);
        return Err(e);
    }
    Ok(info)
}

/// Unmount the ext2 filesystem at `target` and detach its instance.
///
/// The detach is what gives the write claim back, so a re-mount of the same
/// disk succeeds; without it every later claim answers `Busy`.
pub fn vfs_ext2_unmount_named(target: &[u8]) -> VfsResult<()> {
    let mounted = mount_at(target).ok_or(VfsError::InvalidArgument)?;
    if ext2_pool_slot_of(mounted.fs).is_none() {
        return Err(VfsError::InvalidArgument);
    }
    if has_open_refs(mounted.fs) {
        return Err(VfsError::Busy);
    }
    let _ = mounted.fs.sync();
    // Before `forget_filesystem`: records left behind keep `releasable_count`
    // nonzero, which keeps the flusher awake for an obligation nobody will run.
    drain_releasable(mounted.fs);
    forget_filesystem(mounted.fs);
    unmount(target)?;
    vfs_ext2_pool_release(mounted.fs, false);
    Ok(())
}

/// The one devfs instance, so `mount(2)` can put it at a second path. Device
/// nodes are global here, so both mounts show the same tree.
pub fn vfs_devfs_instance() -> &'static DevFs {
    &DEVFS_STATIC
}

/// What `/` is backed by. The boot step decides; this module never infers it
/// from what happens to be mounted, because a writable disk being present is
/// not the same as it being the root the caller asked for.
#[derive(Clone, Copy)]
pub enum RootBacking {
    Ramfs,
    /// The ext2 instance boot attached a device to; read-only when it refuses
    /// writes. Falls back to ramfs when nothing is attached to it.
    Ext2(&'static Ext2Mount),
}

/// The one-shot mount of `/`, `/tmp`, `/dev` and `/dev/shm`. Later calls are no-ops
/// whatever `root` they pass: the kernel-test phase reaches this first with
/// ramfs, and the boot step re-mounts `/` itself when it wants the disk.
pub fn vfs_init_builtin_filesystems_with(root: RootBacking) -> VfsResult<()> {
    if !VFS_INIT.init_once() {
        return Ok(());
    }

    match root {
        RootBacking::Ext2(fs) if fs.is_initialized() => {
            let flags = if fs.is_read_only() { MOUNT_RDONLY } else { 0 };
            mount(b"/", fs, flags)?;
        }
        _ => mount(b"/", &RAMFS_ROOT_STATIC, 0)?,
    }

    mount(b"/tmp", &RAMFS_TMP_STATIC, 0)?;
    mount(b"/dev", &DEVFS_STATIC, 0)?;
    // Where `shm_open` puts its objects, as glibc does.
    mount(b"/dev/shm", &RAMFS_SHM_STATIC, 0)?;

    Ok(())
}

/// [`vfs_init_builtin_filesystems_with`] on a RAM root: the form every caller
/// that is not the boot step wants.
pub fn vfs_init_builtin_filesystems() -> VfsResult<()> {
    vfs_init_builtin_filesystems_with(RootBacking::Ramfs)
}

pub fn vfs_is_initialized() -> bool {
    VFS_INIT.is_set()
}

/// Mount the base's directory `dir` at the same path of the root, read-only
/// and pinned, making the mount point if the root lacks it. `NotFound` when
/// the base holds no such directory.
pub fn vfs_mount_base_dir(dir: &[u8]) -> VfsResult<()> {
    let base = &crate::basefs::BASE_FS;
    let inode = base.resolve(dir)?;
    if base.stat(inode)?.file_type != crate::vfs::traits::FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    let mut at = 0;
    while let Some(next) = dir[at + 1..].iter().position(|&b| b == b'/') {
        at += next + 1;
        make_dir(&dir[..at])?;
        pin_dir(&dir[..at])?;
    }
    make_dir(dir)?;
    crate::vfs::mount::mount_subtree(
        dir,
        base,
        inode,
        MOUNT_RDONLY | crate::vfs::mount::MOUNT_PINNED,
    )
}

/// Mount `fs` read-only and pinned at `path`, making the directories on the
/// way and holding them as the base's are.
pub fn vfs_mount_readonly(path: &[u8], fs: &'static dyn FileSystem) -> VfsResult<()> {
    let mut at = 0;
    while let Some(next) = path[at + 1..].iter().position(|&b| b == b'/') {
        at += next + 1;
        make_dir(&path[..at])?;
        pin_dir(&path[..at])?;
    }
    make_dir(path)?;
    directory_itself(path)?;
    crate::vfs::mount::mount(path, fs, MOUNT_RDONLY | crate::vfs::mount::MOUNT_PINNED)
}

/// A symlink on the way would take the walk past the mount point, so the base
/// refuses a root that has one where it goes.
fn pin_dir(path: &[u8]) -> VfsResult<()> {
    let held = directory_itself(path)?;
    crate::vfs::mount::pin_dir(held.fs, held.inode)
}

/// `path` when it is a directory, not a symlink to one.
fn directory_itself(path: &[u8]) -> VfsResult<crate::vfs::path::ResolvedPath> {
    let held =
        crate::vfs::path::resolve_path_at(path, b"/", crate::vfs::path::RESOLVE_NOFOLLOW_FINAL)?;
    if held.fs.stat(held.inode)?.file_type != crate::vfs::traits::FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    Ok(held)
}

fn make_dir(path: &[u8]) -> VfsResult<()> {
    match crate::vfs::ops::vfs_mkdir(path) {
        Ok(()) | Err(VfsError::AlreadyExists) => Ok(()),
        Err(e) => Err(e),
    }
}
