use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::{LOCK_LEVEL_RESOURCE, LockClassKey};

use crate::blockdev::{BlockDevice, BlockDeviceError, WriteTicket};
use crate::ext2::cache::{BlockCache, DataBatch, cache_entries_for};
use crate::ext2::geometry::Ext2Geometry;
use crate::ext2::journal::AttachError;
use crate::ext2::ondisk::InodeTime;
use crate::ext2::{Ext2Error, Ext2Fs, Ext2Superblock, ReadOnlyReason, SyncPass};
use crate::ext2_dcache::{Ext2Dcache, InodeAttr, NameKey};
use crate::verity::{AttestTrust, FsExtent, VerityError, VerityStatus};
use crate::vfs::{
    FileStat, FileSystem, FileType, FsStats, InodeId, Timestamp, VfsError, VfsResult, orphan,
};
use slopos_kernel_services::driver_runtime::{current_task_account, current_task_is_privileged};
use slopos_ostd::KBox;
use slopos_ostd::authority::{Cap, Seal};
use slopos_ostd::klog_info;
use slopos_ostd::mm::KArc;
use slopos_ostd::sync::WaitQueue;
use slopos_ostd::sync::kernel_io_task::{KernelIoStop, KernelIoToken, KthreadWait};
use slopos_ostd::sync::{InitFlag, Mutex, MutexGuard, OnceLock, WaitResult};

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
    /// The part of the hold spent by writeback — the flusher's passes and
    /// commits, and `sync(2)` — rather than by the operations it serves.
    pub(super) static WRITEBACK_HOLD_CYCLES: AtomicU64 = AtomicU64::new(0);

    pub fn enable() {
        ENABLED.store(true, Ordering::Relaxed);
    }

    pub fn disable() {
        ENABLED.store(false, Ordering::Relaxed);
    }

    /// Zero the totals and every site's counts.
    pub fn reset() {
        for total in [
            &ACQUIRES,
            &WAIT_CYCLES,
            &HOLD_CYCLES,
            &MAX_WAIT,
            &MAX_HOLD,
            &WRITEBACK_HOLD_CYCLES,
        ] {
            total.store(0, Ordering::Relaxed);
        }
        SITES.reset_counts();
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

    /// Hold cycles spent by writeback.
    pub fn writeback_hold() -> u64 {
        WRITEBACK_HOLD_CYCLES.load(Ordering::Relaxed)
    }

    pub(super) static SITES: slopos_mm::lock_sites::SiteTable =
        slopos_mm::lock_sites::SiteTable::new(slopos_ostd::lock_class!(
            "EXT2_LOCK_SITES",
            slopos_ostd::sync::LOCK_LEVEL_UNORDERED
        ));

    /// Every call site seen: `(location, acquires, wait cycles, hold cycles)`.
    pub fn for_each_site(f: impl FnMut(&'static core::panic::Location<'static>, u64, u64, u64)) {
        SITES.for_each(f);
    }
}

/// The mount lock's guard, timing its hold when `prof=on`.
struct CachedGuard<'a> {
    guard: MutexGuard<'a, Option<CachedExt2>>,
    acquired: u64,
    writeback: bool,
    site: Option<&'static slopos_mm::lock_sites::Site>,
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
            if self.writeback {
                lock_profile::WRITEBACK_HOLD_CYCLES.fetch_add(held, Ordering::Relaxed);
            }
            if let Some(site) = self.site {
                site.note_hold(held);
            }
        }
    }
}

const EXT2_ROOT_INODE: u32 = 2;

/// A data writeback the flusher runs without the mount lock
/// ([`Ext2Mount::commit_log`]). While one is in flight every request the mount
/// makes of its device waits for it: a write must not overtake it, a barrier
/// must cover it, and a read must not find a block's home before the write
/// the cache already counts as done.
struct IoGate {
    busy: AtomicBool,
    /// The last batch left a run unwritten and its blocks are not dirty again
    /// yet: a barrier then answers an error, so no commit is made durable
    /// behind data that never reached its home.
    failed: AtomicBool,
    waiters: WaitQueue,
}

impl IoGate {
    fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            waiters: WaitQueue::new(lock_class!("EXT2_IO_GATE.waiters", LOCK_LEVEL_RESOURCE)),
        }
    }

    fn begin(&self) {
        self.busy.store(true, Ordering::Release);
    }

    fn end(&self, failed: bool) {
        self.failed.store(failed, Ordering::Release);
        self.busy.store(false, Ordering::Release);
        let _ = self.waiters.wake_all();
    }

    fn wait_idle(&self) -> Result<(), BlockDeviceError> {
        if !self.busy.load(Ordering::Acquire) {
            return Ok(());
        }
        self.waiters
            .wait_event(|| !self.busy.load(Ordering::Acquire))
            .map_err(|_| BlockDeviceError::Interrupted)
    }
}

