use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::{LOCK_LEVEL_RESOURCE, LockClassKey};

use crate::blockdev::BlockDevice;
use crate::ext2::cache::{BlockCache, cache_entries_for};
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Inode, Ext2Superblock, ReadOnlyReason, SyncPass};
use crate::verity::{AttestTrust, FsExtent, VerityError, VerityStatus};
use crate::vfs::{FileStat, FileSystem, FileType, FsStats, InodeId, VfsError, VfsResult, orphan};
use slopos_kernel_services::driver_runtime::{current_task_account, current_task_is_privileged};
use slopos_ostd::KBox;
use slopos_ostd::klog_info;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, Mutex, MutexGuard, WaitResult};

/// `prof=on`: how long operations wait for a mount's lock and how long its
/// holders keep it, summed over every mount. Every operation on a mount
/// serialises on that one lock, so the wait is what a busy mount costs.
pub mod lock_profile {
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    static ENABLED: AtomicBool = AtomicBool::new(false);
    pub(super) static ACQUIRES: AtomicU64 = AtomicU64::new(0);
    pub(super) static WAIT_CYCLES: AtomicU64 = AtomicU64::new(0);
    pub(super) static HOLD_CYCLES: AtomicU64 = AtomicU64::new(0);
    pub(super) static MAX_WAIT: AtomicU64 = AtomicU64::new(0);
    pub(super) static MAX_HOLD: AtomicU64 = AtomicU64::new(0);

    pub fn enable() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn stamp() -> u64 {
        if ENABLED.load(Ordering::Relaxed) {
            slopos_arch::tsc::rdtsc()
        } else {
            0
        }
    }

    /// `(acquires, wait cycles, hold cycles, longest wait, longest hold)`.
    pub fn totals() -> (u64, u64, u64, u64, u64) {
        (
            ACQUIRES.load(Ordering::Relaxed),
            WAIT_CYCLES.load(Ordering::Relaxed),
            HOLD_CYCLES.load(Ordering::Relaxed),
            MAX_WAIT.load(Ordering::Relaxed),
            MAX_HOLD.load(Ordering::Relaxed),
        )
    }
}

/// The mount lock's guard, timing its hold when `prof=on`.
struct CachedGuard<'a> {
    guard: MutexGuard<'a, Option<CachedExt2>>,
    acquired: u64,
}

impl core::ops::Deref for CachedGuard<'_> {
    type Target = Option<CachedExt2>;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl core::ops::DerefMut for CachedGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

impl Drop for CachedGuard<'_> {
    fn drop(&mut self) {
        if self.acquired != 0 {
            let held = slopos_arch::tsc::rdtsc().saturating_sub(self.acquired);
            lock_profile::HOLD_CYCLES.fetch_add(held, Ordering::Relaxed);
            lock_profile::MAX_HOLD.fetch_max(held, Ordering::Relaxed);
        }
    }
}

const EXT2_ROOT_INODE: u32 = 2;

struct CachedExt2 {
    /// Sole writable handle to the backing device, held for the kernel's
    /// lifetime so no second writer can be acquired.
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    /// `s_r_blocks_count`: what an unprivileged allocation must leave free.
    /// Read once at mount, because it moves only when `tune2fs` moves it.
    reserved_blocks: u32,
    /// Sized to `block_size` at mount.
    cache: KBox<BlockCache>,
    /// Free-count drift from a mutating op. Lives here — not only on the
    /// per-call `Ext2Fs` handle — so a later sync sees earlier ops' dirtiness.
    superblock_dirty: bool,
    /// The log's own file, resolved at mount whether or not a log was
    /// attached. Readers of it are refused either way.
    journal_inode: Option<u32>,
    /// Every handle built over this mount refuses mutation. Not derivable
    /// from the per-call handle's superblock: `NotCleanlyUnmounted` is decided
    /// against the disk state the mount stamp then overwrites, and
    /// `ErrorsRemountRo` is a runtime verdict.
    read_only: bool,
    writeback: Writeback,
}

/// The mount's one writeback pass, which every caller that needs one drives:
/// the flusher, `sync`, and a writer short of log room. Passes that each
/// opened their own would repeat the check point's copies and barriers.
#[derive(Default)]
struct Writeback {
    open: Option<SyncPass>,
    /// Passes opened on this mount; the open one, if any, is the latest.
    opened: u64,
    /// The latest pass that ran to its end.
    finished: u64,
}

/// What a caller of [`Ext2Mount::writeback`] waits for.
#[derive(Clone, Copy)]
enum Want {
    /// Everything dirty when the caller arrived is on the medium: a pass
    /// opened after it did has finished.
    Durable,
    /// The log has room for an ordinary operation.
    LogRoom,
}

/// How the device came up at mount, for the boot log and the mounter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext2MountInfo {
    pub verity: VerityStatus,
    pub read_only: bool,
    /// Why writes are refused, when they are.
    pub read_only_reason: Option<ReadOnlyReason>,
    /// Inodes the previous boot left unlinked-but-open, reclaimed at this
    /// mount.
    pub orphans_drained: u32,
    /// `e2fsck`'s own mount-count or check-interval rule says the image is due
    /// a check. Reported, never acted on: this kernel runs no fsck.
    pub check_overdue: bool,
}

/// One mounted ext2 filesystem: the device, its cache and its state.
///
/// Identity is the instance's address — `vfs::traits::same_filesystem`
/// compares data pointers — so N instances are N filesystems to the VFS, each
/// with a lock of its own rather than one global mutex.
pub struct Ext2Mount {
    /// A *sleeping* mutex: ext2 block-device I/O waits are scheduler-backed,
    /// so the holder may legitimately deschedule mid-operation.
    cached: Mutex<Option<CachedExt2>>,
    init: InitFlag,
    /// Every `Ext2Fs` built over the mounted device refuses mutation. Outside
    /// the lock so a query never waits on in-flight block I/O.
    read_only: AtomicBool,
    /// Set when an operation flipped the mount read-only, as distinct from one
    /// that came up that way. Outside the lock, like [`Ext2Mount::read_only`].
    remount_ro_pending: AtomicBool,
    /// One-shot, so `errors=remount-ro` logs its cause once rather than on
    /// every subsequent operation.
    remount_ro_reported: InitFlag,
    /// Best-effort dirty-block count: the flusher's wait predicate reads only
    /// this and the stop flag, so it takes no lock.
    dirty_pending: AtomicUsize,
}

