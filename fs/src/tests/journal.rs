//! The metadata redo log, and the bounded writeback pass it makes safe.
//!
//! The fixtures build the log the way the image builder does — a preallocated
//! sealed file at `/.journal` — so these tests exercise the same attach path a
//! boot takes rather than a synthetic one.
//!
//! Every mount goes through [`with_log`] and the probe device reports through
//! statics rather than a handle the bodies would carry: one `Ext2Fs` plus one
//! `BlockCache` already fills a 2 KiB frame.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use slopos_ostd::{KBox, KVec};
use slopos_testing::{TestResult, fail};

use super::{Ext2ImageSpec, FIX_FILE_BLOCK, build_ext2_image};
use crate::blockdev::{BlockDevice, BlockDeviceError, MemoryBlockDevice};
use crate::ext2::cache::{BlockCache, CACHE_ENTRIES_MIN};
use crate::ext2::journal::{Journal, JournalRecovery, LogExtent, MAX_LOG_SLOTS};
use crate::ext2::{Ext2Error, Ext2Fs, JOURNAL_PATH, ReadOnlyReason};

/// Comfortably above `journal::MIN_LOG_SLOTS`, and small enough to leave the
/// fixture room to allocate.
const LOG_BLOCKS: u32 = 48;
const IMAGE_BLOCKS: u32 = 512;
/// First block a synthetic log may take as a slot: past the fixture's reserves.
const SPARE_BASE: u32 = FIX_FILE_BLOCK + 1;
/// Held back at the top of the volume, so a replay is observable at a home
/// location no log slot writes to.
const HOME_BLOCKS: u32 = 4;
const HOME_FIRST: u32 = IMAGE_BLOCKS - HOME_BLOCKS;
/// Inode a synthetic log claims; only the log superblock's identity field
/// reads it, to recognise its own log on a later attach.
const SYNTH_INO: u32 = 12;
const PAYLOAD: &[u8] = b"a transaction the home locations never saw";

static PROBE_WRITES: AtomicUsize = AtomicUsize::new(0);
static PROBE_FLUSHES: AtomicUsize = AtomicUsize::new(0);
/// Offset of the first write since the counters were cleared, so a test can
/// assert on write *order* and not only on volume.
static PROBE_FIRST: AtomicU64 = AtomicU64::new(u64::MAX);
static PROBE_REFUSES: AtomicBool = AtomicBool::new(false);
static PROBE_INTERRUPTS: AtomicBool = AtomicBool::new(false);

/// Counts writes, and can be made to refuse them or to abandon reads part-way
/// through a test.
/// Refusing only once armed is what the retraction test needs: the log must
/// attach before the failure it is measuring.
struct ProbeDevice {
    inner: MemoryBlockDevice,
}

impl ProbeDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        PROBE_WRITES.store(0, Ordering::Relaxed);
        PROBE_FLUSHES.store(0, Ordering::Relaxed);
        PROBE_FIRST.store(u64::MAX, Ordering::Relaxed);
        PROBE_REFUSES.store(false, Ordering::Relaxed);
        PROBE_INTERRUPTS.store(false, Ordering::Relaxed);
        Self { inner }
    }
}

impl BlockDevice for ProbeDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        if PROBE_INTERRUPTS.load(Ordering::Relaxed) {
            return Err(BlockDeviceError::Interrupted);
        }
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        if PROBE_REFUSES.load(Ordering::Relaxed) {
            return Err(BlockDeviceError::InvalidBuffer);
        }
        PROBE_WRITES.fetch_add(1, Ordering::Relaxed);
        let _ =
            PROBE_FIRST.compare_exchange(u64::MAX, offset, Ordering::Relaxed, Ordering::Relaxed);
        self.inner.write_at(offset, buffer)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        PROBE_FLUSHES.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn plain_image() -> Option<MemoryBlockDevice> {
    build_ext2_image(Ext2ImageSpec {
        blocks: IMAGE_BLOCKS,
        inodes: 32,
        file_name: None,
        file_data: None,
        file_block: FIX_FILE_BLOCK,
    })
}

/// Mount `device` and hand the handle to `body`. Its own frame, as
/// `tests::with_mounted` is.
#[inline(never)]
fn with_log(
    device: &dyn BlockDevice,
    body: fn(&mut Ext2Fs<'_>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let (sb, bs, is) = Ext2Fs::mount_params(device).map_err(|_| "mount_params")?;
    let mut cache = BlockCache::new_boxed(bs, CACHE_ENTRIES_MIN).map_err(|_| "cache")?;
    let mut fs = Ext2Fs::new(device, &mut cache, sb, bs, is).map_err(|_| "mount")?;
    body(&mut fs)
}

/// A fixture carrying a log, built through the ordinary write path.
pub(super) fn journal_image() -> Option<MemoryBlockDevice> {
    let device = plain_image()?;
    match with_log(&device, install_log) {
        Ok(()) => Some(device),
        Err(_) => None,
    }
}

fn install_log(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let bs = fs.block_size();
    let ino = fs
        .create_file(2, &JOURNAL_PATH[1..])
        .map_err(|_| "create")?;
    let zeros = KVec::<u8>::zeroed(bs as usize).map_err(|_| "zeros")?;
    for index in 0..LOG_BLOCKS {
        fs.write_file(ino, u64::from(index) * u64::from(bs), zeros.as_slice())
            .map_err(|_| "preallocate")?;
    }
    fs.set_sealed(ino).map_err(|_| "seal")?;
    fs.sync().map_err(|_| "sync")?;
    fs.mark_clean().map_err(|_| "clean")
}

fn attach(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    match fs.attach_journal() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("the fixture's log was not attached"),
        Err(_) => Err("attach failed"),
    }
}

