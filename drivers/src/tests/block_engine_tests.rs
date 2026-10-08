//! The request engine and the disk over it, graded on every transport the
//! harness attaches a scratch disk for: virtio-blk, NVMe, and NVMe formatted
//! with 4096-byte logical blocks. Destructive, so only ever a scratch disk,
//! each test in a region of its own:
//!
//! | bytes            | test                             |
//! |------------------|----------------------------------|
//! | 1 MiB            | multi-block read and write       |
//! | 1.5 MiB          | one request over one page        |
//! | 1.95 MiB         | flush                            |
//! | 2.25 MiB         | vectored write                   |
//! | 2.5 and 2.75 MiB | concurrent requests              |
//! | 3 MiB            | late completion, request counters|
//! | 3.5 MiB          | abandoned-write fence            |
//! | 4 MiB            | write readback, partial blocks   |
//! | 6 MiB            | killed writer                    |

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use slopos_abi::task::TaskPriority;
use slopos_core::tests::helpers::{kill_task, mark_current_killed};
use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, stats};
use slopos_ostd::sync::wait_queue::current_task_is_killed;
use slopos_ostd::{KArc, KBox, KVec};
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ok, assert_test, fail, pass};

use crate::block::engine::hooks;
use crate::block::{self, EngineDisk};

const SCRATCH_DISKS: [&[u8]; 4] = [b"vdb", b"nvme0n2", b"nvme1n2", b"sda"];

fn show(name: &[u8]) -> &str {
    core::str::from_utf8(name).unwrap_or("?")
}

fn scratch(name: &[u8]) -> Result<KArc<EngineDisk>, TestResult> {
    block::disk(name).ok_or_else(|| fail!("scratch disk {} not attached", show(name)))
}

fn claim(name: &[u8]) -> Result<KBox<dyn BlockDevice + Send + Sync>, TestResult> {
    block::claim(name).map_err(|e| fail!("claiming {} failed: {:?}", show(name), e))
}

macro_rules! on_scratch {
    ($body:ident => $virtio:ident, $nvme:ident, $nvme_4k:ident, $usb:ident) => {
        pub fn $virtio() -> TestResult {
            $body(SCRATCH_DISKS[0])
        }
        pub fn $nvme() -> TestResult {
            $body(SCRATCH_DISKS[1])
        }
        pub fn $nvme_4k() -> TestResult {
            $body(SCRATCH_DISKS[2])
        }
        pub fn $usb() -> TestResult {
            $body(SCRATCH_DISKS[3])
        }
        slopos_testing::stest!(name = $virtio, suite = block_engine);
        slopos_testing::stest!(name = $nvme, suite = block_engine);
        slopos_testing::stest!(name = $nvme_4k, suite = block_engine);
        slopos_testing::stest!(name = $usb, suite = block_engine);
    };
}

fn pattern(len: usize, seed: u8) -> Result<KVec<u8>, TestResult> {
    let mut buf = KVec::<u8>::zeroed(len).map_err(|_| fail!("pattern alloc"))?;
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31) ^ (i >> 7)) as u8 ^ seed;
    }
    Ok(buf)
}

/// The root disk carries the ext2 superblock this boot mounted.
pub fn test_block_root_superblock_reads() -> TestResult {
    let Some(root) = block::disk_name(0) else {
        return fail!("no disk registered");
    };
    let disk = match scratch(root.as_bytes()) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let mut buf = [0u8; 512];
    assert_test!(
        disk.read_at(1024, &mut buf).is_ok(),
        "the superblock read must succeed"
    );
    let magic = u16::from_le_bytes([buf[0x38], buf[0x39]]);
    assert_eq_test!(magic, 0xEF53, "ext2 superblock magic mismatch");
    pass!()
}