static FLUSH_STOP: KernelIoStop = KernelIoStop::new(
    "ext2-flush",
    lock_class!("EXT2_FLUSH_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static FLUSH_THREAD_STARTED: InitFlag = InitFlag::new();

/// Periodic writeback cadence — analog of Linux `dirty_writeback_centisecs`.
const FLUSH_INTERVAL_MS: u64 = 5_000;
/// Eager-wake threshold — analog of `dirty_background_ratio`. Past this many
/// dirty blocks a mutating op kicks the flusher instead of awaiting the tick.
const FLUSH_EAGER_THRESHOLD: usize = 48;
/// First retry delay after a failed sync (exponential backoff floor).
const FLUSH_BACKOFF_MIN_MS: u64 = 50;
/// Backoff ceiling — a persistently failing device is retried no more often
/// than the periodic flush cadence.
const FLUSH_BACKOFF_MAX_MS: u64 = FLUSH_INTERVAL_MS;

impl Ext2Mount {
    pub const fn new_const(class: &'static LockClassKey) -> Self {
        Self {
            cached: Mutex::new(None, class),
            init: InitFlag::new(),
            read_only: AtomicBool::new(false),
            remount_ro_pending: AtomicBool::new(false),
            remount_ro_reported: InitFlag::new(),
            dirty_pending: AtomicUsize::new(0),
        }
    }

    pub fn is_initialized(&self) -> bool {
        self.init.is_set()
    }

    /// Whether this mount refuses every mutation. `false` when nothing is
    /// mounted on it.
    pub fn is_read_only(&self) -> bool {
        self.init.is_set() && self.read_only.load(Ordering::Acquire)
    }

    fn lock_cached(&self) -> WaitResult<CachedGuard<'_>> {
        let began = lock_profile::stamp();
        let guard = self.cached.lock()?;
        let acquired = lock_profile::stamp();
        if began != 0 {
            let waited = acquired.saturating_sub(began);
            lock_profile::ACQUIRES.fetch_add(1, Ordering::Relaxed);
            lock_profile::WAIT_CYCLES.fetch_add(waited, Ordering::Relaxed);
            lock_profile::MAX_WAIT.fetch_max(waited, Ordering::Relaxed);
        }
        Ok(CachedGuard { guard, acquired })
    }

    fn with_fs<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        if !self.init.is_set() {
            return Err(VfsError::IoError);
        }
        let mut guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        // The check point is the lock owner's, not the operation's:
        // `Ext2Fs::transaction` would sync the whole filesystem inside this
        // one hold, while the chunked drain here gives the lock back every
        // [`WRITEBACK_CHUNK`] writes.
        let short = match guard.as_ref() {
            Some(cached) => !cached.cache.journal_has_headroom(),
            None => return Err(VfsError::IoError),
        };
        if short {
            drop(guard);
            // Best-effort: a failure here leaves `transaction`'s own fallback
            // to try again and report it.
            let _ = self.writeback(Want::LogRoom);
            guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        }
        let cached = guard.as_mut().ok_or(VfsError::IoError)?;
        let result = self.with_cached_fs(cached, f);
        // The log filling is its own reason to wake the flusher: draining it
        // there is a bounded pass, whereas at its low-water mark the next
        // operation pays an unbounded one under the lock.
        let drain = cached.cache.journal_needs_drain();
        self.note_dirty(cached.cache.dirty_count());
        if drain {
            FLUSH_STOP.wake_one_for_work();
        }
        drop(guard);
        // Off-lock: the log line must not be emitted while every path walk on
        // this mount is queued behind the lock it would hold.
        self.report_remount_ro_if_pending();
        result
    }

    /// Run one ext2 operation over `cached`, publishing everything it moved.
    ///
    /// The `Ext2Fs` handle is per-call and the mount state is not, so this is
    /// the one place that copies between them: the superblock, the dirty flag,
    /// and the corruption verdict that latches the mount read-only.
    fn with_cached_fs<R>(
        &self,
        cached: &mut CachedExt2,
        f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>,
    ) -> VfsResult<R> {
        let (superblock, block_size, inode_size) =
            (cached.superblock, cached.block_size, cached.inode_size);
        let mut fs = Ext2Fs::new(
            &*cached.device,
            &mut cached.cache,
            superblock,
            block_size,
            inode_size,
        )
        .map_err(ext2_error_to_vfs)?;
        fs.set_superblock_dirty(cached.superblock_dirty);
        if cached.read_only {
            fs.force_read_only();
        }
        // Per call, because the mount is shared and the entitlement is the
        // caller's.
        if !current_task_is_privileged() {
            fs.set_block_reserve(cached.reserved_blocks);
        }
        // Charged whatever the entitlement: spending the system reserve says
        // nothing about how much of the volume one principal may hold.
        fs.set_account(current_task_account());
        fs.set_journal_inode(cached.journal_inode);
        // Classified on reads too, not only in the mutating entry points: a
        // read that finds a group descriptor pointing outside the volume is
        // the same evidence of damage, and a mount that keeps writing after
        // one is what `errors=remount-ro` exists to stop.
        let raw = f(&mut fs);
        let result = fs.note_result(raw).map_err(ext2_error_to_vfs);
        // Published unconditionally: every mutating entry point rolls its own
        // dirtied blocks and free counts back on failure, so the post-state is
        // the committed one either way.
        let new_superblock = fs.superblock();
        let new_superblock_dirty = fs.superblock_dirty();
        let corrupted = fs.corruption_seen();
        drop(fs);

        // Deliberately no per-op flush: dirty blocks stay in the persistent
        // cache until eviction, the background flusher, `sync`, or shutdown.
        cached.superblock = new_superblock;
        cached.superblock_dirty = new_superblock_dirty;
        if corrupted && !cached.read_only {
            // `errors=remount-ro`. The damage is on the disk rather than in
            // this operation, and writing on into a filesystem known to be
            // damaged turns a repairable image into a lost one, so it is the
            // *mount* that stops writing.
            cached.read_only = true;
            self.read_only.store(true, Ordering::Release);
            self.remount_ro_pending.store(true, Ordering::Release);
        }
        result
    }

    /// The one-shot `errors=remount-ro` log line, emitted off the mount lock.
    fn report_remount_ro_if_pending(&self) {
        if !self.remount_ro_pending.load(Ordering::Acquire) || !self.remount_ro_reported.init_once()
        {
            return;
        }
        klog_info!(
            "ext2: filesystem error — remounting read-only. Repair with e2fsck on the host."
        );
    }

    /// Never holds the FS lock.
    fn note_dirty(&self, dirty: usize) {
        self.dirty_pending.store(dirty, Ordering::Relaxed);
        if dirty >= FLUSH_EAGER_THRESHOLD {
            FLUSH_STOP.wake_one_for_work();
        }
    }

    /// What the flusher's park predicate reads for this slot.
    pub(crate) fn dirty_pending(&self) -> usize {
        self.dirty_pending.load(Ordering::Relaxed)
    }

    /// Wake the flusher to complete a deferred inode free.
    ///
    /// The last close of an unlinked file cannot do the free itself: it runs
    /// from a `Drop` the task-exit path reaches under a preempt guard, and the
    /// free takes a sleeping mutex and parks on block I/O.
    fn wake_for_detached(&self) {
        if self.init.is_set() {
            FLUSH_STOP.wake_one_for_work();
        }
    }

    /// Commit one inode. Takes the same lock every other ext2 operation does,
    /// but writes only that inode's blocks — so a descriptor-granular `fsync`
    /// no longer drags every other file's dirty state to the device with it.
    fn sync_one_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()> {
        if !self.init.is_set() {
            return Ok(());
        }
        let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
        let mut guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        let Some(cached) = guard.as_mut() else {
            return Ok(());
        };
        let result = self.with_cached_fs(cached, |fs| fs.sync_inode(ino, data_only));
        // Not a `CLEAN_THROUGH` publication: this committed one inode, so a
        // later whole-filesystem `sync` still owes the device everything else.
        self.dirty_pending
            .store(cached.cache.dirty_count(), Ordering::Relaxed);
        drop(guard);
        self.report_remount_ro_if_pending();
        result
    }

    /// The pages this mount's block cache would give back under pressure.
    /// `try_lock` only: the mount lock is a sleeping mutex held across block
    /// I/O, so waiting here blocks on the I/O that needs the memory.
    fn reclaimable_pages(&self) -> u32 {
        let Some(guard) = self.cached.try_lock() else {
            return 0;
        };
        guard
            .as_ref()
            .map_or(0, |cached| cached.cache.reclaimable())
    }

    fn reclaim_clean(&self, want: u32) -> u32 {
        let Some(mut guard) = self.cached.try_lock() else {
            return 0;
        };
        guard
            .as_mut()
            .map_or(0, |cached| cached.cache.shrink_clean(want))
    }
}