type SharedDevice = KArc<KBox<dyn BlockDevice + Send + Sync>>;

/// The mount's view of its device: every request first waits out a batch the
/// flusher is writing behind the gate, which writes through `inner` itself.
struct GatedDevice {
    inner: SharedDevice,
    gate: KArc<IoGate>,
}

impl BlockDevice for GatedDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.gate.wait_idle()?;
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.gate.wait_idle()?;
        self.inner.write_at(offset, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.gate.wait_idle()?;
        self.inner.write_vectored(offset, segs)
    }

    fn submit_write(&self, offset: u64, segs: &[&[u8]]) -> Result<WriteTicket, BlockDeviceError> {
        self.gate.wait_idle()?;
        self.inner.submit_write(offset, segs)
    }

    fn complete_write(&self, ticket: WriteTicket) -> Result<(), BlockDeviceError> {
        self.inner.complete_write(ticket)
    }

    fn write_depth(&self) -> usize {
        self.inner.write_depth()
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn logical_block_size(&self) -> u32 {
        self.inner.logical_block_size()
    }

    fn write_protected(&self) -> bool {
        self.inner.write_protected()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        self.gate.wait_idle()?;
        if self.gate.failed.load(Ordering::Acquire) {
            return Err(BlockDeviceError::DeviceFault);
        }
        self.inner.flush()
    }

    fn checkpoint(&self) -> Result<(), BlockDeviceError> {
        self.gate.wait_idle()?;
        self.inner.checkpoint()
    }
}

struct CachedExt2 {
    /// Sole writable handle to the backing device, held for the kernel's
    /// lifetime so no second writer can be acquired.
    device: GatedDevice,
    superblock: Ext2Superblock,
    /// Derived again once the journal attaches; nothing written after moves it.
    geom: Ext2Geometry,
    /// `s_r_blocks_count`: what an unprivileged allocation must leave free.
    /// Read once at mount, because it moves only when `tune2fs` moves it.
    reserved_blocks: u32,
    /// Sized to the volume's block size at mount.
    cache: KBox<BlockCache>,
    /// Free-count drift from a mutating op. Lives here — not only on the
    /// per-call `Ext2Fs` handle — so a later sync sees earlier ops' dirtiness.
    superblock_dirty: bool,
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
    /// Log records appended but not yet written, for the commit timer.
    log_pending: AtomicUsize,
    /// Work that should not wait for the timer: [`WANT_COMMIT`] for a ring
    /// half full, [`WANT_FULL`] for dirty blocks past the background
    /// threshold or a log that needs a drain. The flusher takes them.
    wants: AtomicU8,
    /// A whole pass would write something: dirty blocks, unbarriered writes
    /// or a log to check point. What the periodic pass and the clean stamp
    /// wait on.
    needs_pass: AtomicBool,
    /// When the last whole writeback pass finished, in monotonic ms.
    last_full_ms: AtomicU64,
    /// When an operation last left something to write, in monotonic ms.
    last_busy_ms: AtomicU64,
    /// A pass finished with the image left dirty on the medium because the
    /// mount was not idle yet; a later visit stamps it clean.
    clean_owed: AtomicBool,
    /// The name and attribute caches `lookup` and `stat` answer from without
    /// the lock. Built at the slot's first attach and never freed, because a
    /// reader holds no lock that would keep it alive.
    dcache: OnceLock<Ext2Dcache>,
    /// The caches describe the attached image: set once attach has
    /// invalidated them behind the replay and orphan drain, cleared before
    /// detach lets the image go.
    dcache_live: AtomicBool,
    /// The pool flusher passes this instance by, so a test can hold the log
    /// and the open pass in the state it built.
    #[cfg(feature = "tests")]
    flusher_excluded: AtomicBool,
}

const WANT_COMMIT: u8 = 1;
const WANT_FULL: u8 = 2;

static FLUSH_STOP: KernelIoStop = KernelIoStop::new(
    "ext2-flush",
    lock_class!("EXT2_FLUSH_STOP.waiters", LOCK_LEVEL_RESOURCE),
);
static FLUSH_THREAD_STARTED: InitFlag = InitFlag::new();