/// disk0 is the root image; names follow probe order and NSIDs.
pub fn test_block_registry_names() -> TestResult {
    assert_test!(
        block::disk_name(0).is_some_and(|n| n.as_bytes() == b"nvme0n1"),
        "disk0 must be the root image on the first NVMe namespace"
    );
    assert_test!(
        block::first_fixed_disk().is_some_and(|n| n.as_bytes() == b"nvme0n1"),
        "a stick never becomes the disk root=auto takes"
    );
    for name in SCRATCH_DISKS {
        assert_test!(
            block::disk(name).is_some(),
            "{} must be registered",
            show(name)
        );
    }
    assert_test!(block::disk(b"vdz").is_none(), "vdz must not exist");
    assert_test!(
        block::disk(b"nvme0n9").is_none(),
        "an NSID the controller lacks must not exist"
    );
    assert_test!(
        block::disk_count() >= 6,
        "at least the root, four scratches and the media disk"
    );
    assert_eq_test!(
        block::disk(b"nvme1n2").map(|d| d.logical_block_size()),
        Some(4096),
        "the 4K-native namespace must report 4096-byte blocks"
    );
    pass!()
}

/// A whole-disk claim is exclusive, and returns once dropped.
pub fn test_block_whole_disk_claim_is_exclusive() -> TestResult {
    let name = SCRATCH_DISKS[1];
    let first = match claim(name) {
        Ok(c) => c,
        Err(r) => return r,
    };
    assert_test!(
        matches!(block::claim(name), Err(block::ClaimError::Busy)),
        "a second claim must be Busy while the first is live"
    );
    drop(first);
    assert_test!(
        block::claim(name).is_ok(),
        "the claim must be re-acquirable once dropped"
    );
    pass!()
}

fn write_readback(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let offset = 4 << 20;
    let data = assert_ok!(pattern(4096, 0x11), "pattern");
    assert_test!(device.write_at(offset, &data).is_ok(), "write must succeed");
    let mut back = assert_ok!(KVec::<u8>::zeroed(4096), "readback");
    assert_test!(
        device.read_at(offset, &mut back).is_ok(),
        "readback must succeed"
    );
    assert_test!(back[..] == data[..], "readback must match what was written");
    pass!()
}
on_scratch!(write_readback => test_block_write_readback_virtio, test_block_write_readback_nvme, test_block_write_readback_nvme_4k, test_block_write_readback_usb);

/// A span of whole blocks is one request; a sub-span with partial blocks at
/// both ends reads through the same device.
fn multiblock(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    const SPAN: usize = 3 * 4096;
    let offset = 1 << 20;
    let data = assert_ok!(pattern(SPAN, 0x22), "pattern");
    assert_test!(device.write_at(offset, &data).is_ok(), "write must succeed");
    let mut back = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback");
    assert_test!(
        device.read_at(offset, &mut back).is_ok(),
        "readback must succeed"
    );
    assert_test!(back[..] == data[..], "multi-block readback must match");

    let mut sub = assert_ok!(KVec::<u8>::zeroed(5000), "sub-span");
    assert_test!(
        device.read_at(offset + 100, &mut sub).is_ok(),
        "an unaligned sub-span read must succeed"
    );
    assert_test!(
        sub[..] == data[100..5100],
        "the sub-span must match the pattern slice"
    );
    pass!()
}
on_scratch!(multiblock => test_block_multiblock_virtio, test_block_multiblock_nvme, test_block_multiblock_nvme_4k, test_block_multiblock_usb);