trait Ext2VfsBackend {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R>;
    /// The flusher kthread is what drains a filesystem's deferred frees.
    fn ext2_wake_for_detached(&self);
    fn ext2_sync(&self) -> VfsResult<()>;
    fn ext2_statfs(&self) -> VfsResult<FsStats>;
    fn ext2_sync_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()>;
}

impl Ext2VfsBackend for Ext2Mount {
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R> {
        self.with_fs(f)
    }

    fn ext2_wake_for_detached(&self) {
        self.wake_for_detached();
    }

    fn ext2_sync(&self) -> VfsResult<()> {
        self.sync_fs()
    }

    fn ext2_statfs(&self) -> VfsResult<FsStats> {
        self.statfs()
    }

    fn ext2_sync_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()> {
        self.sync_one_inode(inode, data_only)
    }
}

impl<T: Ext2VfsBackend + Send + Sync> FileSystem for T {
    fn name(&self) -> &'static str {
        "ext2"
    }

    fn root_inode(&self) -> InodeId {
        EXT2_ROOT_INODE as InodeId
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            fs.lookup_child(parent as u32, name)
                .map(|ino| ino.raw() as InodeId)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            Ok(FileStat {
                inode,
                file_type: inode_to_file_type(&ext2_inode),
                size: ext2_inode.size as u64,
                mode: ext2_inode.mode,
                nlink: ext2_inode.links_count as u32,
                uid: ext2_inode.uid as u32,
                gid: ext2_inode.gid as u32,
                atime: ext2_inode.atime as u64,
                mtime: ext2_inode.mtime as u64,
                ctime: ext2_inode.ctime as u64,
                dev_major: 0,
                dev_minor: 0,
                // `EXT2_IMMUTABLE_FL` is the carrier, so the seal survives a
                // reboot and reads as one to `lsattr` and `e2fsck`.
                sealed: ext2_inode.is_immutable(),
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file(inode as u32, offset, buf))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.write_file(inode as u32, offset, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                _ => return Err(Ext2Error::InvalidInode),
            };
            Ok(inode as InodeId)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_ext2(|fs| fs.unlink_entry(parent as u32, name))
    }

    fn detach(&self, parent: InodeId, name: &[u8]) -> VfsResult<Option<InodeId>> {
        self.with_ext2(|fs| fs.detach_entry(parent as u32, name))
            .map(|o| o.map(|ino| ino as InodeId))
    }

    fn release_detached(&self, inode: InodeId) -> VfsResult<()> {
        let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
        self.with_ext2(|fs| fs.release_orphan(ino))
    }

    fn wake_for_detached(&self) -> bool {
        self.ext2_wake_for_detached();
        true
    }

    fn rmdir(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.with_ext2(|fs| fs.remove_directory(parent as u32, name))
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let mut count = 0usize;
        self.readdir_cookie(inode, offset as u64, &mut |_, name, ino, ft| {
            count += 1;
            callback(name, ino, ft)
        })?;
        Ok(count)
    }

    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        self.with_ext2(|fs| {
            let ext2_inode = fs.read_inode(inode as u32)?;
            if !ext2_inode.is_directory() {
                return Err(Ext2Error::NotDirectory);
            }
            fs.for_each_dir_entry_from(inode as u32, cookie, |next, entry| {
                let ft = ext2_file_type_to_vfs(entry.file_type);
                callback(next, entry.name, entry.inode.raw() as InodeId, ft)
            })
        })
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        self.with_ext2(|fs| fs.truncate_file(inode as u32, size))
    }

    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        self.with_ext2(|fs| {
            fs.rename_entry(old_parent as u32, old_name, new_parent as u32, new_name)
        })
    }

    fn rename_detaching(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<Option<InodeId>> {
        self.with_ext2(|fs| {
            fs.rename_entry_with(
                old_parent as u32,
                old_name,
                new_parent as u32,
                new_name,
                crate::ext2::LastLink::Orphan,
            )
        })
        .map(|o| o.map(|ino| ino as InodeId))
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_symlink(inode as u32, buf))
    }

    fn symlink(&self, parent: InodeId, name: &[u8], target: &[u8]) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            fs.create_symlink(parent as u32, name, target)
                .map(|i| i as InodeId)
        })
    }

    fn link(&self, parent: InodeId, name: &[u8], target: InodeId) -> VfsResult<()> {
        let parent = u32::try_from(parent).map_err(|_| VfsError::InvalidArgument)?;
        let target = u32::try_from(target).map_err(|_| VfsError::InvalidArgument)?;
        self.with_ext2(|fs| fs.link_entry(parent, name, target))
            .map_err(|e| match e {
                // `link_entry` reports a directory source this way, and POSIX
                // spells that refusal `EPERM` rather than `EISDIR`.
                VfsError::IsDirectory => VfsError::PermissionDenied,
                other => other,
            })
    }

    fn set_times(&self, inode: InodeId, atime: Option<u64>, mtime: Option<u64>) -> VfsResult<()> {
        let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
        self.with_ext2(|fs| fs.set_times(ino, atime, mtime))
    }

    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_mode(inode as u32, mode))
    }

    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_sealed(inode as u32))
    }

    fn sync(&self) -> VfsResult<()> {
        self.ext2_sync()
    }

    fn statfs(&self) -> VfsResult<FsStats> {
        self.ext2_statfs()
    }

    fn sync_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()> {
        self.ext2_sync_inode(inode, data_only)
    }
}