/// An operation the log recorded and nothing else did survives a mount that
/// never saw a clean unmount — the whole point of having a log.
pub fn test_ext2_journal_replays_an_unsynced_operation() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, log_one_operation) {
        return fail!("staging the transaction: {}", msg);
    }
    let Ok((sb, ..)) = Ext2Fs::mount_params(&device) else {
        return fail!("the staged image no longer parses");
    };
    if Ext2Fs::mount_read_only_reason(&sb, &device) != Some(ReadOnlyReason::NotCleanlyUnmounted) {
        return fail!("the image should read as never cleanly unmounted");
    }
    match with_log(&device, replay_and_check) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn log_one_operation(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    // What a boot does: stamp the image not-clean, then attach.
    fs.mark_dirty_on_disk().map_err(|_| "not-clean stamp")?;
    attach(fs)?;
    let ino = fs.create_file(2, b"logged.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    // Deliberately no sync. The commit record is on the medium and the home
    // locations are not, which is exactly what a power cut here leaves.
    Ok(())
}

fn replay_and_check(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let recovery = match fs.attach_journal() {
        Ok(Some(recovery)) => recovery,
        Ok(None) => return Err("no log on the remount"),
        Err(_) => return Err("attach failed on the remount"),
    };
    if !recovery.replayed() {
        return Err("the log held a committed transaction and replayed nothing");
    }
    let ino = fs
        .resolve_path(b"/logged.txt")
        .map_err(|_| "the replay did not restore the name")?;
    let mut buf = [0u8; 64];
    let read = fs.read_file(ino, 0, &mut buf).map_err(|_| "read back")?;
    if &buf[..read] != PAYLOAD {
        return Err("the replayed file holds the wrong bytes");
    }
    Ok(())
}

/// A commit that cannot reach the medium leaves neither the log nor the
/// filesystem carrying half an operation.
pub fn test_ext2_journal_retracts_a_failed_commit() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, retract_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn retract_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let head = fs.journal_head();
    PROBE_REFUSES.store(true, Ordering::Relaxed);
    let created = fs.create_file(2, b"doomed.txt").is_ok();
    PROBE_REFUSES.store(false, Ordering::Relaxed);
    if created {
        return Err("a create whose commit could not be written reported success");
    }
    if fs.journal_head() != head {
        return Err("the log kept the records of a retracted operation");
    }
    if fs.dirty_count() != 0 {
        return Err("the cache kept blocks a retracted operation dirtied");
    }
    if fs.resolve_path(b"/doomed.txt").is_ok() {
        return Err("the retracted name is still resolvable");
    }
    Ok(())
}

/// A read abandoned because its requester was killed says nothing about the
/// image: the operation fails and the mount stays writable.
///
/// The path that broke: every device error read as damage, so the guest
/// build's linker, killed with a read in flight, took `/devel` read-only.
pub fn test_ext2_killed_read_is_not_damage() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, killed_read_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn killed_read_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"killed.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    fs.sync().map_err(|_| "sync")?;
    fs.cache_drop_clean_for_test();
    let mut buf = [0u8; 64];
    PROBE_INTERRUPTS.store(true, Ordering::Relaxed);
    let read = fs.read_file(ino, 0, &mut buf);
    let read = fs.note_result(read);
    PROBE_INTERRUPTS.store(false, Ordering::Relaxed);
    if read != Err(Ext2Error::Interrupted) {
        return Err("the abandoned read did not answer Interrupted");
    }
    if fs.corruption_seen() {
        return Err("an abandoned read latched the mount as damaged");
    }
    let n = fs
        .read_file(ino, 0, &mut buf)
        .map_err(|_| "read after the kill")?;
    if &buf[..n] != PAYLOAD {
        return Err("the reread holds the wrong bytes");
    }
    Ok(())
}

/// The log's own file is kernel state: not readable, not removable.
pub fn test_ext2_journal_file_is_not_userland_data() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    match with_log(&device, guard_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn guard_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.resolve_path(JOURNAL_PATH).map_err(|_| "resolve")?;
    let mut buf = [0u8; 32];
    if fs.read_file(ino, 0, &mut buf).is_ok() {
        return Err("the log's blocks are readable through the VFS");
    }
    if fs.unlink_entry(2, &JOURNAL_PATH[1..]).is_ok() {
        return Err("the log's file can be unlinked from under the mount");
    }
    if fs.truncate_file(ino, 0).is_ok() {
        return Err("the log's file can be truncated from under the mount");
    }
    Ok(())
}

/// A writeback step writes at most its budget and a pass driven one write at a
/// time still finishes — which is what bounds the wait behind `sync(2)`.
pub fn test_ext2_writeback_step_respects_its_budget() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, budget_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn budget_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    for name in [&b"a.txt"[..], b"b.txt", b"c.txt", b"d.txt"] {
        let ino = fs.create_file(2, name).map_err(|_| "create")?;
        fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    }

    let mut pass = fs.begin_sync();
    let mut steps = 0usize;
    while !pass.is_done() {
        let before = PROBE_WRITES.load(Ordering::Relaxed);
        fs.sync_step(&mut pass, 1).map_err(|_| "step")?;
        if PROBE_WRITES.load(Ordering::Relaxed) - before > 1 {
            return Err("a step wrote more blocks than its budget");
        }
        steps += 1;
        if steps > 4096 {
            return Err("the pass never reached its end");
        }
    }
    if steps < 4 {
        return Err("the fixture produced too little to bound");
    }
    if fs.sync_pending() {
        return Err("the pass finished with work outstanding");
    }
    Ok(())
}