/// A span straddling a 4 KiB boundary covers no logical block whole on any
/// disk: the bytes around it survive the read-modify-write of both ends.
fn partial_block_write(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let offset = (4 << 20) + 8192;
    let base = assert_ok!(pattern(8192, 0x33), "base");
    assert_test!(
        device.write_at(offset, &base).is_ok(),
        "seeding must succeed"
    );
    let patch = [0xE7u8; 300];
    assert_test!(
        device.write_at(offset + 4000, &patch).is_ok(),
        "a sub-block write must succeed"
    );
    let mut back = assert_ok!(KVec::<u8>::zeroed(8192), "readback");
    assert_test!(device.read_at(offset, &mut back).is_ok(), "readback");
    let mut want = base;
    want[4000..4300].copy_from_slice(&patch);
    assert_test!(
        back[..] == want[..],
        "a read-modify-write must leave the rest of each block as it was"
    );
    pass!()
}
on_scratch!(partial_block_write => test_block_partial_write_virtio, test_block_partial_write_nvme, test_block_partial_write_nvme_4k, test_block_partial_write_usb);

fn flush(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let data = assert_ok!(pattern(4096, 0xA5), "pattern");
    assert_test!(
        device.write_at(2_048_000, &data).is_ok(),
        "write before flush must succeed"
    );
    assert_test!(device.flush().is_ok(), "flush must complete");
    pass!()
}
on_scratch!(flush => test_block_flush_virtio, test_block_flush_nvme, test_block_flush_nvme_4k, test_block_flush_usb);

/// 8 KiB is two data pages in one request.
fn single_request_over_one_page(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    const SPAN: usize = 8192;
    let offset = 3 << 19;
    let data = assert_ok!(pattern(SPAN, 0x44), "pattern");
    let before = stats::snapshot();
    assert_test!(device.write_at(offset, &data).is_ok(), "write must succeed");
    let after = stats::snapshot();
    assert_test!(
        after.write_requests - before.write_requests >= 1,
        "the write must be counted"
    );
    let mut back = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback");
    assert_test!(device.read_at(offset, &mut back).is_ok(), "readback");
    assert_test!(
        back[..] == data[..],
        "a span over one bounce page must round-trip byte for byte"
    );
    pass!()
}
on_scratch!(single_request_over_one_page => test_block_two_page_request_virtio, test_block_two_page_request_nvme, test_block_two_page_request_nvme_4k, test_block_two_page_request_usb);

/// Non-adjacent buffers land back to back in one device extent.
fn write_vectored(name: &[u8]) -> TestResult {
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let offset = 9 << 18;
    let a = assert_ok!(pattern(1024, 0xA0), "a");
    let b = assert_ok!(pattern(2048, 0xB0), "b");
    let c = assert_ok!(pattern(1024, 0xC0), "c");
    let segs: [&[u8]; 3] = [&a, &b, &c];
    assert_test!(
        device.write_vectored(offset, &segs).is_ok(),
        "a vectored write of three segments must succeed"
    );
    let mut back = assert_ok!(KVec::<u8>::zeroed(4096), "readback");
    assert_test!(device.read_at(offset, &mut back).is_ok(), "readback");
    assert_test!(
        back[..1024] == a[..] && back[1024..3072] == b[..] && back[3072..] == c[..],
        "each segment must land directly after the one before"
    );
    pass!()
}
on_scratch!(write_vectored => test_block_write_vectored_virtio, test_block_write_vectored_nvme, test_block_write_vectored_nvme_4k, test_block_write_vectored_usb);