impl Ext2Mount {
    /// Mount the ext2 image on `device` into this instance. A verity trailer
    /// makes the mount read-only; one that is present but unusable refuses the
    /// mount, so an image claiming attestation is never read unverified.
    ///
    /// `read_only` is the mounter's intent, not an observation of the device:
    /// a device that would accept writes must still yield an instance that
    /// refuses them.
    pub fn attach(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        read_only: bool,
    ) -> VfsResult<Ext2MountInfo> {
        // A second call must error rather than silently drop the caller's
        // capability token, which would release the exclusive write claim.
        if !self.init.init_once() {
            return Err(VfsError::AlreadyExists);
        }
        match self.mount_device(device, read_only) {
            Ok(info) => Ok(info),
            Err(e) => {
                // `init` stays set when the device could not be taken back:
                // an instance that reports uninitialised while still holding
                // one is invisible to every sweep that would free it.
                if self.clear_cached() {
                    self.init.reset();
                }
                Err(e)
            }
        }
    }

    /// Tear this instance down: what is dirty reaches the device, the image is
    /// declared clean, and the [`CachedExt2`] goes — with it the device and
    /// its exclusive write claim, so the same disk can be mounted again.
    ///
    /// `false` when the device is still attached, which leaves the instance
    /// initialised: the caller owes it another attempt.
    pub fn detach(&self) -> bool {
        if !self.init.is_set() {
            return true;
        }
        let _ = self.sync_fs();
        self.mark_filesystem_clean();
        if !self.clear_cached() {
            return false;
        }
        self.init.reset();
        self.read_only.store(false, Ordering::Release);
        self.remount_ro_pending.store(false, Ordering::Release);
        self.remount_ro_reported.reset();
        self.dirty_pending.store(0, Ordering::Relaxed);
        true
    }

    /// Drop the device outside the mount lock: the write token's `Drop` takes
    /// the block registry, and nothing needs those two held at once. `false`
    /// when the lock could not be taken at all — `Mutex::lock` aborts for a
    /// task marked for death — leaving the instance still owning its device.
    fn clear_cached(&self) -> bool {
        let Ok(mut guard) = self.lock_cached() else {
            return false;
        };
        let stale = guard.take();
        drop(guard);
        drop(stale);
        true
    }

    fn mount_device(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        requested_read_only: bool,
    ) -> VfsResult<Ext2MountInfo> {
        // The superblock is read off the raw device: a trailer can only be
        // recognised relative to the extent the filesystem claims, and the
        // sub-block read is one verity would not check anyway.
        let (superblock, block_size, inode_size) =
            Ext2Fs::mount_params(&*device).map_err(ext2_error_to_vfs)?;
        let extent = FsExtent {
            block_size,
            blocks: superblock.blocks_count as u64,
        };
        // An image the last boot never marked clean may have blocks rewritten
        // after its bitmap was persisted, so its attestation is stale this
        // boot.
        let trust = if superblock.state == crate::ext2::ondisk::EXT2_VALID_FS {
            AttestTrust::Persisted
        } else {
            AttestTrust::NoneThisBoot
        };
        let (device, verity) = crate::verity::build_verified_trusting(device, extent, trust)
            .map_err(|e| {
                klog_info!("verity: refusing to mount — {:?}", e);
                verity_error_to_vfs(e)
            })?;
        log_verity_status(verity);
        // Asked against the superblock as it came off the disk:
        // `install_cached` stamps `EXT2_ERROR_FS` into it, and asking after
        // would read this mount's own stamp as the previous mount's crash.
        let read_only_reason = if requested_read_only {
            Some(ReadOnlyReason::Requested)
        } else {
            Ext2Fs::mount_read_only_reason(&superblock, &*device)
        };
        let read_only = read_only_reason.is_some();
        self.read_only.store(read_only, Ordering::Release);
        self.install_cached(device, superblock, block_size, inode_size, read_only)?;

        // The log is attached before the read-only verdict is logged, because
        // a replay can retract the only reason there was one.
        let read_only_reason = self.attach_journal(read_only_reason);
        let read_only = read_only_reason.is_some();
        self.read_only.store(read_only, Ordering::Release);
        log_read_only_reason(read_only_reason);

        let (orphans_drained, check_overdue) = self.post_mount_recovery();

        if !read_only {
            start_flusher();
        }
        Ok(Ext2MountInfo {
            verity,
            read_only,
            read_only_reason,
            orphans_drained,
            check_overdue,
        })
    }