/// A pass writes what was dirty when it opened and nothing an operation
/// dirtied behind it. Without that, releasing the mount lock between steps
/// would publish metadata ahead of a later operation's data.
pub fn test_ext2_writeback_leaves_a_later_operation_alone() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    match with_log(&device, epoch_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn epoch_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"first.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    drive(fs, fs.begin_sync())?;
    if fs.sync_pending() {
        return Err("an undisturbed pass left work behind");
    }

    // The same again, with an operation landing after the pass opened.
    let ino = fs.create_file(2, b"second.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    let pass = fs.begin_sync();
    let late = fs.create_file(2, b"third.txt").map_err(|_| "create")?;
    fs.write_file(late, 0, PAYLOAD).map_err(|_| "write")?;
    drive(fs, pass)?;
    if !fs.sync_pending() {
        return Err("the pass published an operation that started after it");
    }
    fs.sync().map_err(|_| "second sync")?;
    if fs.sync_pending() {
        return Err("the following pass did not take the late operation");
    }
    Ok(())
}

fn drive(fs: &mut Ext2Fs<'_>, mut pass: crate::ext2::SyncPass) -> Result<(), &'static str> {
    let mut steps = 0usize;
    while !pass.is_done() {
        fs.sync_step(&mut pass, 8).map_err(|_| "step")?;
        steps += 1;
        if steps > 4096 {
            return Err("the pass never reached its end");
        }
    }
    Ok(())
}

/// A block whose only current copy is a log record reaches its home location.
///
/// The path that broke: a cache miss served from the log left the entry
/// *clean*, so the pass had nothing to write, the check point skipped it (the
/// cache said the home matched) and the reset dropped the only copy. Every
/// read still answered correctly until the mount went away.
pub fn test_ext2_journal_checkpoints_a_block_read_back_from_the_log() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, log_then_reread) {
        return fail!("{}", msg);
    }
    let Ok((sb, ..)) = Ext2Fs::mount_params(&device) else {
        return fail!("the image no longer parses");
    };
    if let Some(reason) = Ext2Fs::mount_read_only_reason(&sb, &device) {
        return fail!("the image should have been left clean, got {:?}", reason);
    }
    match with_log(&device, expect_survivor) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn log_then_reread(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"keep.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;

    // A failed operation drops every entry it touched, so the committed
    // contents of those blocks live only in the log.
    if fs.create_file(2, b"keep.txt").is_ok() {
        return Err("a duplicate create succeeded");
    }
    // ... and this read brings them back from it.
    let found = fs.resolve_path(b"/keep.txt").map_err(|_| "resolve")?;
    if found != ino {
        return Err("the re-read resolved to a different inode");
    }

    fs.sync().map_err(|_| "sync")?;
    if fs.sync_pending() {
        return Err("the sync left work behind");
    }
    fs.mark_clean().map_err(|_| "clean stamp")
}

fn expect_survivor(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let recovery = match fs.attach_journal() {
        Ok(Some(recovery)) => recovery,
        Ok(None) => return Err("no log on the remount"),
        Err(_) => return Err("attach failed on the remount"),
    };
    if recovery.replayed() {
        return Err("the log still held a transaction a clean unmount should have drained");
    }
    let ino = fs
        .resolve_path(b"/keep.txt")
        .map_err(|_| "the check point never reached the home locations")?;
    let mut buf = [0u8; 64];
    let read = fs.read_file(ino, 0, &mut buf).map_err(|_| "read back")?;
    if &buf[..read] != PAYLOAD {
        return Err("the survivor holds the wrong bytes");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_replays_an_unsynced_operation,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_retracts_a_failed_commit,
    suite = fs
);
slopos_testing::stest!(name = test_ext2_killed_read_is_not_damage, suite = fs);
slopos_testing::stest!(
    name = test_ext2_journal_checkpoints_a_block_read_back_from_the_log,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_file_is_not_userland_data,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_writeback_step_respects_its_budget,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_writeback_leaves_a_later_operation_alone,
    suite = fs
);

/// An `unlink` of a file something still holds open must publish nothing to a
/// home location before its transaction commits.
///
/// The path that broke: the orphan list got its ordering by flushing the
/// member's record *home* mid-operation, which under a log publishes
/// uncommitted metadata that the rollback then drops rather than repairs — so
/// a later failure left a live name pointing at `links_count == 0`.
///
/// Observed as the barrier count: a mid-operation home write barriers behind
/// itself, so the old path issued two where the log path issues one.
pub fn test_ext2_journal_orphan_publishes_nothing_before_the_commit() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, orphan_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn orphan_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"orphan.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    fs.sync().map_err(|_| "sync")?;

    PROBE_FLUSHES.store(0, Ordering::Relaxed);
    let detached = fs
        .detach_entry(2, b"orphan.txt")
        .map_err(|_| "detach")?
        .ok_or("the last link did not orphan the inode")?;
    if detached != ino {
        return Err("a different inode was orphaned");
    }
    let barriers = PROBE_FLUSHES.load(Ordering::Relaxed);
    if barriers != 1 {
        return Err("the orphan path barriered more than once, so it published home early");
    }
    // And the list is still usable: the deferred head write landed.
    fs.release_orphan(detached).map_err(|_| "release")?;
    if fs.resolve_path(b"/orphan.txt").is_ok() {
        return Err("the detached name is still resolvable");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_orphan_publishes_nothing_before_the_commit,
    suite = fs
);

/// Unthreading an orphan publishes the list head *before* the commit that
/// destroys the member's chain link.
///
/// The two directions need opposite orderings. A push wants the member's
/// `i_dtime` recoverable before the head names it, so it defers; a *removal*
/// shares a transaction with the free that overwrites that `i_dtime`, so a
/// deferred head would name an inode whose chain link is already gone and the
/// next mount's drain would discard every orphan behind it.
///
/// Observed as the offset of the first device write: the head is at byte 1024,
/// and a deferred one would put a log block first.
pub fn test_ext2_journal_orphan_removal_publishes_the_head_first() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    match with_log(&device, orphan_removal_body) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

fn orphan_removal_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let ino = fs.create_file(2, b"unthread.txt").map_err(|_| "create")?;
    fs.write_file(ino, 0, PAYLOAD).map_err(|_| "write")?;
    let detached = fs
        .detach_entry(2, b"unthread.txt")
        .map_err(|_| "detach")?
        .ok_or("the last link did not orphan the inode")?;
    fs.sync().map_err(|_| "sync")?;

    PROBE_FIRST.store(u64::MAX, Ordering::Relaxed);
    fs.release_orphan(detached).map_err(|_| "release")?;
    if PROBE_FIRST.load(Ordering::Relaxed) != 1024 {
        return Err("the removal logged before it published the head");
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_orphan_removal_publishes_the_head_first,
    suite = fs
);

/// A superblock field as the medium holds it, not as a handle remembers it.
fn on_disk_u16(device: &dyn BlockDevice, field_offset: u64) -> u16 {
    let mut buf = [0u8; 2];
    if device.read_at(1024 + field_offset, &mut buf).is_err() {
        return u16::MAX;
    }
    u16::from_le_bytes(buf)
}

/// An image marked clean while still mounted must be re-stamped dirty before
/// the next mutation reaches the medium, or a crash leaves a superblock
/// claiming a consistency its blocks do not have and nothing tells `e2fsck` to
/// look. Observed as the offset of the first device write — 1024 is the
/// superblock, and without the thaw it is this operation's log block. An idle
/// window is not a mount, so the re-stamp must not bill one.
pub fn test_ext2_clean_stamp_thaws_before_the_next_write() -> TestResult {
    use crate::ext2::ondisk::{EXT2_ERROR_FS, EXT2_VALID_FS};

    // `journal_image` ends in `mark_clean`: a clean image carrying a log, which
    // is what an idle boot leaves behind.
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ProbeDevice::new(image);
    if on_disk_u16(&device, 58) != EXT2_VALID_FS {
        return fail!("the fixture is not clean, so there is nothing to thaw");
    }
    let mounts = on_disk_u16(&device, 52);

    if let Err(msg) = with_log(&device, thaw_body) {
        return fail!("{}", msg);
    }
    if PROBE_FIRST.load(Ordering::Relaxed) != 1024 {
        return fail!("the mutation wrote before it stamped the image dirty");
    }
    if on_disk_u16(&device, 58) != EXT2_ERROR_FS {
        return fail!("a mutated image is still marked clean on the medium");
    }
    if on_disk_u16(&device, 52) != mounts {
        return fail!("the thaw billed a second mount");
    }
    TestResult::Pass
}

fn thaw_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    // The attach writes the log's own superblock, so measure after it.
    attach(fs)?;
    PROBE_FIRST.store(u64::MAX, Ordering::Relaxed);
    fs.create_file(2, b"thaw.txt")
        .map(|_| ())
        .map_err(|_| "create")
}

slopos_testing::stest!(
    name = test_ext2_clean_stamp_thaws_before_the_next_write,
    suite = fs
);

/// Home block of synthetic log slot `i`. Slot 0 keeps a block to itself — a
/// payload must not land on the log's own superblock — and past the spare
/// range the list repeats, which no test here reads back through.
fn synth_slot_block(i: u32) -> u32 {
    let pool = HOME_FIRST - SPARE_BASE - 1;
    match i {
        0 => SPARE_BASE,
        _ => SPARE_BASE + 1 + (i - 1) % pool,
    }
}

/// A log built directly rather than through `/.journal`: the tests that use it
/// want more slots than a fixture image has blocks, and measure the slot count
/// rather than any payload.
#[inline(never)]
fn synthetic_log(
    device: &dyn BlockDevice,
    count: u32,
) -> Result<(KBox<Journal>, JournalRecovery), &'static str> {
    let bs = image_block_size(device)?;
    let mut slots = KVec::with_capacity(count as usize).map_err(|_| "slots")?;
    for i in 0..count {
        slots.push(synth_slot_block(i)).map_err(|_| "slots")?;
    }
    let extent = LogExtent {
        first_data_block: 1,
        blocks_count: IMAGE_BLOCKS,
    };
    Journal::attach(slots, bs, SYNTH_INO, extent, device).map_err(|_| "attach")
}