/// Two requests in flight at once, each completing with its own data.
fn concurrent_requests(name: &[u8]) -> TestResult {
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    const SPAN: usize = 4096;
    let (one, two) = (10u64 << 18, 11u64 << 18);
    let first = assert_ok!(pattern(SPAN, 0x11), "first");
    let second = assert_ok!(pattern(SPAN, 0xEE), "second");
    assert_test!(
        device.write_at(one, &first).is_ok() && device.write_at(two, &second).is_ok(),
        "seeding both regions must succeed"
    );
    let engine = disk.engine();
    let ns = disk.namespace();
    let in_flight_one = match engine.submit_read(ns, one, SPAN) {
        Ok(idx) => idx,
        Err(e) => return fail!("first submission failed: {:?}", e),
    };
    let in_flight_two = match engine.submit_read(ns, two, SPAN) {
        Ok(idx) => idx,
        Err(e) => return fail!("second submission with the first in flight: {:?}", e),
    };
    let mut got_one = assert_ok!(KVec::<u8>::zeroed(SPAN), "first readback");
    let mut got_two = assert_ok!(KVec::<u8>::zeroed(SPAN), "second readback");
    if let Err(e) = engine.complete(in_flight_one, &mut got_one) {
        return fail!("first request did not complete: {:?}", e);
    }
    if let Err(e) = engine.complete(in_flight_two, &mut got_two) {
        return fail!("second request did not complete: {:?}", e);
    }
    assert_test!(got_one[..] == first[..], "the first request's own extent");
    assert_test!(got_two[..] == second[..], "the second request's own extent");
    pass!()
}
on_scratch!(concurrent_requests => test_block_concurrent_requests_virtio, test_block_concurrent_requests_nvme, test_block_concurrent_requests_nvme_4k, test_block_concurrent_requests_usb);

/// A span past the end of the medium is a bounds error.
fn read_past_capacity(name: &[u8]) -> TestResult {
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let capacity = disk.capacity();
    let mut buf = [0u8; 512];
    match disk.read_at(capacity - 256, &mut buf) {
        Err(BlockDeviceError::OutOfBounds) => {}
        other => return fail!("want OutOfBounds past capacity, got {:?}", other),
    }
    match disk.read_at(u64::MAX - 16, &mut buf) {
        Err(BlockDeviceError::OutOfBounds) => pass!(),
        other => fail!("want OutOfBounds for an overflowing span, got {:?}", other),
    }
}
on_scratch!(read_past_capacity => test_block_past_capacity_virtio, test_block_past_capacity_nvme, test_block_past_capacity_nvme_4k, test_block_past_capacity_usb);

/// A completion landing while the timeout epilogue allocates the slot's
/// replacement pages must be handed to the caller and the slot kept in
/// service. The rounds run past the slot count, so a per-event leak wedges
/// the device inside the test rather than after it.
fn late_completion_keeps_slot(name: &[u8]) -> TestResult {
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    const SPAN: usize = 4096;
    let offset = 3 << 20;
    let data = assert_ok!(pattern(SPAN, 0x3C), "pattern");
    assert_test!(device.write_at(offset, &data).is_ok(), "seeding");
    let engine = disk.engine();
    let slots = engine.slots_in_service();
    assert_test!(slots > 0, "the engine must start with a slot in service");
    for round in 0..slots + 2 {
        let mut got = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback");
        if let Err(e) =
            engine.read_completing_in_replacement_window(disk.namespace(), offset, &mut got)
        {
            return fail!("round {}: the late completion was lost: {:?}", round, e);
        }
        assert_test!(got[..] == data[..], "the late completion's own payload");
        assert_eq_test!(
            engine.slots_in_service(),
            slots,
            "the slot must return to service after the epilogue"
        );
    }
    pass!()
}
on_scratch!(late_completion_keeps_slot => test_block_late_completion_virtio, test_block_late_completion_nvme, test_block_late_completion_nvme_4k, test_block_late_completion_usb);