    /// Attach the metadata log and answer the read-only reason that survives
    /// it. An unclean image refuses writes because nothing could say what the
    /// last boot left half-done; a replayed log is that evidence, so the
    /// refusal is lifted.
    #[inline(never)]
    fn attach_journal(&self, reason: Option<ReadOnlyReason>) -> Option<ReadOnlyReason> {
        let Ok(mut guard) = self.lock_cached() else {
            return reason;
        };
        let Some(cached) = guard.as_mut() else {
            return reason;
        };
        // A log is attachable only on a handle that may write, so the unclean
        // latch is lifted for the attempt and restored if it finds nothing.
        let recoverable = reason == Some(ReadOnlyReason::NotCleanlyUnmounted);
        if recoverable {
            cached.read_only = false;
        }
        let mut journal_inode = None;
        let recovery = self.with_cached_fs(cached, |fs| {
            let outcome = fs.attach_journal();
            journal_inode = fs.journal_inode();
            outcome
        });
        let outcome = match recovery {
            Ok(Some(recovery)) => recovery,
            Ok(None) => crate::ext2::journal::JournalRecovery::NONE,
            Err(e) => {
                klog_info!("ext2: journal attach failed: {:?}", e);
                crate::ext2::journal::JournalRecovery::NONE
            }
        };
        match cached.cache.journal() {
            Some(journal) => klog_info!(
                "ext2: metadata log attached — {} slots at inode {}, replayed {} transactions ({} blocks)",
                journal.capacity(),
                journal.inode(),
                outcome.transactions,
                outcome.blocks,
            ),
            None if cached.read_only => {
                klog_info!(
                    "ext2: no metadata log — the mount refuses writes, and a replay is a write"
                )
            }
            None => klog_info!(
                "ext2: no metadata log ({} absent or not preallocated) — operations \
                 are undo-scoped and an unclean image stays read-only",
                core::str::from_utf8(crate::ext2::JOURNAL_PATH).unwrap_or("/.journal"),
            ),
        }
        cached.journal_inode = journal_inode;
        let keep = if recoverable && outcome.replayed() {
            klog_info!("ext2: the replay is what makes this mount writable again");
            None
        } else {
            reason
        };
        // Never *clears* a latch: `with_cached_fs` raises one of its own when
        // the attach finds the image or the device damaged.
        cached.read_only = keep.is_some() || cached.read_only;
        if recoverable && keep.is_none() && !cached.read_only {
            // The mount skipped its own not-clean stamp while it was refusing
            // writes; the log is what makes writing safe again, so it owes it
            // now.
            stamp_not_clean(cached);
        }
        if cached.read_only {
            keep.or(Some(ReadOnlyReason::ErrorsRemountRo))
        } else {
            keep
        }
    }

    /// Reclaim what the previous boot left unlinked-but-open, and ask whether
    /// the image is due a check. Both need the mount published, so neither can
    /// happen inside [`Ext2Mount::install_cached`].
    #[inline(never)]
    fn post_mount_recovery(&self) -> (u32, bool) {
        let Ok(mut guard) = self.lock_cached() else {
            return (0, false);
        };
        let Some(cached) = guard.as_mut() else {
            return (0, false);
        };
        let drained = self.with_cached_fs(cached, |fs| {
            let drained = fs.drain_orphans()?;
            let overdue = fs
                .read_bookkeeping()?
                .check_overdue(slopos_kernel_services::clock::realtime_unix_secs());
            Ok((drained, overdue))
        });
        drop(guard);
        match drained {
            Ok((n, overdue)) => (n, overdue),
            Err(e) => {
                klog_info!("ext2: orphan drain failed: {:?}", e);
                (0, false)
            }
        }
    }
}

#[inline(never)]
fn log_read_only_reason(reason: Option<ReadOnlyReason>) {
    let Some(reason) = reason else {
        return;
    };
    match reason {
        ReadOnlyReason::Requested => {
            klog_info!("ext2: mounting read-only — the mount asked for it")
        }
        ReadOnlyReason::DeviceWriteProtected => {
            klog_info!("ext2: mounting read-only — the device is verity-attested")
        }
        ReadOnlyReason::UnsupportedFeature => klog_info!(
            "ext2: mounting read-only — the image declares a feature this kernel does not write"
        ),
        // Loud on purpose: the image is safe to read and unsafe to write, and
        // a silently read-only root is the failure mode this line prevents.
        ReadOnlyReason::NotCleanlyUnmounted => klog_info!(
            "ext2: MOUNTING READ-ONLY — the image was never marked clean, so the last \
             boot crashed or is still running. Repair it on the host with \
             `e2fsck -fy <image>`; until then every write returns EROFS."
        ),
        ReadOnlyReason::ErrorsRemountRo => {
            klog_info!("ext2: mounting read-only — a previous error latched the mount")
        }
    }
}

#[inline(never)]
fn log_verity_status(verity: VerityStatus) {
    match verity {
        VerityStatus::Absent => klog_info!("verity: no trailer — image mounts unverified"),
        VerityStatus::Verified { blocks, block_size } => klog_info!(
            "verity: enabled — {} blocks of {} bytes, device write-protected",
            blocks,
            block_size,
        ),
        VerityStatus::VerifiedWritable {
            blocks,
            block_size,
            attested,
        } => klog_info!(
            "verity: enabled — {} of {} blocks of {} bytes still attested, device writable",
            attested,
            blocks,
            block_size,
        ),
    }
}