/// Its own frame: `mount_params` hands back a whole superblock.
#[inline(never)]
fn image_block_size(device: &dyn BlockDevice) -> Result<u32, &'static str> {
    Ext2Fs::mount_params(device)
        .map(|(_, bs, _)| bs)
        .map_err(|_| "mount_params")
}

/// A block logged several times resolves to its newest record, and to nothing
/// once revoked. The scan this index replaced answered by walking every slot.
pub fn test_ext2_journal_index_answers_the_newest_record() -> TestResult {
    let Some(device) = plain_image() else {
        return TestResult::Skipped;
    };
    match newest_record_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

#[inline(never)]
fn newest_record_body(device: &dyn BlockDevice) -> Result<(), &'static str> {
    let (mut log, _) = synthetic_log(device, 64)?;
    let target = HOME_FIRST;
    let mut newest = 0u32;
    let mut oldest = 0u32;
    for round in 0..3 {
        newest = log
            .write_record(&[target], device, &mut |_| Some(PAYLOAD))
            .map_err(|_| "write_record")?;
        if round == 0 {
            oldest = newest;
        }
    }
    if newest == oldest {
        return Err("the fixture logged one block three times into one slot");
    }
    if log.resident_slot(target) != Some(newest) {
        return Err("the index did not answer the newest of a block's records");
    }
    log.note_revoke(target, device).map_err(|_| "note_revoke")?;
    log.flush_revokes(device).map_err(|_| "flush_revokes")?;
    if log.resident_slot(target).is_some() {
        return Err("a revoked block still resolves to a log record");
    }
    Ok(())
}