/// One count is one device request: a span wider than the largest transfer
/// costs more than one write, and a sub-block write pays for its
/// read-modify-write pair.
fn counters_per_request(name: &[u8]) -> TestResult {
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    let device = match claim(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    const WIDE: usize = 256 * 1024;
    let offset = 3 << 20;
    let wide = assert_ok!(KVec::<u8>::zeroed(WIDE), "wide payload");
    // Deltas and lower bounds: the counters are global and the ext2 flusher
    // writes to another disk while this runs.
    let before = stats::snapshot();
    assert_test!(device.write_at(offset, &wide).is_ok(), "the wide write");
    let after = stats::snapshot();
    assert_test!(
        after.write_requests - before.write_requests
            >= (WIDE / disk.engine().max_transfer()) as u64,
        "a span wider than the largest transfer must count as several writes"
    );
    assert_test!(
        after.blocks_written - before.blocks_written >= (WIDE / 512) as u64,
        "the block total must be the bytes the requests carried"
    );
    let before = stats::snapshot();
    assert_test!(
        device.write_at(offset + 3, &wide[..8]).is_ok(),
        "the sub-block write"
    );
    let after = stats::snapshot();
    assert_test!(
        after.read_requests - before.read_requests >= 1
            && after.write_requests - before.write_requests >= 1,
        "a sub-block write is a read and a write at the device"
    );
    pass!()
}
on_scratch!(counters_per_request => test_block_counters_virtio, test_block_counters_nvme, test_block_counters_nvme_4k, test_block_counters_usb);

/// Which scratch disk a spawned writer thread targets.
static THREAD_DISK: AtomicUsize = AtomicUsize::new(0);

const KILLED_OFFSET: u64 = 6 << 20;
const KILLED_WRITES: u64 = 64;
static KILLED_PATTERN: [u8; 4096] = [0x6B; 4096];
const REFUSED_OFFSET: u64 = KILLED_OFFSET + KILLED_WRITES * 4096;
const REFUSED_OLDER: [u8; 512] = [0x21; 512];
const REFUSED_NEWER: [u8; 512] = [0x12; 512];

const KILLED_PENDING: u8 = 0;
const KILLED_PASS: u8 = 1;
const KILLED_ABANDONED: u8 = 2;
const KILLED_SUBMITTED: u8 = 3;
const KILLED_UNMARKED: u8 = 4;
const KILLED_FAILED: u8 = 5;

static KILLED_OUTCOME: AtomicU8 = AtomicU8::new(KILLED_PENDING);

/// A kernel thread, because the harness runs on a stub with no task to kill.
fn killed_writer() {
    KILLED_OUTCOME.store(killed_writes(), Ordering::Release);
}

fn killed_writes() -> u8 {
    let name = SCRATCH_DISKS[THREAD_DISK.load(Ordering::Acquire)];
    let Ok(device) = block::claim(name) else {
        return KILLED_FAILED;
    };
    if device.write_at(REFUSED_OFFSET, &REFUSED_OLDER).is_err() {
        return KILLED_FAILED;
    }
    for i in 0..KILLED_WRITES {
        hooks::kill_after_next_submit();
        let wrote = device.write_at(KILLED_OFFSET + i * 4096, &KILLED_PATTERN);
        let killed = current_task_is_killed();
        mark_current_killed(false);
        if !killed {
            return KILLED_UNMARKED;
        }
        if wrote.is_err() {
            return KILLED_ABANDONED;
        }
    }
    if !mark_current_killed(true) {
        return KILLED_UNMARKED;
    }
    let refused = device.write_at(REFUSED_OFFSET, &REFUSED_NEWER);
    mark_current_killed(false);
    match refused {
        Err(BlockDeviceError::Interrupted) => KILLED_PASS,
        Ok(()) => KILLED_SUBMITTED,
        Err(_) => KILLED_FAILED,
    }
}

/// A requester killed with a write in the device waits it out, and one killed
/// before a write reaches the device sends nothing.
fn killed_write_is_waited_out(index: usize) -> TestResult {
    let name = SCRATCH_DISKS[index];
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    THREAD_DISK.store(index, Ordering::Release);
    KILLED_OUTCOME.store(KILLED_PENDING, Ordering::Release);
    if slopos_ostd::task::spawn("killed-writer", killed_writer, TaskPriority::Normal).is_err() {
        return fail!("could not spawn the writer thread");
    }
    let finished = crate::hpet::poll_wait(
        &|| KILLED_OUTCOME.load(Ordering::Acquire) != KILLED_PENDING,
        30_000,
    );
    assert_test!(finished, "the killed writer never finished");
    match KILLED_OUTCOME.load(Ordering::Acquire) {
        KILLED_PASS => {}
        KILLED_ABANDONED => return fail!("a write in the device came back unfinished"),
        KILLED_SUBMITTED => return fail!("a task already killed sent a new write"),
        KILLED_UNMARKED => return fail!("the writer thread could not be marked killed"),
        _ => return fail!("the writer's setup failed"),
    }
    assert_eq_test!(
        disk.engine().quarantine_count(),
        0,
        "a waited-out write must leave nothing quarantined"
    );
    let mut back = assert_ok!(KVec::<u8>::zeroed(KILLED_PATTERN.len()), "readback");
    let last = KILLED_OFFSET + (KILLED_WRITES - 1) * 4096;
    assert_test!(
        disk.read_at(last, &mut back).is_ok() && back[..] == KILLED_PATTERN[..],
        "the killed requester's writes must be on the device"
    );
    assert_test!(
        disk.read_at(REFUSED_OFFSET, &mut back[..512]).is_ok() && back[..512] == REFUSED_OLDER[..],
        "the refused write must not be on the device"
    );
    pass!()
}

fn killed_write(name: &[u8]) -> TestResult {
    let index = SCRATCH_DISKS.iter().position(|d| *d == name).unwrap_or(0);
    killed_write_is_waited_out(index)
}
on_scratch!(killed_write => test_block_killed_write_virtio, test_block_killed_write_nvme, test_block_killed_write_nvme_4k, test_block_killed_write_usb);

const FENCED_OFFSET: u64 = 7 << 19;
const FENCED_OLDER: [u8; 512] = [0x3C; 512];
const FENCED_NEWER: [u8; 512] = [0xC3; 512];
/// The writer's span: inside one block, so the write is a read-modify-write.
const FENCED_SPAN: core::ops::Range<usize> = 128..384;
/// Long enough for an unfenced write to have landed.
const FENCE_HOLD_MS: u32 = 100;

const FENCED_PENDING: u8 = 0;
const FENCED_AFTER_RETURN: u8 = 1;
const FENCED_BEFORE_RETURN: u8 = 2;
const FENCED_INTERRUPTED: u8 = 3;
const FENCED_FAILED: u8 = 4;

static FENCE_RETURNED: AtomicBool = AtomicBool::new(false);
static FENCED_STARTED: AtomicBool = AtomicBool::new(false);
static FENCED_OUTCOME: AtomicU8 = AtomicU8::new(FENCED_PENDING);

/// A kernel thread, so the write parks on the fence rather than polling it.
fn fenced_writer() {
    let name = SCRATCH_DISKS[THREAD_DISK.load(Ordering::Acquire)];
    let outcome = match block::claim(name) {
        Ok(device) => {
            FENCED_STARTED.store(true, Ordering::Release);
            let offset = FENCED_OFFSET + FENCED_SPAN.start as u64;
            match device.write_at(offset, &FENCED_NEWER[FENCED_SPAN]) {
                Ok(()) if FENCE_RETURNED.load(Ordering::Acquire) => FENCED_AFTER_RETURN,
                Ok(()) => FENCED_BEFORE_RETURN,
                Err(BlockDeviceError::Interrupted) => FENCED_INTERRUPTED,
                Err(_) => FENCED_FAILED,
            }
        }
        Err(_) => FENCED_FAILED,
    };
    FENCED_OUTCOME.store(outcome, Ordering::Release);
}

/// Stage an abandoned write, run [`fenced_writer`] behind it, `kill` the writer
/// if asked, then return the staged write.
fn fenced_write(disk: &EngineDisk, kill: bool) -> Result<u8, &'static str> {
    let engine = disk.engine();
    let tag = engine
        .stage_abandoned_write(disk.namespace())
        .ok_or("could not stage")?;
    FENCE_RETURNED.store(false, Ordering::Release);
    FENCED_STARTED.store(false, Ordering::Release);
    FENCED_OUTCOME.store(FENCED_PENDING, Ordering::Release);

    let writer = slopos_ostd::task::spawn("fenced-writer", fenced_writer, TaskPriority::Normal);
    let started = writer.is_ok()
        && crate::hpet::poll_wait(&|| FENCED_STARTED.load(Ordering::Acquire), 10_000);
    crate::hpet::poll_wait(&|| false, FENCE_HOLD_MS);
    let mut readback = [0u8; 100];
    let read = disk.read_at(FENCED_OFFSET + 1, &mut readback);
    if kill && let Ok(id) = writer {
        kill_task(id.as_u32());
    }
    FENCE_RETURNED.store(true, Ordering::Release);
    engine.return_abandoned_write(tag);

    if !started {
        return Err("the writer thread never started");
    }
    if read.is_err() {
        return Err("a read waited behind an abandoned write");
    }
    if !crate::hpet::poll_wait(
        &|| FENCED_OUTCOME.load(Ordering::Acquire) != FENCED_PENDING,
        10_000,
    ) {
        return Err("the fenced writer never finished");
    }
    Ok(FENCED_OUTCOME.load(Ordering::Acquire))
}