impl Ext2Mount {
    /// Build the cache, publish `cached` and stamp the not-clean bit. Its own
    /// frame so the cache temporaries do not share one with the verity parse.
    #[inline(never)]
    fn install_cached(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        superblock: Ext2Superblock,
        block_size: u32,
        inode_size: u16,
        read_only: bool,
    ) -> VfsResult<()> {
        // Zero on a device that cannot answer: a reserve of zero refuses
        // nothing, rather than failing a mount that would otherwise succeed.
        let reserved_blocks = Ext2Fs::read_block_reserve(&*device).unwrap_or(0);
        let target_entries =
            cache_entries_for(superblock.blocks_count as u64, superblock.blocks_per_group);
        let cache = BlockCache::new_boxed(block_size, target_entries).map_err(ext2_error_to_vfs)?;
        let mut guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        *guard = Some(CachedExt2 {
            device,
            superblock,
            block_size,
            inode_size: if inode_size == 0 { 128 } else { inode_size },
            reserved_blocks,
            cache,
            superblock_dirty: false,
            journal_inode: None,
            read_only,
            writeback: Writeback::default(),
        });
        if let Some(cached) = guard.as_mut() {
            stamp_not_clean(cached);
        }
        Ok(())
    }

    /// Capacity of this filesystem, off the in-memory superblock. Takes the
    /// mount lock, so a `statfs` racing a write sees one side of it or the
    /// other, but issues no block I/O.
    fn statfs(&self) -> VfsResult<FsStats> {
        if !self.init.is_set() {
            return Err(VfsError::IoError);
        }
        let guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        let cached = guard.as_ref().ok_or(VfsError::IoError)?;
        Ok(ext2_stats_of(
            &cached.superblock,
            cached.block_size,
            cached.reserved_blocks,
            cached.read_only,
        ))
    }
}

/// The not-clean bit is what tells a later fsck it must run; without it a
/// crash leaves an image that still claims to be clean. A no-op on a
/// read-only handle, so a write-protected device is never touched.
#[inline(never)]
fn stamp_not_clean(cached: &mut CachedExt2) {
    if cached.read_only {
        return;
    }
    let (sb, bs, is) = (cached.superblock, cached.block_size, cached.inode_size);
    let Ok(mut fs) = Ext2Fs::new(&*cached.device, &mut cached.cache, sb, bs, is) else {
        return;
    };
    if fs.mark_dirty_on_disk().is_ok() {
        cached.superblock = fs.superblock();
    }
}

/// The superblock's counts as `statfs(2)` wants them. Split from the mount
/// lookup so a test can call it on an image it mounted itself.
pub(crate) fn ext2_stats_of(
    superblock: &Ext2Superblock,
    block_size: u32,
    reserved_blocks: u32,
    read_only: bool,
) -> FsStats {
    let free_blocks = u64::from(superblock.free_blocks_count);
    FsStats {
        magic: slopos_abi::fs::EXT2_SUPER_MAGIC,
        block_size,
        blocks: u64::from(superblock.blocks_count),
        blocks_free: free_blocks,
        // The reserve is what the allocator refuses an unprivileged writer, so
        // reporting it as available would be a lie the next `write` contradicts.
        blocks_available: free_blocks.saturating_sub(u64::from(reserved_blocks)),
        inodes: u64::from(superblock.inodes_count),
        inodes_free: u64::from(superblock.free_inodes_count),
        // The VFS limit, not ext2's own 255: a longer name is refused
        // `ENAMETOOLONG` before this filesystem ever sees it.
        max_name_len: crate::MAX_NAME_LEN as u32,
        read_only,
    }
}

fn verity_error_to_vfs(e: VerityError) -> VfsError {
    match e {
        VerityError::UnsupportedTrailer | VerityError::TooLarge => VfsError::NotSupported,
        VerityError::CorruptTrailer
        | VerityError::Geometry
        | VerityError::Device
        | VerityError::OutOfMemory => VfsError::IoError,
    }
}

/// Device writes one holder of the mount lock may issue before giving it back.
/// Small enough that a path walk behind a pass waits for a bounded number of
/// round trips, large enough that the extra acquisitions are noise.
pub(crate) const WRITEBACK_CHUNK: usize = 32;

/// Steps one caller may take before it gives up. A pass advances a phase or
/// writes a block on every step, so this bounds a livelock rather than the
/// work: reaching it means the device is failing every write.
const WRITEBACK_MAX_STEPS: usize = 4096;

/// What one call of [`Ext2Mount::writeback_step`] did.
enum Progress {
    /// The caller's [`Want`] holds, so it is done.
    Met,
    /// The mount's pass advanced by one step.
    Stepped,
}

impl Ext2Mount {
    /// Takes the FS lock, so the caller must hold none.
    ///
    /// Waits for a pass opened after the call, which the caller drives step by
    /// step with every other caller of the mount's one pass. The wait is
    /// bounded: the pass releases the mount lock every [`WRITEBACK_CHUNK`]
    /// writes, and the epoch it fixed keeps the ordered phases ordered across
    /// those gaps.
    pub fn sync_fs(&self) -> VfsResult<()> {
        self.sync_pass().0
    }

    /// [`Self::sync_fs`], plus how many [`Ext2Fs::sync_step`] calls this
    /// caller made — the count that bounds its wait, since every step gave the
    /// mount lock back.
    pub(crate) fn sync_pass(&self) -> (VfsResult<()>, usize) {
        self.writeback(Want::Durable)
    }

    fn writeback(&self, want: Want) -> (VfsResult<()>, usize) {
        if !self.init.is_set() {
            return (Ok(()), 0);
        }
        let mut target = None;
        let mut steps = 0usize;
        for _ in 0..WRITEBACK_MAX_STEPS {
            match self.writeback_step(want, &mut target) {
                Ok(Progress::Met) => return (Ok(()), steps),
                Ok(Progress::Stepped) => steps += 1,
                Err(e) => return (Err(e), steps),
            }
        }
        (Ok(()), steps)
    }