/// An aborted operation leaves the index as the committed log left it: no
/// mapping for what it logged, and the mapping its revoke cleared back in place.
pub fn test_ext2_journal_index_rewinds_with_an_aborted_operation() -> TestResult {
    let Some(device) = plain_image() else {
        return TestResult::Skipped;
    };
    match index_rewind_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

#[inline(never)]
fn index_rewind_body(device: &dyn BlockDevice) -> Result<(), &'static str> {
    let (mut log, _) = synthetic_log(device, 64)?;
    let kept = HOME_FIRST;
    log.begin_op();
    // Logged twice, so the restore has to put two mappings back in the order
    // that leaves the newer one answering.
    let mut kept_slot = 0u32;
    for _ in 0..2 {
        kept_slot = log
            .write_record(&[kept], device, &mut |_| Some(PAYLOAD))
            .map_err(|_| "write_record")?;
    }
    log.write_commit(device).map_err(|_| "write_commit")?;

    let head = log.head();
    let aborted = [HOME_FIRST + 1, HOME_FIRST + 2, HOME_FIRST + 3];
    log.begin_op();
    // Revoked from inside the operation that aborts, so the rewind has to put
    // a committed mapping back as well as drop its own.
    log.note_revoke(kept, device).map_err(|_| "note_revoke")?;
    log.flush_revokes(device).map_err(|_| "flush_revokes")?;
    log.write_record(&aborted, device, &mut |_| Some(PAYLOAD))
        .map_err(|_| "write_record")?;
    log.abort_op();

    if log.head() != head {
        return Err("the rewind left the head past where the operation began");
    }
    for block in aborted {
        if log.resident_slot(block).is_some() {
            return Err("a block an aborted operation logged still resolves to a record");
        }
    }
    if log.resident_slot(kept) != Some(kept_slot) {
        return Err("the rewind did not restore the newest mapping the operation revoked");
    }
    Ok(())
}

/// A `/.journal` with more slots than the kernel can index is used up to the
/// cap, not refused, and the front of it still replays.
pub fn test_ext2_journal_uses_the_front_of_an_oversized_log() -> TestResult {
    let Some(device) = plain_image() else {
        return TestResult::Skipped;
    };
    match oversized_log_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

#[inline(never)]
fn oversized_log_body(device: &dyn BlockDevice) -> Result<(), &'static str> {
    let bs = image_block_size(device)?;
    let mut payload = KVec::<u8>::zeroed(bs as usize).map_err(|_| "payload")?;
    payload.as_mut_slice()[..PAYLOAD.len()].copy_from_slice(PAYLOAD);
    let target = HOME_FIRST;

    let (mut log, _) = synthetic_log(device, MAX_LOG_SLOTS + 8)?;
    if log.capacity() != MAX_LOG_SLOTS - 1 {
        return Err("a log past the cap was not truncated to it");
    }
    log.write_record(&[target], device, &mut |_| Some(payload.as_slice()))
        .map_err(|_| "write_record")?;
    log.write_commit(device).map_err(|_| "write_commit")?;
    // Dropped without a check point, so the next attach has to find the
    // transaction and apply it — and dropped first, because two capped logs'
    // slot arrays at once are not what this measures.
    drop(log);

    let (log, recovery) = synthetic_log(device, MAX_LOG_SLOTS + 8)?;
    if recovery.transactions != 1 || recovery.blocks != 1 {
        return Err("the capped log did not replay its committed transaction");
    }
    if log.capacity() != MAX_LOG_SLOTS - 1 {
        return Err("the replaying attach disagreed with the cap");
    }
    drop(log);
    payload.as_mut_slice().fill(0);
    device
        .read_at(target as u64 * bs as u64, payload.as_mut_slice())
        .map_err(|_| "read")?;
    if &payload.as_slice()[..PAYLOAD.len()] != PAYLOAD {
        return Err("the replay did not reach the home location");
    }
    Ok(())
}

/// Blocks one transaction logs: more than the 256 the old flat low-water mark
/// left however large the log was, which is what a batched write's metadata needs.
const BIG_TXN_BLOCKS: u32 = 400;

/// The room the log promises an operation scales with the log, so a
/// transaction bigger than the old flat mark commits instead of failing
/// `BlockCache::commit_op`'s slot check with `NoSpace`.
pub fn test_ext2_journal_headroom_admits_a_large_transaction() -> TestResult {
    let Some(device) = plain_image() else {
        return TestResult::Skipped;
    };
    match large_transaction_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

#[inline(never)]
fn large_transaction_body(device: &dyn BlockDevice) -> Result<(), &'static str> {
    let (mut log, _) = synthetic_log(device, 2048)?;
    let per_record = log.max_entries() as u32;
    let want = BIG_TXN_BLOCKS + BIG_TXN_BLOCKS.div_ceil(per_record) + 1;
    if want <= 256 {
        return Err("the fixture's transaction is no bigger than the old flat mark");
    }
    // Whenever the log claims headroom it must hold a whole transaction:
    // `Ext2Fs::transaction` admits an operation on exactly that claim.
    while log.has_headroom() {
        if log.free_slots() < want {
            return Err("the log claimed headroom for a transaction it has no room for");
        }
        log.spill(HOME_FIRST, PAYLOAD, device)
            .map_err(|_| "spill")?;
    }

    log.reset(device).map_err(|_| "reset")?;
    let mut targets = KVec::<u32>::with_capacity(per_record as usize).map_err(|_| "targets")?;
    let mut done = 0u32;
    while done < BIG_TXN_BLOCKS {
        let take = (BIG_TXN_BLOCKS - done).min(per_record);
        targets.clear();
        for k in 0..take {
            targets.push(1 + done + k).map_err(|_| "targets")?;
        }
        log.write_record(targets.as_slice(), device, &mut |_| Some(PAYLOAD))
            .map_err(|_| "a transaction the log promised room for was refused")?;
        done += take;
    }
    log.write_commit(device)
        .map_err(|_| "the commit record of a transaction the log promised room for was refused")?;
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_index_answers_the_newest_record,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_index_rewinds_with_an_aborted_operation,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_uses_the_front_of_an_oversized_log,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_headroom_admits_a_large_transaction,
    suite = fs
);