/// Whole-pass writeback cadence — analog of Linux `dirty_writeback_centisecs`
/// — and so also how long an idle mount waits to be marked clean.
const FLUSH_INTERVAL_MS: u64 = 5_000;
/// Log commit cadence: how long an appended record may wait in the ring. jbd2
/// defaults to five seconds; a build machine that may be closed rudely at any
/// moment is better served by one.
const COMMIT_INTERVAL_MS: u64 = 1_000;
/// How long a mount must have had nothing to write before it is stamped clean.
const CLEAN_IDLE_MS: u64 = COMMIT_INTERVAL_MS;
/// Dirty blocks past which a mount with a log wakes the flusher early, as a
/// share of its cache — analog of `dirty_background_ratio` — floored so a
/// small cache still batches.
const FLUSH_EAGER_THRESHOLD: usize = 48;
const FLUSH_EAGER_SHARE: usize = 8;
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
            log_pending: AtomicUsize::new(0),
            wants: AtomicU8::new(0),
            needs_pass: AtomicBool::new(false),
            last_full_ms: AtomicU64::new(0),
            last_busy_ms: AtomicU64::new(0),
            clean_owed: AtomicBool::new(false),
            dcache: OnceLock::new(),
            dcache_live: AtomicBool::new(false),
            #[cfg(feature = "tests")]
            flusher_excluded: AtomicBool::new(false),
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

    #[track_caller]
    fn lock_cached(&self) -> WaitResult<CachedGuard<'_>> {
        let location = core::panic::Location::caller();
        let began = lock_profile::stamp();
        let guard = self.cached.lock()?;
        let acquired = lock_profile::stamp();
        let mut site = None;
        if began != 0 {
            let waited = acquired.saturating_sub(began);
            lock_profile::ACQUIRES.fetch_add(1, Ordering::Relaxed);
            lock_profile::WAIT_CYCLES.fetch_add(waited, Ordering::Relaxed);
            lock_profile::MAX_WAIT.fetch_max(waited, Ordering::Relaxed);
            site = lock_profile::SITES.site(location);
            if let Some(site) = site {
                site.note_acquire(waited);
            }
        }
        Ok(CachedGuard {
            guard,
            acquired,
            writeback: false,
            site,
        })
    }

    /// [`Self::lock_cached`] for writeback, whose hold `prof=on` reports apart.
    #[track_caller]
    fn lock_cached_for_writeback(&self) -> WaitResult<CachedGuard<'_>> {
        let mut guard = self.lock_cached()?;
        guard.writeback = true;
        Ok(guard)
    }

    #[track_caller]
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
        self.note_state(&cached.cache);
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
        let mut fs = Ext2Fs::new(
            &cached.device,
            &mut cached.cache,
            cached.superblock,
            cached.geom,
        );
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
        fs.set_gens(self.dcache.get().map(Ext2Dcache::gens));
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

    /// Publish what the flusher's predicate and timer read, waking it when
    /// this mount should not wait for the timer. Called under the FS lock;
    /// touches only atomics.
    fn note_state(&self, cache: &BlockCache) {
        let dirty = cache.dirty_count();
        self.dirty_pending.store(dirty, Ordering::Relaxed);
        self.log_pending
            .store(cache.journal_pending() as usize, Ordering::Relaxed);
        let needs_pass = dirty > 0 || cache.unbarriered_writes() > 0 || !cache.journal_is_empty();
        self.needs_pass.store(needs_pass, Ordering::Relaxed);
        if needs_pass {
            self.last_busy_ms.store(
                slopos_kernel_services::clock::uptime_ms(),
                Ordering::Relaxed,
            );
        }
        // Past the background threshold writeback starts early. With a log,
        // dirty blocks are already safe in it, so the threshold scales with
        // the cache; without one it stays small, writeback being all that
        // makes them durable.
        let threshold = if cache.journal().is_some() {
            (cache.capacity() / FLUSH_EAGER_SHARE).max(FLUSH_EAGER_THRESHOLD)
        } else {
            FLUSH_EAGER_THRESHOLD
        };
        let mut wants = 0u8;
        // The log filling is its own reason: draining it there is a bounded
        // pass, whereas at its low-water mark the next operation pays an
        // unbounded one under the lock.
        if dirty >= threshold || cache.journal_needs_drain() {
            wants |= WANT_FULL;
        }
        // Written before an operation finds the ring full and writes it
        // inline.
        if cache.journal_ring_filling() {
            wants |= WANT_COMMIT;
        }
        if wants != 0 && self.wants.fetch_or(wants, Ordering::Relaxed) & wants != wants {
            FLUSH_STOP.wake_one_for_work();
        }
    }

    /// What the flusher's park predicate reads for this slot.
    pub(crate) fn needs_flusher_now(&self) -> bool {
        self.wants.load(Ordering::Relaxed) != 0
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
        self.note_state(&cached.cache);
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
    #[track_caller]
    fn with_ext2<R>(&self, f: impl FnOnce(&mut Ext2Fs) -> Result<R, Ext2Error>) -> VfsResult<R>;
    /// The flusher kthread is what drains a filesystem's deferred frees.
    fn ext2_wake_for_detached(&self);
    fn ext2_sync(&self) -> VfsResult<()>;
    fn ext2_statfs(&self) -> VfsResult<FsStats>;
    fn ext2_sync_inode(&self, inode: InodeId, data_only: bool) -> VfsResult<()>;
    /// `None` while nothing is attached or the caches could not be built.
    fn ext2_dcache(&self) -> Option<&Ext2Dcache>;
}