    /// One hold of the mount lock: answer whether `want` holds, or advance the
    /// mount's pass by a step, opening one if none is open. `target` is the
    /// first pass a [`Want::Durable`] caller can count, fixed on its first
    /// call.
    fn writeback_step(&self, want: Want, target: &mut Option<u64>) -> VfsResult<Progress> {
        let mut guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        let Some(cached) = guard.as_mut() else {
            return Ok(Progress::Met);
        };
        let target = *target.get_or_insert(cached.writeback.opened + 1);
        let met = match want {
            Want::Durable => cached.writeback.finished >= target,
            Want::LogRoom => cached.cache.journal_has_headroom(),
        };
        if met {
            return Ok(Progress::Met);
        }
        let mut pass = match cached.writeback.open {
            Some(pass) => pass,
            None => {
                // Read state, not a completion count: an op that *failed*
                // leaves its dirtied blocks cached, so a finished pass is not
                // evidence that there is nothing left to write.
                match self
                    .with_cached_fs(cached, |fs| Ok(fs.sync_pending().then(|| fs.begin_sync())))?
                {
                    Some(pass) => {
                        cached.writeback.opened += 1;
                        pass
                    }
                    None => {
                        self.dirty_pending.store(0, Ordering::Relaxed);
                        return Ok(Progress::Met);
                    }
                }
            }
        };
        let result = self.with_cached_fs(cached, |fs| fs.sync_step(&mut pass, WRITEBACK_CHUNK));
        cached.writeback.open = match result {
            Ok(()) if pass.is_done() => {
                cached.writeback.finished = cached.writeback.opened;
                None
            }
            Ok(()) => Some(pass),
            Err(_) => None,
        };
        self.dirty_pending
            .store(cached.cache.dirty_count(), Ordering::Relaxed);
        drop(guard);
        self.report_remount_ro_if_pending();
        result.map(|()| Progress::Stepped)
    }

    /// Open the mount's pass if none is, and advance it by one step.
    #[cfg(feature = "tests")]
    pub(crate) fn writeback_step_for_test(&self) -> VfsResult<()> {
        self.writeback_step(Want::Durable, &mut None).map(|_| ())
    }

    /// Passes opened on this mount and the latest that finished.
    #[cfg(feature = "tests")]
    pub(crate) fn writeback_passes_for_test(&self) -> (u64, u64) {
        let Ok(guard) = self.lock_cached() else {
            return (0, 0);
        };
        guard.as_ref().map_or((0, 0), |cached| {
            (cached.writeback.opened, cached.writeback.finished)
        })
    }

    /// Whether the log has room for an ordinary operation without a check
    /// point first. `true` with no log: there is nothing to run out of.
    #[cfg(feature = "tests")]
    pub(crate) fn journal_has_headroom(&self) -> bool {
        let Ok(guard) = self.lock_cached() else {
            return true;
        };
        guard
            .as_ref()
            .is_none_or(|cached| cached.cache.journal_has_headroom())
    }

    /// Whether the log holds transactions a mount would have to replay.
    #[cfg(feature = "tests")]
    pub(crate) fn journal_is_empty(&self) -> bool {
        let Ok(guard) = self.lock_cached() else {
            return true;
        };
        guard
            .as_ref()
            .is_none_or(|cached| cached.cache.journal_is_empty())
    }

    /// Declare the image clean on the medium whenever it genuinely is: nothing
    /// dirty, nothing unbarriered, no superblock drift, an empty log. Runs at
    /// shutdown and from every idle flusher pass, so a rude power-off leaves
    /// an image that still mounts writable. The thaw is
    /// [`Ext2Fs::transaction`]'s re-stamp, which reaches the medium before any
    /// write it covers.
    fn mark_filesystem_clean(&self) {
        if !self.init.is_set() {
            return;
        }
        let Ok(mut guard) = self.lock_cached() else {
            return;
        };
        let Some(cached) = guard.as_mut() else {
            return;
        };
        if cached.read_only || cached.superblock.state == crate::ext2::ondisk::EXT2_VALID_FS {
            return;
        }
        // A non-empty log counts as unflushed state: stamping clean over one
        // tells the next mount there is nothing to replay while the homes
        // still lack it.
        if cached.cache.dirty_count() > 0
            || cached.cache.unbarriered_writes() > 0
            || cached.superblock_dirty
            || !cached.cache.journal_is_empty()
        {
            return;
        }
        // The verity attested bitmap goes down, and is flushed, BEFORE the
        // clean stamp: a crash in between leaves the image not clean, so the
        // next mount trusts no attestation rather than verifying a block this
        // boot rewrote against a stale bitmap.
        if let Err(e) = cached.device.checkpoint() {
            klog_info!("verity: could not persist the attested bitmap: {:?}", e);
            return;
        }
        if let Err(e) = cached.device.flush() {
            klog_info!("ext2: device flush before the clean stamp failed: {:?}", e);
            return;
        }
        let (sb, bs, is) = (cached.superblock, cached.block_size, cached.inode_size);
        let Ok(mut fs) = Ext2Fs::new(&*cached.device, &mut cached.cache, sb, bs, is) else {
            return;
        };
        if fs.mark_clean().is_ok() {
            cached.superblock = fs.superblock();
            cached.superblock_dirty = fs.superblock_dirty();
        }
    }
}

/// Must be called with interrupts still enabled — the virtio-blk completion
/// path needs them. Best-effort, and over every bound instance: a second
/// filesystem's dirty blocks are as lost as the root's if nobody writes them.
pub fn ext2_vfs_shutdown_sync() {
    FLUSH_STOP.request();
    // Writeback goes before the syncs below, or a mapped page's bytes are lost
    // and the sync reports a clean image.
    crate::filemap::flush_all();
    crate::vfs::init::ext2_pool_for_each_bound(&mut |mount| {
        // An orphan whose last descriptor closed during teardown is one this
        // boot can still free, rather than one the next mount's drain pays for.
        orphan::drain_releasable(mount);
        let _ = mount.sync_fs();
        mount.mark_filesystem_clean();
    });
}