/// Blocks the extent test writes in one call: past `DATA_LOG_LIMIT`, so the
/// data is written home and barriered rather than logged, and past the cache's
/// flush-run bound, so a coalesced phase is still several requests.
const EXTENT_BLOCKS: u32 = 64;

/// Byte *i* of the extent, so a misplaced segment mismatches rather than
/// reading as the zeros a sparse read would also give.
fn extent_byte(i: usize) -> u8 {
    (i % 251) as u8
}

static EXT_WRITES: AtomicUsize = AtomicUsize::new(0);
static EXT_BYTES: AtomicUsize = AtomicUsize::new(0);
static EXT_BARRIERS: AtomicUsize = AtomicUsize::new(0);
/// Requests and bytes taken *before* the first barrier: the ordered-writeback
/// data phase and nothing else.
static EXT_PRE_WRITES: AtomicUsize = AtomicUsize::new(0);
static EXT_PRE_BYTES: AtomicUsize = AtomicUsize::new(0);
/// The measurement, lifted out of a body whose only error channel is a
/// `&'static str` the assertion needs numbers alongside.
static EXT_SEEN_WRITES: AtomicUsize = AtomicUsize::new(0);
static EXT_SEEN_BYTES: AtomicUsize = AtomicUsize::new(0);
static EXT_SEEN_BARRIERS: AtomicUsize = AtomicUsize::new(0);
static EXT_SEEN_BLOCK_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Records the *length* of every write request, so a gathered write counts
/// once and carries its whole run: requests measured apart from bytes.
struct ExtentDevice {
    inner: MemoryBlockDevice,
}

impl ExtentDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        Self::rearm();
        Self { inner }
    }

    fn rearm() {
        EXT_WRITES.store(0, Ordering::Relaxed);
        EXT_BYTES.store(0, Ordering::Relaxed);
        EXT_BARRIERS.store(0, Ordering::Relaxed);
        EXT_PRE_WRITES.store(0, Ordering::Relaxed);
        EXT_PRE_BYTES.store(0, Ordering::Relaxed);
    }

    fn note(len: usize) {
        EXT_WRITES.fetch_add(1, Ordering::Relaxed);
        EXT_BYTES.fetch_add(len, Ordering::Relaxed);
        if EXT_BARRIERS.load(Ordering::Relaxed) == 0 {
            EXT_PRE_WRITES.fetch_add(1, Ordering::Relaxed);
            EXT_PRE_BYTES.fetch_add(len, Ordering::Relaxed);
        }
    }
}

impl BlockDevice for ExtentDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        Self::note(buffer.len());
        self.inner.write_at(offset, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        Self::note(segs.iter().map(|seg| seg.len()).sum());
        self.inner.write_vectored(offset, segs)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }

    fn flush(&self) -> Result<(), BlockDeviceError> {
        EXT_BARRIERS.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// A multi-block extent costs requests in proportion to the bytes it moves,
/// not the blocks it touches, and the ordered-writeback phases stay separate
/// while it does. The slow path issued one `write_at` per block, so a
/// 64-block extent was 64 round trips against a device that takes a run.
pub fn test_ext2_journal_write_extent_coalesces_its_requests() -> TestResult {
    let Some(image) = journal_image() else {
        return TestResult::Skipped;
    };
    let device = ExtentDevice::new(image);
    if let Err(msg) = with_log(&device, extent_body) {
        return fail!("{}", msg);
    }
    // A fresh mount, so the bytes come off the medium rather than the cache
    // that wrote them: only here is a misplaced segment visible.
    if let Err(msg) = with_log(&device, extent_verify) {
        return fail!("{}", msg);
    }

    let bs = EXT_SEEN_BLOCK_SIZE.load(Ordering::Relaxed).max(1);
    let requests = EXT_SEEN_WRITES.load(Ordering::Relaxed);
    let bytes = EXT_SEEN_BYTES.load(Ordering::Relaxed);
    let blocks = bytes / bs;
    let extent = bs * EXTENT_BLOCKS as usize;

    if EXT_SEEN_BARRIERS.load(Ordering::Relaxed) == 0 {
        return fail!("the data phase of an ordered write issued no barrier");
    }
    // The pre-barrier phase carried the extent and nothing else: extra bytes
    // are a metadata block merged into a data run, missing ones a data block
    // deferred past the barrier.
    if bytes != extent {
        return fail!(
            "the data phase wrote {} bytes before the barrier, not the extent's {}",
            bytes,
            extent
        );
    }
    if requests * 8 > blocks {
        return fail!(
            "the extent went home in {} requests for {} blocks; a coalesced phase needs far fewer",
            requests,
            blocks
        );
    }
    TestResult::Pass
}

fn extent_body(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    let bs = fs.block_size() as usize;
    let len = bs * EXTENT_BLOCKS as usize;
    let mut buffer = KVec::<u8>::zeroed(len).map_err(|_| "buffer")?;
    for (i, byte) in buffer.as_mut_slice().iter_mut().enumerate() {
        *byte = extent_byte(i);
    }
    // The create is what stamps the image dirty and spends the log's first
    // slots, so counting after it leaves only the write's own two phases.
    let ino = fs.create_file(2, b"extent.bin").map_err(|_| "create")?;
    ExtentDevice::rearm();
    let written = fs
        .write_file(ino, 0, buffer.as_slice())
        .map_err(|_| "write")?;
    EXT_SEEN_WRITES.store(EXT_PRE_WRITES.load(Ordering::Relaxed), Ordering::Relaxed);
    EXT_SEEN_BYTES.store(EXT_PRE_BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
    EXT_SEEN_BARRIERS.store(EXT_BARRIERS.load(Ordering::Relaxed), Ordering::Relaxed);
    EXT_SEEN_BLOCK_SIZE.store(bs, Ordering::Relaxed);
    if written != len {
        return Err("the extent write was short");
    }
    fs.sync().map_err(|_| "sync")?;
    fs.mark_clean().map_err(|_| "clean")
}

fn extent_verify(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.resolve_path(b"/extent.bin").map_err(|_| "resolve")?;
    let bs = fs.block_size() as usize;
    let len = bs * EXTENT_BLOCKS as usize;
    let mut buffer = KVec::<u8>::zeroed(len).map_err(|_| "buffer")?;
    if fs
        .read_file(ino, 0, buffer.as_mut_slice())
        .map_err(|_| "read")?
        != len
    {
        return Err("the extent read back short");
    }
    for (i, byte) in buffer.as_slice().iter().enumerate() {
        if *byte != extent_byte(i) {
            return Err("a gathered write put a block at the wrong device offset");
        }
    }
    Ok(())
}

slopos_testing::stest!(
    name = test_ext2_journal_write_extent_coalesces_its_requests,
    suite = fs
);

/// Logged by a small write, then written home by one too large to log: the home
/// copy is newer, and a cache miss and a check point must both answer it.
///
/// The path that broke: the log kept its mapping, so a miss read the logged
/// copy back and the check point wrote it home. In the guest build every rlib
/// member began with the zeros its archive header's small write had logged.
pub fn test_ext2_journal_home_write_outranks_an_older_logged_copy() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, relog_then_reread) {
        return fail!("{}", msg);
    }
    match with_log(&device, relog_verify) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("after the check point: {}", msg),
    }
}