impl Ext2VfsBackend for Ext2Mount {
    #[track_caller]
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

    fn ext2_dcache(&self) -> Option<&Ext2Dcache> {
        if !self.dcache_live.load(Ordering::Acquire) {
            return None;
        }
        self.dcache.get()
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
        let dcache = self.ext2_dcache();
        let key = dcache.and_then(|_| NameKey::new(parent, name));
        if let (Some(dcache), Some(key)) = (dcache, key.as_ref())
            && let Some(found) = dcache.lookup(key)
        {
            return found.map(InodeId::from).ok_or(VfsError::NotFound);
        }
        let mut stamp = None;
        let found = self.with_ext2(|fs| {
            // Under the lock, before the read: a mutation after this point
            // moves the counter past what the entry is stamped with.
            if let (Some(dcache), Some(key)) = (dcache, key.as_ref()) {
                stamp = Some(dcache.name_stamp(key));
            }
            match fs.lookup_child(parent as u32, name) {
                Ok(ino) => Ok(Some(ino.raw())),
                Err(Ext2Error::PathNotFound) => Ok(None),
                Err(e) => Err(e),
            }
        })?;
        if let (Some(dcache), Some(key), Some(stamp)) = (dcache, key.as_ref(), stamp) {
            dcache.insert_name(key, stamp, found);
        }
        found.map(InodeId::from).ok_or(VfsError::NotFound)
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let dcache = self.ext2_dcache();
        let ino = u32::try_from(inode).ok();
        if let (Some(dcache), Some(ino)) = (dcache, ino)
            && let Some(attr) = dcache.attr(ino)
        {
            return Ok(attr.to_stat(inode));
        }
        let mut stamp = None;
        let attr = self.with_ext2(|fs| {
            if let (Some(dcache), Some(ino)) = (dcache, ino) {
                stamp = Some(dcache.attr_stamp(ino));
            }
            fs.read_inode(inode as u32)
                .map(|record| InodeAttr::of(&record))
        })?;
        if let (Some(dcache), Some(ino), Some(stamp)) = (dcache, ino, stamp) {
            dcache.insert_attr(ino, stamp, attr);
        }
        Ok(attr.to_stat(inode))
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file(inode as u32, offset, buf))
    }

    fn read_pages(&self, inode: InodeId, offset: u64, pages: &mut [&mut [u8]]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.read_file_pages(inode as u32, offset, pages))
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        self.with_ext2(|fs| fs.write_file(inode as u32, offset, buf))
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        self.with_ext2(|fs| {
            let inode = match file_type {
                FileType::Directory => fs.create_directory(parent as u32, name)?,
                FileType::Regular => fs.create_file(parent as u32, name)?,
                FileType::Pipe => fs.create_fifo(parent as u32, name)?,
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

    fn set_times(
        &self,
        inode: InodeId,
        atime: Option<Timestamp>,
        mtime: Option<Timestamp>,
    ) -> VfsResult<()> {
        let ino = u32::try_from(inode).map_err(|_| VfsError::InvalidArgument)?;
        let on_disk = |t: Timestamp| InodeTime::new(t.secs, t.nanos);
        self.with_ext2(|fs| fs.set_times(ino, atime.map(on_disk), mtime.map(on_disk)))
    }

    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_mode(inode as u32, mode))
    }

    fn inode_flags(&self, inode: InodeId) -> VfsResult<u32> {
        self.with_ext2(|fs| fs.inode_flags(inode as u32))
    }

    fn set_inode_flags(
        &self,
        inode: InodeId,
        flags: u32,
        seal: Option<&Cap<'_, Seal>>,
    ) -> VfsResult<()> {
        self.with_ext2(|fs| fs.set_inode_flags(inode as u32, flags, seal.is_some()))
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
        self.dcache.call_once(Ext2Dcache::new);
        let mounted = self.mount_device(device, read_only);
        // After the replay and the orphan drain, which rewrite blocks without
        // telling the caches, and whatever the last image left in them.
        if let Some(dcache) = self.dcache.get() {
            dcache.invalidate_all();
        }
        match mounted {
            Ok(info) => {
                self.dcache_live.store(true, Ordering::Release);
                // Attaching stamped the image dirty, and a mount nothing
                // writes to runs no pass that would stamp it clean again.
                self.clean_owed.store(true, Ordering::Relaxed);
                Ok(info)
            }
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
        self.dcache_live.store(false, Ordering::Release);
        if !self.clear_cached() {
            self.dcache_live.store(true, Ordering::Release);
            return false;
        }
        if let Some(dcache) = self.dcache.get() {
            dcache.invalidate_all();
        }
        self.init.reset();
        self.read_only.store(false, Ordering::Release);
        self.remount_ro_pending.store(false, Ordering::Release);
        self.remount_ro_reported.reset();
        self.dirty_pending.store(0, Ordering::Relaxed);
        self.log_pending.store(0, Ordering::Relaxed);
        self.wants.store(0, Ordering::Relaxed);
        self.needs_pass.store(false, Ordering::Relaxed);
        self.last_busy_ms.store(0, Ordering::Relaxed);
        self.clean_owed.store(false, Ordering::Relaxed);
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
        // A batch the flusher still has in flight writes through this device;
        // the claim goes with the last reference, once it lands.
        if let Some(stale) = stale.as_ref() {
            let _ = stale.device.gate.wait_idle();
        }
        drop(stale);
        true
    }

    fn mount_device(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        requested_read_only: bool,
    ) -> VfsResult<Ext2MountInfo> {
        let (verity, read_only_reason) = self.open_device(device, requested_read_only)?;

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

    /// Everything a mount does before its journal. A volume needing recovery
    /// that this mount may not write is refused: its homes are stale until a
    /// replay.
    #[inline(never)]
    fn open_device(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        requested_read_only: bool,
    ) -> VfsResult<(VerityStatus, Option<ReadOnlyReason>)> {
        // Read off the raw device: a trailer is found only relative to the extent the
        // filesystem claims, and verity would not check a sub-block read anyway.
        let (superblock, geom) = Ext2Fs::mount_params(&*device).map_err(ext2_error_to_vfs)?;
        let (device, verity) = wrap_verified(device, &superblock, geom.block_size())?;
        log_verity_status(verity);
        // Asked before the mount stamps the volume in use: asked after, that
        // stamp would read as the previous mount's crash.
        let read_only_reason = if requested_read_only {
            Some(ReadOnlyReason::Requested)
        } else {
            Ext2Fs::mount_read_only_reason(&superblock, &*device)
        };
        let replay_barred = matches!(
            read_only_reason,
            Some(ReadOnlyReason::Requested | ReadOnlyReason::DeviceWriteProtected)
        );
        if replay_barred && superblock.has_journal() && superblock.needs_recovery() {
            klog_info!("ext2: refusing a read-only mount of a volume whose journal needs recovery");
            return Err(VfsError::ReadOnly);
        }
        let read_only = read_only_reason.is_some();
        self.read_only.store(read_only, Ordering::Release);
        self.install_cached(device, superblock, geom, read_only)?;
        Ok((verity, read_only_reason))
    }

    /// Attach the journal and answer the read-only reason that survives it,
    /// then stamp a writable mount in use. A replay lifts `NeedsRecovery`.
    #[inline(never)]
    fn attach_journal(&self, reason: Option<ReadOnlyReason>) -> Option<ReadOnlyReason> {
        let Ok(mut guard) = self.lock_cached() else {
            return reason;
        };
        let Some(cached) = guard.as_mut() else {
            return reason;
        };
        // A log is attachable only on a handle that may write, so the
        // recovery latch is lifted for the attempt and restored if it fails.
        let recoverable = reason == Some(ReadOnlyReason::NeedsRecovery);
        let writable = reason.is_none() || recoverable;
        if recoverable {
            cached.read_only = false;
        }
        let mut attach = Ok(None);
        if writable && cached.superblock.has_journal() {
            let _ = self.with_cached_fs(cached, |fs| {
                attach = fs.attach_journal();
                Ok(())
            });
        }
        let keep = match attach {
            Ok(Some(recovery)) => {
                if let Some(journal) = cached.cache.journal() {
                    klog_info!(
                        "ext2: journal attached — {} blocks at inode {}, replayed {} transactions ({} blocks)",
                        journal.capacity(),
                        journal.inode(),
                        recovery.transactions,
                        recovery.blocks,
                    );
                }
                if recoverable {
                    klog_info!("ext2: the journal is recovered, so the mount is writable again");
                }
                None
            }
            Ok(None) => reason,
            Err(e) => {
                klog_info!("ext2: journal unusable: {:?}", e);
                Some(match (reason, e) {
                    (Some(r), _) => r,
                    (None, AttachError::Fs(e)) if e.is_corruption() => {
                        ReadOnlyReason::ErrorsRemountRo
                    }
                    (None, _) => ReadOnlyReason::JournalUnusable,
                })
            }
        };
        if matches!(attach, Ok(Some(_))) {
            match Ext2Geometry::derive(&cached.superblock) {
                Ok(geom) => cached.geom = geom,
                Err(_) => cached.read_only = true,
            }
        }
        // Never *clears* a latch: `with_cached_fs` raises one of its own when
        // the attach finds the image or the device damaged.
        cached.read_only = keep.is_some() || cached.read_only;
        if !cached.read_only {
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
            klog_info!("ext2: mounting read-only — the device refuses writes")
        }
        ReadOnlyReason::UnsupportedFeature => klog_info!(
            "ext2: mounting read-only — the image declares a feature this kernel does not write"
        ),
        ReadOnlyReason::NeedsRecovery => klog_info!(
            "ext2: MOUNTING READ-ONLY — the volume went down in use and its journal \
             could not be replayed. Replay it on the host with `e2fsck -fy <image>`."
        ),
        ReadOnlyReason::JournalUnusable => klog_info!(
            "ext2: mounting read-only — the volume's journal is one this kernel neither \
             replays nor writes"
        ),
        ReadOnlyReason::ErrorsRecorded => klog_info!(
            "ext2: mounting read-only — the volume records errors; `e2fsck -fy` clears them"
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

/// The device behind its verity layer, once the filesystem's blocks are ones
/// the medium writes whole.
#[inline(never)]
fn wrap_verified(
    device: KBox<dyn BlockDevice + Send + Sync>,
    superblock: &Ext2Superblock,
    block_size: u32,
) -> VfsResult<(KBox<dyn BlockDevice + Send + Sync>, VerityStatus)> {
    // A block the medium cannot write whole is a read-modify-write of its
    // neighbours, and a torn one takes blocks outside the transaction with it.
    if (block_size as u64) < u64::from(device.logical_block_size()) {
        klog_info!(
            "ext2: refusing {}-byte blocks on a device of {}-byte logical blocks",
            block_size,
            device.logical_block_size()
        );
        return Err(VfsError::InvalidArgument);
    }
    let extent = FsExtent {
        block_size,
        blocks: superblock.blocks_count as u64,
    };
    // An image the last boot never marked clean may have blocks rewritten after
    // its bitmap was persisted, so its attestation is stale this boot.
    let trust = if superblock.is_clean() {
        AttestTrust::Persisted
    } else {
        AttestTrust::NoneThisBoot
    };
    crate::verity::build_verified_trusting(device, extent, trust).map_err(|e| {
        klog_info!("verity: refusing to mount — {:?}", e);
        verity_error_to_vfs(e)
    })
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
    /// Build the cache and publish `cached`, on a frame of its own so the cache
    /// temporaries do not share one with the verity parse.
    #[inline(never)]
    fn install_cached(
        &self,
        device: KBox<dyn BlockDevice + Send + Sync>,
        superblock: Ext2Superblock,
        geom: Ext2Geometry,
        read_only: bool,
    ) -> VfsResult<()> {
        // Zero on a device that cannot answer: a reserve of zero refuses
        // nothing, rather than failing a mount that would otherwise succeed.
        let reserved_blocks = Ext2Fs::read_block_reserve(&*device).unwrap_or(0);
        let device = GatedDevice {
            inner: KArc::try_new(device).map_err(|_| ext2_error_to_vfs(Ext2Error::OutOfMemory))?,
            gate: KArc::try_new(IoGate::new())
                .map_err(|_| ext2_error_to_vfs(Ext2Error::OutOfMemory))?,
        };
        let target_entries =
            mount_cache_entries(superblock.blocks_count as u64, superblock.blocks_per_group);
        let cache =
            BlockCache::new_boxed(geom.block_size(), target_entries).map_err(ext2_error_to_vfs)?;
        let mut guard = self.lock_cached().map_err(|_| VfsError::Interrupted)?;
        *guard = Some(CachedExt2 {
            device,
            superblock,
            geom,
            reserved_blocks,
            cache,
            superblock_dirty: false,
            read_only,
            writeback: Writeback::default(),
        });
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
            cached.geom.block_size(),
            cached.reserved_blocks,
            cached.read_only,
        ))
    }
}

/// The in-use stamp is what tells a later mount or fsck to recover; without it
/// a crash leaves an image that still claims to be clean. A no-op on a
/// read-only handle, so a write-protected device is never touched.
#[inline(never)]
fn stamp_not_clean(cached: &mut CachedExt2) {
    if cached.read_only {
        return;
    }
    let mut fs = Ext2Fs::new(
        &cached.device,
        &mut cached.cache,
        cached.superblock,
        cached.geom,
    );
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

/// Blocks one holder of the mount lock may write back before giving it back:
/// four `FLUSH_RUN`-sized requests, which the device takes two at a time, so a
/// path walk behind a pass waits for a bounded number of round trips and the
/// extra acquisitions are noise.
pub(crate) const WRITEBACK_CHUNK: usize = 128;

/// Blocks the flusher's commit copies out and writes per trip without the
/// mount lock: 512 KiB at 4 KiB blocks, inside one heap allocation's limit.
const WRITEBACK_BATCH: usize = 128;

/// Steps one caller may take before it gives up. A pass advances a phase or
/// writes a block on every step, so this bounds a livelock rather than the
/// work: reaching it means the device is failing every write. Enough steps
/// for a whole cache of dirty blocks, several times over.
const WRITEBACK_MAX_STEPS: usize = 4 * crate::ext2::cache::CACHE_ENTRIES_MAX / WRITEBACK_CHUNK;

/// The cache a mount of this geometry gets on this machine.
pub(crate) fn mount_cache_entries(volume_blocks: u64, blocks_per_group: u32) -> usize {
    let pages = slopos_mm::page_alloc::get_page_allocator_stats();
    let usable = u64::from(pages.free) + u64::from(pages.allocated);
    cache_entries_for(volume_blocks, blocks_per_group, usable)
}

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
        let mut guard = self
            .lock_cached_for_writeback()
            .map_err(|_| VfsError::Interrupted)?;
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
                        self.note_state(&cached.cache);
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
        self.note_state(&cached.cache);
        drop(guard);
        self.report_remount_ro_if_pending();
        result.map(|()| Progress::Stepped)
    }

    /// Keep the pool flusher off this instance. Set before `attach`, so no
    /// pass of its own is open when the test starts.
    #[cfg(feature = "tests")]
    pub(crate) fn exclude_flusher_for_test(&self, excluded: bool) {
        self.flusher_excluded.store(excluded, Ordering::Release);
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

    #[cfg(feature = "tests")]
    pub(crate) fn flusher_visit_for_test(&self) -> VfsResult<()> {
        self.flush_once(false)
    }

    #[cfg(feature = "tests")]
    pub(crate) fn superblock_clean_for_test(&self) -> Option<bool> {
        let guard = self.lock_cached().ok()?;
        guard.as_ref().map(|cached| cached.superblock.is_clean())
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
        if cached.read_only || cached.superblock.is_clean() {
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
        let mut fs = Ext2Fs::new(
            &cached.device,
            &mut cached.cache,
            cached.superblock,
            cached.geom,
        );
        if fs.mark_clean().is_ok() {
            cached.superblock = fs.superblock();
            cached.superblock_dirty = fs.superblock_dirty();
        }
    }

    /// Make the log durable without a whole pass: the dirty data its records
    /// may name goes home a chunk per lock hold, then one hold writes what
    /// the chunks left, the ring and the barriers. Metadata stays dirty for
    /// the next check point — the log already holds it.
    fn commit_log(&self) -> VfsResult<()> {
        if !self.init.is_set() {
            return Ok(());
        }
        let mut epoch = None;
        let mut scan = 0u32;
        let mut batch = DataBatch::default();
        for _ in 0..WRITEBACK_MAX_STEPS {
            let (device, gate, block_size, more) = {
                let mut guard = self
                    .lock_cached_for_writeback()
                    .map_err(|_| VfsError::Interrupted)?;
                let Some(cached) = guard.as_mut() else {
                    return Ok(());
                };
                let epoch = *epoch.get_or_insert(cached.cache.writeback_epoch());
                let progress = cached
                    .cache
                    .stage_data_batch(epoch, scan, WRITEBACK_BATCH, &mut batch)
                    .map_err(ext2_error_to_vfs)?;
                scan = progress.next;
                if batch.is_empty() {
                    break;
                }
                cached.device.gate.begin();
                (
                    cached.device.inner.clone(),
                    cached.device.gate.clone(),
                    cached.geom.block_size(),
                    progress.more,
                )
            };
            batch.write(&**device, block_size);
            // Before the gate opens: a teardown waiting on it expects its own
            // reference to be the device's last.
            drop(device);
            gate.end(batch.failed());
            if let Ok(mut guard) = self.lock_cached_for_writeback()
                && let Some(cached) = guard.as_mut()
            {
                cached.cache.finish_data_batch(&batch);
                cached.device.gate.failed.store(false, Ordering::Release);
                self.note_state(&cached.cache);
            }
            if !more {
                break;
            }
        }
        let mut guard = self
            .lock_cached_for_writeback()
            .map_err(|_| VfsError::Interrupted)?;
        let Some(cached) = guard.as_mut() else {
            return Ok(());
        };
        let result = cached
            .cache
            .sync_log(&cached.device)
            .map_err(ext2_error_to_vfs);
        self.note_state(&cached.cache);
        result
    }

    /// One flusher visit: a whole pass when one is asked for or due, else a
    /// log commit when records are waiting.
    fn flush_once(&self, stopping: bool) -> VfsResult<()> {
        let wants = self.wants.swap(0, Ordering::Relaxed);
        let now = slopos_kernel_services::clock::uptime_ms();
        let due =
            now.saturating_sub(self.last_full_ms.load(Ordering::Relaxed)) >= FLUSH_INTERVAL_MS;
        if stopping || wants & WANT_FULL != 0 || (due && self.needs_pass.load(Ordering::Relaxed)) {
            self.sync_fs()?;
            self.stamp_clean_if_idle(stopping);
            self.last_full_ms.store(now, Ordering::Relaxed);
            return Ok(());
        }
        if wants & WANT_COMMIT != 0 || self.log_pending.load(Ordering::Relaxed) > 0 {
            return self.commit_log();
        }
        if self.clean_owed.load(Ordering::Relaxed) {
            self.stamp_clean_if_idle(false);
        }
        Ok(())
    }

    /// The clean stamp is for an idle mount: a busy one would pay a
    /// superblock read, write and barrier to stamp it and the same again on
    /// its next operation to take the stamp back, every pass.
    fn stamp_clean_if_idle(&self, stopping: bool) {
        let now = slopos_kernel_services::clock::uptime_ms();
        let last_busy = self.last_busy_ms.load(Ordering::Relaxed);
        // Zero: nothing has been written since the attach.
        let idle = last_busy == 0 || now.saturating_sub(last_busy) >= CLEAN_IDLE_MS;
        if stopping || idle {
            self.clean_owed.store(false, Ordering::Relaxed);
            self.mark_filesystem_clean();
        } else {
            self.clean_owed.store(true, Ordering::Relaxed);
        }
    }
}

/// Must be called with interrupts still enabled — a block completion needs
/// them. Best-effort, and over every bound instance: a second
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
                    crate::vfs::init::ext2_pool_needs_flusher()
                        || orphan::releasable_count() > 0
                        || crate::filemap::pending_count() > 0
                },
                COMMIT_INTERVAL_MS,
            )
        };

        // A file mapping's writeback goes through the filesystem, so it cannot
        // run from the `release` that queued it.
        crate::filemap::drain_pending();

        // A whole pass on the stop path too: dirty blocks that never reach the
        // device are lost.
        let stopping = waited == KthreadWait::Stop;
        let mut failed = false;
        crate::vfs::init::ext2_pool_for_each_bound(&mut |mount| {
            #[cfg(feature = "tests")]
            if mount.flusher_excluded.load(Ordering::Acquire) {
                return;
            }
            // Before the sync, so the frees it performs go out in the same
            // pass rather than waiting a further tick. Takes the mount lock
            // itself, so it must not run under one.
            orphan::drain_releasable(mount);
            if mount.flush_once(stopping).is_err() {
                failed = true;
            }
        });
        backoff_ms = if failed {
            (backoff_ms * 2).clamp(FLUSH_BACKOFF_MIN_MS, FLUSH_BACKOFF_MAX_MS)
        } else {
            0
        };
        if stopping {
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
        Ext2Error::BadChecksum => VfsError::IoError,
        Ext2Error::Interrupted => VfsError::Interrupted,
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