fn start_flusher() {
    if !FLUSH_THREAD_STARTED.init_once() {
        return;
    }
    if slopos_ostd::spawn_kernel_io!(&FLUSH_STOP, ext2_flusher_entry).is_err() {
        // Roll back so a later mount can retry the spawn; eviction, `sync` and
        // shutdown still persist without it.
        FLUSH_THREAD_STARTED.reset();
    }
}

/// Background writeback kthread (analog of Linux per-bdi flusher).
///
/// One thread for the whole pool: a pass takes one instance's lock at a time,
/// never two, so nothing here can order two mounts' locks against each other.
/// Persistent sync failures back off exponentially, because failed writes
/// leave blocks dirty and would otherwise satisfy the wait predicate
/// back-to-back.
fn ext2_flusher_entry(token: KernelIoToken<'static>) {
    let mut backoff_ms: u64 = 0;
    loop {
        let waited = if backoff_ms > 0 {
            token.park_timeout(&FLUSH_STOP, || false, backoff_ms)
        } else {
            token.park_timeout(
                &FLUSH_STOP,
                || {
                    crate::vfs::init::ext2_pool_has_dirty()
                        || orphan::releasable_count() > 0
                        || crate::filemap::pending_count() > 0
                },
                FLUSH_INTERVAL_MS,
            )
        };

        // A file mapping's writeback goes through the filesystem, so it cannot
        // run from the `release` that queued it.
        crate::filemap::drain_pending();

        // Sync on the stop path too: dirty blocks that never reach the device
        // are lost.
        let mut failed = false;
        crate::vfs::init::ext2_pool_for_each_bound(&mut |mount| {
            // Before the sync, so the frees it performs go out in the same
            // pass rather than waiting a further tick. Takes the mount lock
            // itself, so it must not run under one.
            orphan::drain_releasable(mount);
            if mount.sync_fs().is_err() {
                failed = true;
            } else {
                mount.mark_filesystem_clean();
            }
        });
        backoff_ms = if failed {
            (backoff_ms * 2).clamp(FLUSH_BACKOFF_MIN_MS, FLUSH_BACKOFF_MAX_MS)
        } else {
            0
        };
        if waited == KthreadWait::Stop {
            break;
        }
    }
    FLUSH_STOP.note_exited();
}

fn ext2_error_to_vfs(e: Ext2Error) -> VfsError {
    match e {
        Ext2Error::InvalidSuperblock => VfsError::IoError,
        Ext2Error::UnsupportedBlockSize => VfsError::IoError,
        Ext2Error::UnsupportedFeature => VfsError::NotSupported,
        Ext2Error::ReadOnly => VfsError::ReadOnly,
        Ext2Error::InvalidInode => VfsError::NotFound,
        Ext2Error::InvalidBlock => VfsError::IoError,
        // The caller's argument, not the image: `EINVAL`, and no latch.
        Ext2Error::InvalidRange => VfsError::InvalidArgument,
        Ext2Error::UnsupportedIndirection => VfsError::NotSupported,
        Ext2Error::DeviceError => VfsError::IoError,
        Ext2Error::DirectoryFormat => VfsError::IoError,
        Ext2Error::NotDirectory => VfsError::NotDirectory,
        Ext2Error::NotFile => VfsError::NotFile,
        Ext2Error::PathNotFound => VfsError::NotFound,
        Ext2Error::NoSpace => VfsError::NoSpace,
        Ext2Error::NameTooLong => VfsError::NameTooLong,
        Ext2Error::AlreadyExists => VfsError::AlreadyExists,
        Ext2Error::NotEmpty => VfsError::NotEmpty,
        Ext2Error::IsDirectory => VfsError::IsDirectory,
        Ext2Error::TooManyLinks => VfsError::TooManyLinks,
        Ext2Error::OutOfMemory => VfsError::IoError,
        Ext2Error::Immutable => VfsError::PermissionDenied,
        Ext2Error::InvalidPath => VfsError::InvalidPath,
        Ext2Error::Interrupted => VfsError::Interrupted,
    }
}

fn inode_to_file_type(inode: &Ext2Inode) -> FileType {
    let mode = inode.mode & 0xF000;
    match mode {
        0x4000 => FileType::Directory,
        0x8000 => FileType::Regular,
        0xA000 => FileType::Symlink,
        0x2000 => FileType::CharDevice,
        0x6000 => FileType::BlockDevice,
        0x1000 => FileType::Pipe,
        0xC000 => FileType::Socket,
        _ => FileType::Regular,
    }
}

fn ext2_file_type_to_vfs(file_type: u8) -> FileType {
    match file_type {
        1 => FileType::Regular,
        2 => FileType::Directory,
        3 => FileType::CharDevice,
        4 => FileType::BlockDevice,
        5 => FileType::Pipe,
        6 => FileType::Socket,
        7 => FileType::Symlink,
        _ => FileType::Regular,
    }
}

struct Ext2CacheReclaim;

impl slopos_ostd::mm::reclaim::Reclaimable for Ext2CacheReclaim {
    fn name(&self) -> &'static str {
        "ext2-page-cache"
    }

    fn reclaimable_pages(&self) -> u32 {
        let mut total = 0u32;
        crate::vfs::init::ext2_pool_for_each_bound(&mut |mount| {
            total = total.saturating_add(mount.reclaimable_pages());
        });
        total
    }

    fn reclaim(&self, want: u32) -> u32 {
        let mut freed = 0u32;
        crate::vfs::init::ext2_pool_for_each_bound(&mut |mount| {
            let left = want.saturating_sub(freed);
            if left > 0 {
                freed = freed.saturating_add(mount.reclaim_clean(left));
            }
        });
        freed
    }
}

static EXT2_CACHE_RECLAIM: Ext2CacheReclaim = Ext2CacheReclaim;

pub fn register_reclaim(token: &slopos_ostd::sync::BspToken<'_>) {
    slopos_ostd::mm::reclaim::register(token, &EXT2_CACHE_RECLAIM);
}