/// The same two writes, committed and never check-pointed: the replay must not
/// put the logged copy back over the home write that superseded it.
pub fn test_ext2_journal_replay_keeps_a_home_write_over_an_older_logged_copy() -> TestResult {
    let Some(device) = journal_image() else {
        return TestResult::Skipped;
    };
    if let Err(msg) = with_log(&device, relog_unsynced) {
        return fail!("{}", msg);
    }
    match with_log(&device, relog_replay) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("after the replay: {}", msg),
    }
}

const RELOG_SMALL: &[u8] = b"a small write the log takes whole";

fn relog_writes(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.create_file(2, b"relog.bin").map_err(|_| "create")?;
    fs.write_file(ino, 0, RELOG_SMALL)
        .map_err(|_| "small write")?;
    let len = fs.block_size() as usize * EXTENT_BLOCKS as usize;
    let mut buffer = KVec::<u8>::zeroed(len).map_err(|_| "buffer")?;
    for (i, byte) in buffer.as_mut_slice().iter_mut().enumerate() {
        *byte = extent_byte(i);
    }
    if fs
        .write_file(ino, 0, buffer.as_slice())
        .map_err(|_| "large write")?
        != len
    {
        return Err("the large write was short");
    }
    Ok(())
}

fn relog_matches(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    let ino = fs.resolve_path(b"/relog.bin").map_err(|_| "resolve")?;
    let len = fs.block_size() as usize * EXTENT_BLOCKS as usize;
    let mut buffer = KVec::<u8>::zeroed(len).map_err(|_| "buffer")?;
    if fs
        .read_file(ino, 0, buffer.as_mut_slice())
        .map_err(|_| "read")?
        != len
    {
        return Err("the file read back short");
    }
    for (i, byte) in buffer.as_slice().iter().enumerate() {
        if *byte != extent_byte(i) {
            return Err("the file reads back the logged copy, not the home write");
        }
    }
    Ok(())
}

fn relog_then_reread(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    attach(fs)?;
    relog_writes(fs)?;
    fs.cache_drop_clean_for_test();
    relog_matches(fs)?;
    fs.cache_drop_clean_for_test();
    fs.sync().map_err(|_| "sync")?;
    fs.mark_clean().map_err(|_| "clean")
}

fn relog_verify(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    relog_matches(fs)
}

fn relog_unsynced(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    fs.mark_dirty_on_disk().map_err(|_| "not-clean stamp")?;
    attach(fs)?;
    relog_writes(fs)
}

