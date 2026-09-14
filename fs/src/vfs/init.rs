use core::sync::atomic::{AtomicU8, Ordering};

use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::{InitFlag, OnceLock};
use slopos_ostd::{KArc, KBox, lock_class};

use crate::blockdev::{BlockDevice, BlockDeviceError};
use crate::devfs::{DevFs, devfs_block_device_by_name};
use crate::ext2_vfs::{Ext2Mount, Ext2MountInfo};
use crate::ramfs::RamFs;
use crate::vfs::mount::{MOUNT_RDONLY, mount, mount_at, unmount, with_mount_table};
use crate::vfs::orphan::{drain_releasable, forget_filesystem, has_open_refs};
use crate::vfs::traits::{FileSystem, same_filesystem};
use crate::vfs::{VfsError, VfsResult};

static VFS_INIT: InitFlag = InitFlag::new();

static RAMFS_ROOT_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_ROOT", LOCK_LEVEL_RESOURCE));
static RAMFS_TMP_STATIC: RamFs = RamFs::new_const(lock_class!("RAMFS_TMP", LOCK_LEVEL_RESOURCE));
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

/// The ext2 instances `mount(2)` hands out for `fstype="ext2"`. One
/// `lock_class!` site each, for the reason [`RAMFS_POOL`] gives; slot 0 keeps
/// the historical `CACHED_EXT2` name so the class the boot phase registers
/// survives.
static EXT2_POOL: [Ext2Mount; EXT2_POOL_LEN] = [
    Ext2Mount::new_const(lock_class!("CACHED_EXT2", LOCK_LEVEL_RESOURCE)),
    Ext2Mount::new_const(lock_class!("EXT2_POOL_1", LOCK_LEVEL_RESOURCE)),
    Ext2Mount::new_const(lock_class!("EXT2_POOL_2", LOCK_LEVEL_RESOURCE)),
    Ext2Mount::new_const(lock_class!("EXT2_POOL_3", LOCK_LEVEL_RESOURCE)),
];

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

/// Whether any attached instance has dirty blocks. The flusher's park
/// predicate, so it reads atomics and takes no lock.
pub(crate) fn ext2_pool_has_dirty() -> bool {
    EXT2_POOL.iter().any(|mount| mount.dirty_pending() > 0)
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

/// Resolve a block-device name to an *exclusive writable* handle.
///
/// `slopos-fs` cannot name a driver — the crate graph runs fs -> core ->
/// drivers -> boot — so boot installs the virtio-blk claim behind this.
pub type BlockClaimFn = fn(&[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>>;

static BLOCK_CLAIM: OnceLock<BlockClaimFn> = OnceLock::new();

pub fn vfs_register_block_claim(claim: BlockClaimFn) {
    BLOCK_CLAIM.call_once(|| claim);
}

/// An exclusive writable handle on the named block device — `vdb`,
/// `/dev/vdb`, `vdb2`, `/dev/vdb2`. Dropping it releases the claim.
///
/// `NotSupported` when no driver registered one, which is what a kernel built
/// without the block layer looks like.
pub fn vfs_claim_block_device(name: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let claim = BLOCK_CLAIM.get().ok_or(VfsError::NotSupported)?;
    (*claim)(name)
}

/// A device view that refuses every write, over whatever devfs publishes for
/// the named device.
///
/// The wrapper is what makes the view read-only, not devfs: a `/dev` node is
/// a handle on the same device the kernel already holds, so the root disk's
/// node *is* the root mount's exclusive write claim. Refusing here keeps a
/// bug past the mount's own read-only gate off the medium.
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

    fn write_protected(&self) -> bool {
        true
    }
}

/// The read-only view of the named device, for an `MS_RDONLY` mount.
fn read_only_block_device(name: &[u8]) -> VfsResult<KBox<dyn BlockDevice + Send + Sync>> {
    let name = core::str::from_utf8(name).map_err(|_| VfsError::NotFound)?;
    let shared = devfs_block_device_by_name(name).ok_or(VfsError::NotFound)?;
    let boxed = KBox::try_new(ReadOnlyBlockDevice(shared)).map_err(|_| VfsError::IoError)?;
    Ok(boxed)
}

/// Attach the block device `source` names to a pooled ext2 instance and mount
/// it at `target`.
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
    // answers `AlreadyClaimed`.
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
/// disk succeeds; without it `open_writer` answers `AlreadyClaimed` forever.
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

/// The one-shot mount of `/`, `/tmp` and `/dev`. Later calls are no-ops
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