/// A write waits until an abandoned one is returned — a sub-block one before
/// it reads its block — a killed one sends nothing, and a read never waits.
fn waits_out_an_abandoned_write(name: &[u8]) -> TestResult {
    let index = SCRATCH_DISKS.iter().position(|d| *d == name).unwrap_or(0);
    let disk = match scratch(name) {
        Ok(d) => d,
        Err(r) => return r,
    };
    {
        let device = match claim(name) {
            Ok(d) => d,
            Err(r) => return r,
        };
        assert_test!(
            device.write_at(FENCED_OFFSET, &FENCED_OLDER).is_ok(),
            "seeding the region must succeed"
        );
    }
    THREAD_DISK.store(index, Ordering::Release);

    let _ = hooks::take_rmw_read_past_fence();
    match fenced_write(&disk, true) {
        Ok(FENCED_INTERRUPTED) => {}
        Ok(FENCED_AFTER_RETURN | FENCED_BEFORE_RETURN) => {
            return fail!("a writer killed behind the fence still sent its write");
        }
        Ok(_) => return fail!("the killed fenced writer failed"),
        Err(msg) => return fail!("{}", msg),
    }
    let mut back = [0u8; 512];
    assert_test!(
        disk.read_at(FENCED_OFFSET, &mut back).is_ok() && back == FENCED_OLDER,
        "the killed writer's write must not be on the device"
    );

    match fenced_write(&disk, false) {
        Ok(FENCED_AFTER_RETURN) => {}
        Ok(FENCED_BEFORE_RETURN) => {
            return fail!("a write reached the device while an earlier one could still land");
        }
        Ok(_) => return fail!("the fenced write failed once the abandoned one was returned"),
        Err(msg) => return fail!("{}", msg),
    }
    assert_test!(
        !hooks::take_rmw_read_past_fence(),
        "a read-modify-write read its block while an abandoned write could still land"
    );
    let mut want = FENCED_OLDER;
    want[FENCED_SPAN].copy_from_slice(&FENCED_NEWER[FENCED_SPAN]);
    assert_test!(
        disk.read_at(FENCED_OFFSET, &mut back).is_ok() && back == want,
        "the later write must be what the block holds"
    );
    pass!()
}
on_scratch!(waits_out_an_abandoned_write => test_block_abandoned_write_fence_virtio, test_block_abandoned_write_fence_nvme, test_block_abandoned_write_fence_nvme_4k, test_block_abandoned_write_fence_usb);

slopos_testing::stest!(
    name = test_block_root_superblock_reads,
    suite = block_engine
);
slopos_testing::stest!(name = test_block_registry_names, suite = block_engine);
slopos_testing::stest!(
    name = test_block_whole_disk_claim_is_exclusive,
    suite = block_engine
);