fn relog_replay(fs: &mut Ext2Fs<'_>) -> Result<(), &'static str> {
    match fs.attach_journal() {
        Ok(Some(recovery)) if recovery.replayed() => relog_matches(fs),
        Ok(_) => Err("the log held committed transactions and replayed none"),
        Err(_) => Err("attach failed on the remount"),
    }
}

slopos_testing::stest!(
    name = test_ext2_journal_home_write_outranks_an_older_logged_copy,
    suite = fs
);
slopos_testing::stest!(
    name = test_ext2_journal_replay_keeps_a_home_write_over_an_older_logged_copy,
    suite = fs
);

/// Where a record header carries its entry count, and one past its end.
const REC_COUNT_OFF: usize = 12;
const REC_COUNT_END: usize = REC_COUNT_OFF + 4;

/// Answers one record header differently on its second read — the precondition
/// `build_disposition`'s clamp names: the scan and the disposition each read
/// the header off the medium, and nothing makes a device answer alike twice.
struct TamperDevice {
    inner: MemoryBlockDevice,
    /// Offset whose header is rewritten, or `u64::MAX` for none.
    target: AtomicU64,
    reads: AtomicUsize,
    /// Entries the rewritten header claims.
    claim: AtomicU32,
}

impl TamperDevice {
    fn new(inner: MemoryBlockDevice) -> Self {
        Self {
            inner,
            target: AtomicU64::new(u64::MAX),
            reads: AtomicUsize::new(0),
            claim: AtomicU32::new(0),
        }
    }

    /// Rewrite the entry count of every read of `offset` after the first, so
    /// the scan sees the honest record and the disposition sees `claim`.
    fn arm(&self, offset: u64, claim: u32) {
        self.reads.store(0, Ordering::Relaxed);
        self.claim.store(claim, Ordering::Relaxed);
        self.target.store(offset, Ordering::Relaxed);
    }

    fn disarm(&self) {
        self.target.store(u64::MAX, Ordering::Relaxed);
    }
}

impl BlockDevice for TamperDevice {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), BlockDeviceError> {
        self.inner.read_at(offset, buffer)?;
        if offset == self.target.load(Ordering::Relaxed)
            && buffer.len() >= REC_COUNT_END
            && self.reads.fetch_add(1, Ordering::Relaxed) > 0
        {
            let claim = self.claim.load(Ordering::Relaxed);
            buffer[REC_COUNT_OFF..REC_COUNT_END].copy_from_slice(&claim.to_le_bytes());
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buffer: &[u8]) -> Result<(), BlockDeviceError> {
        self.inner.write_at(offset, buffer)
    }

    fn write_vectored(&self, offset: u64, segs: &[&[u8]]) -> Result<(), BlockDeviceError> {
        self.inner.write_vectored(offset, segs)
    }

    fn capacity(&self) -> u64 {
        self.inner.capacity()
    }
}

/// Home blocks the clamp fixture's two transactions target: past every slot
/// the synthetic log takes, so a replay's own writes never land on the log.
const CLAMP_WIDE_HOME: u32 = HOME_FIRST;
const CLAMP_TAIL_HOME: u32 = HOME_FIRST + 1;
/// Slots the fixture needs: a full-width `DATA` record and its commit, then a
/// one-block transaction and its — past `2 + max_entries`, which is the only
/// region in which the clamped and unclamped cursors differ.
const CLAMP_SLOTS: u32 = 300;
/// What the tampered header claims on top of the maximum.
const CLAMP_OVERCLAIM: u32 = 7;

/// A record header claiming more entries than a block holds must read as
/// claiming exactly the maximum, for the slot cursor as much as for the
/// indexing: `build_disposition` clamped the index but advanced its cursor by
/// the unclamped value, stepping the second pass past the tail transaction.
pub fn test_ext2_journal_record_count_is_clamped_for_the_cursor_too() -> TestResult {
    let Some(image) = plain_image() else {
        return TestResult::Skipped;
    };
    let device = TamperDevice::new(image);
    match clamped_cursor_body(&device) {
        Ok(()) => TestResult::Pass,
        Err(msg) => fail!("{}", msg),
    }
}

#[inline(never)]
fn clamped_cursor_body(device: &TamperDevice) -> Result<(), &'static str> {
    let bs = image_block_size(device)?;
    let honest = clamp_replay(device, bs, None)?;
    let tampered = clamp_replay(device, bs, Some(CLAMP_OVERCLAIM))?;
    if honest != tampered {
        return Err("an overclaimed entry count replayed to a different state");
    }
    Ok(())
}

/// Lay the two-transaction log down, replay it from a fresh attach, and answer
/// what the replay did. `overclaim` inflates the header's count on the
/// disposition's read of it, which is the second.
#[inline(never)]
fn clamp_replay(
    device: &TamperDevice,
    bs: u32,
    overclaim: Option<u32>,
) -> Result<(u32, u32), &'static str> {
    let mut payload = KVec::<u8>::zeroed(bs as usize).map_err(|_| "payload")?;
    payload.as_mut_slice()[..PAYLOAD.len()].copy_from_slice(PAYLOAD);
    // Both home locations start empty, so a transaction that never reached
    // one shows up as zeros rather than as the previous round's payload.
    for home in [CLAMP_WIDE_HOME, CLAMP_TAIL_HOME] {
        let mut zeros = KVec::<u8>::zeroed(bs as usize).map_err(|_| "zeros")?;
        zeros.as_mut_slice().fill(0);
        device
            .write_at(u64::from(home) * u64::from(bs), zeros.as_slice())
            .map_err(|_| "clear home")?;
    }

    device.disarm();
    let wide = clamp_fill(device, payload.as_slice())?;
    if let Some(extra) = overclaim {
        // Slot 1 is the wide record's header.
        device.arm(u64::from(synth_slot_block(1)) * u64::from(bs), wide + extra);
    }

    let (log, recovery) = synthetic_log(device, CLAMP_SLOTS)?;
    drop(log);
    device.disarm();

    if recovery.transactions != 2 {
        return Err("the fixture's two committed transactions did not both replay");
    }
    // One write per payload slot of the wide record, plus the tail
    // transaction's one. A cursor that overshot leaves the tail out.
    if recovery.blocks != wide + 1 {
        return Err("the replay wrote a different number of blocks home");
    }
    payload.as_mut_slice().fill(0);
    device
        .read_at(
            u64::from(CLAMP_TAIL_HOME) * u64::from(bs),
            payload.as_mut_slice(),
        )
        .map_err(|_| "read")?;
    if &payload.as_slice()[..PAYLOAD.len()] != PAYLOAD {
        return Err("the transaction past the overclaimed record never reached its home");
    }
    Ok((recovery.transactions, recovery.blocks))
}

/// Write a full-width `DATA` record and a one-block one, each committed, and
/// answer the entries the wide one lists. Its own frame: the target array is
/// one entry per header slot.
#[inline(never)]
fn clamp_fill(device: &TamperDevice, payload: &[u8]) -> Result<u32, &'static str> {
    let (mut log, _) = synthetic_log(device, CLAMP_SLOTS)?;
    let wide = log.max_entries();
    let mut targets = KVec::<u32>::zeroed(wide).map_err(|_| "targets")?;
    targets.as_mut_slice().fill(CLAMP_WIDE_HOME);
    log.write_record(targets.as_slice(), device, &mut |_| Some(payload))
        .map_err(|_| "write_record")?;
    log.write_commit(device).map_err(|_| "write_commit")?;
    log.write_record(&[CLAMP_TAIL_HOME], device, &mut |_| Some(payload))
        .map_err(|_| "write_record")?;
    log.write_commit(device).map_err(|_| "write_commit")?;
    // Dropped without a check point: the next attach is what has to find both
    // transactions and apply them.
    drop(log);
    u32::try_from(wide).map_err(|_| "max_entries")
}

slopos_testing::stest!(
    name = test_ext2_journal_record_count_is_clamped_for_the_cursor_too,
    suite = fs
);
