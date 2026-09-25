//! VirtIO completion primitive regression tests: `IrqEdgeEvent`, the sleeping
//! `Mutex`, virtqueue descriptor free-list invariants, HPET `period_fs()`, and
//! live virtio-blk I/O after probe.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use slopos_abi::task::TaskPriority;
use slopos_core::tests::helpers::{kill_task, mark_current_killed};
use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_ostd::sync::wait_queue::current_task_is_killed;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_ok, assert_test, fail, pass};

use slopos_fs::blockdev::{BlockDevice, BlockDeviceError, BlockDeviceIndex, stats};
use slopos_ostd::mm::heap::KVec;
use slopos_ostd::sync::Mutex;

use crate::hpet;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{EdgeWait, IrqEdgeEvent};
use crate::virtio_blk;
use crate::virtio_blk::BlkClaimError;

/// The disposable scratch device (virtio-disk1). Destructive block tests
/// target THIS index, never disk0 — the live root-fs image.
const SCRATCH: BlockDeviceIndex = BlockDeviceIndex(1);

pub fn test_edge_event_new_not_signaled() -> TestResult {
    let ev = IrqEdgeEvent::new();
    assert_test!(!ev.try_consume(), "new IrqEdgeEvent should not be signaled");
    pass!()
}

pub fn test_edge_event_signal_then_consume() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should return true after signal"
    );
    pass!()
}

pub fn test_edge_event_double_consume() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    let first = ev.try_consume();
    let second = ev.try_consume();
    assert_test!(first, "first try_consume should succeed");
    assert_test!(!second, "second try_consume should fail (single-shot)");
    pass!()
}

pub fn test_edge_event_reset_clears_signal() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.reset();
    assert_test!(
        !ev.try_consume(),
        "try_consume should fail after reset clears signal"
    );
    pass!()
}

pub fn test_edge_event_signal_after_reset() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.reset();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should succeed after signal-reset-signal"
    );
    pass!()
}

pub fn test_edge_event_multiple_signals() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    ev.signal();
    ev.signal();
    assert_test!(ev.try_consume(), "first consume after triple signal");
    assert_test!(
        !ev.try_consume(),
        "second consume should fail — only one event"
    );
    pass!()
}

pub fn test_edge_event_wait_presignaled() -> TestResult {
    let ev = IrqEdgeEvent::new();
    ev.signal();
    assert_eq_test!(
        ev.wait_timeout(5000),
        EdgeWait::Latched,
        "a pre-signaled wait must consume the latched edge without parking"
    );
    pass!()
}

pub fn test_edge_event_wait_timeout() -> TestResult {
    const TIMEOUT_MS: u32 = 1;
    let ev = IrqEdgeEvent::new();

    let Some(owed_ticks) = hpet::ms_to_ticks(TIMEOUT_MS) else {
        assert_eq_test!(
            ev.wait_timeout(TIMEOUT_MS),
            EdgeWait::TimedOut,
            "unsignaled wait should time out"
        );
        return pass!();
    };

    let start = hpet::read_counter();
    let outcome = ev.wait_timeout(TIMEOUT_MS);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);

    assert_eq_test!(
        outcome,
        EdgeWait::TimedOut,
        "unsignaled wait should time out"
    );
    assert_test!(
        elapsed_ticks >= owed_ticks,
        "timeout returned after {} HPET ticks, owing {}",
        elapsed_ticks,
        owed_ticks
    );
    pass!()
}

pub fn test_sleep_mutex_lock_unlock() -> TestResult {
    let m = Mutex::new(7u32, lock_class!("test.virtio_mutex1", LOCK_LEVEL_RESOURCE));
    {
        let Ok(mut g) = m.lock() else {
            return fail!("uncontended lock must succeed");
        };
        *g += 1;
    }
    let Ok(g) = m.lock() else {
        return fail!("uncontended relock must succeed");
    };
    assert_eq_test!(*g, 8, "mutated value must persist across lock cycles");
    pass!()
}

pub fn test_sleep_mutex_try_lock_contention() -> TestResult {
    let m = Mutex::new(0u32, lock_class!("test.virtio_mutex2", LOCK_LEVEL_RESOURCE));
    let Ok(g) = m.lock() else {
        return fail!("uncontended lock must succeed");
    };
    assert_test!(
        m.try_lock().is_none(),
        "try_lock must fail while the mutex is held"
    );
    drop(g);
    assert_test!(
        m.try_lock().is_some(),
        "try_lock must succeed after the holder releases"
    );
    pass!()
}

pub fn test_sleep_mutex_relock_after_try() -> TestResult {
    let m = Mutex::new(1u32, lock_class!("test.virtio_mutex3", LOCK_LEVEL_RESOURCE));
    {
        let mut g = match m.try_lock() {
            Some(g) => g,
            None => return fail!("try_lock on a fresh mutex must succeed"),
        };
        *g = 2;
    }
    let Ok(g) = m.lock() else {
        return fail!("uncontended lock must succeed");
    };
    assert_eq_test!(*g, 2, "lock after try_lock must observe the mutation");
    pass!()
}

pub fn test_virtqueue_unready_alloc_none() -> TestResult {
    let mut q = Virtqueue::new();
    assert_eq_test!(q.free_count(), 0, "fresh queue advertises no descriptors");
    assert_test!(
        q.alloc_desc().is_none(),
        "alloc_desc on an unready queue must return None"
    );
    q.free_desc(3);
    assert_eq_test!(
        q.free_count(),
        0,
        "free_desc of an out-of-range index must be a no-op"
    );
    pass!()
}

pub fn test_hpet_period_fs_nonzero() -> TestResult {
    assert_test!(hpet::is_available(), "HPET must be available for this test");
    let period = hpet::period_fs();
    assert_test!(period > 0, "period_fs should be > 0 when HPET is init'd");
    assert_test!(
        period <= 100_000_000,
        "period_fs {} exceeds HPET spec max",
        period
    );
    pass!()
}

pub fn test_hpet_period_fs_matches_full_name() -> TestResult {
    assert_eq_test!(
        hpet::period_fs(),
        hpet::period_femtoseconds(),
        "period_fs and period_femtoseconds should return the same value"
    );
    pass!()
}

pub fn test_virtio_blk_read_interrupt_driven() -> TestResult {
    // Reads target disk0 (the root-fs image), which carries the ext2 superblock.
    let Some(disk0) = virtio_blk::blk_device_by_index(BlockDeviceIndex(0)) else {
        return fail!("root-fs block device (disk0) not present");
    };
    assert_test!(virtio_blk::blk_is_ready(disk0), "virtio-blk must be ready");

    let mut buf = [0u8; 512];
    let ok = virtio_blk::blk_read(disk0, 1024, &mut buf).is_ok();
    assert_test!(ok, "superblock read should succeed via IRQ-driven I/O");
    let magic = u16::from_le_bytes([buf[0x38], buf[0x39]]);
    assert_eq_test!(magic, 0xEF53, "ext2 superblock magic mismatch");
    pass!()
}

pub fn test_virtio_blk_consecutive_reads() -> TestResult {
    let Some(disk0) = virtio_blk::blk_device_by_index(BlockDeviceIndex(0)) else {
        return fail!("root-fs block device (disk0) not present");
    };
    assert_test!(virtio_blk::blk_is_ready(disk0), "virtio-blk must be ready");

    let mut buf1 = [0u8; 512];
    let mut buf2 = [0u8; 512];
    let ok1 = virtio_blk::blk_read(disk0, 0, &mut buf1).is_ok();
    let ok2 = virtio_blk::blk_read(disk0, 512, &mut buf2).is_ok();
    assert_test!(ok1, "first consecutive read should succeed");
    assert_test!(ok2, "second consecutive read should succeed");
    pass!()
}

pub fn test_virtio_blk_write_readback_interrupt_driven() -> TestResult {
    // The scratch disk is blank each run and the claim is exclusive, so
    // nothing needs saving or restoring.
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    let offset = 8192u64 * 512;
    let pattern: [u8; 512] = {
        let mut p = [0u8; 512];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        p
    };

    assert_test!(
        token.write_at(offset, &pattern).is_ok(),
        "write should succeed via IRQ-driven I/O"
    );
    let mut readback = [0u8; 512];
    assert_test!(
        token.read_at(offset, &mut readback).is_ok(),
        "readback should succeed via IRQ-driven I/O"
    );
    assert_test!(
        readback == pattern,
        "readback data should match written pattern"
    );
    pass!()
}

/// A sector-aligned span wider than one sector goes out as a single
/// scatter-gather chain; an unaligned sub-span exercises the head/tail split.
pub fn test_virtio_blk_multisector_write_readback() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    // Heap buffers: the three together are 3772 bytes, which on one stack
    // frame steps past the 4 KiB guard page `stack-probes: none` cannot catch.
    const SPAN: usize = 3 * 512;
    let offset = 2048u64 * 512;
    let mut pattern = assert_ok!(KVec::<u8>::zeroed(SPAN), "pattern buffer");
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = ((i * 7) ^ (i >> 8)) as u8;
    }

    assert_test!(
        token.write_at(offset, &pattern).is_ok(),
        "multi-sector write should succeed"
    );

    let mut readback = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback buffer");
    assert_test!(
        token.read_at(offset, &mut readback).is_ok(),
        "multi-sector readback should succeed"
    );
    assert_test!(
        readback[..] == pattern[..],
        "multi-sector readback should match the written pattern"
    );

    // Unaligned sub-span: partial head, aligned middle, partial tail.
    let mut sub = assert_ok!(KVec::<u8>::zeroed(700), "sub-span buffer");
    assert_test!(
        token.read_at(offset + 100, &mut sub).is_ok(),
        "unaligned sub-span read should succeed"
    );
    assert_test!(
        sub[..] == pattern[100..800],
        "unaligned sub-span must match the pattern slice"
    );
    pass!()
}

pub fn test_virtio_blk_flush_completes() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    let pattern = [0xA5u8; 512];
    assert_test!(
        token.write_at(4000 * 512, &pattern).is_ok(),
        "write before flush should succeed"
    );
    assert_test!(
        token.flush().is_ok(),
        "flush barrier should complete without timing out"
    );
    pass!()
}

pub fn test_block_device_exclusive_write_claim() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };

    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("first open_writer should succeed: {:?}", e),
    };

    assert_test!(
        matches!(
            virtio_blk::open_writer(handle),
            Err(BlkClaimError::AlreadyClaimed)
        ),
        "second open_writer must return AlreadyClaimed while a token is live"
    );

    drop(token);
    assert_test!(
        virtio_blk::open_writer(handle).is_ok(),
        "exclusive claim must be re-acquirable after the token is dropped"
    );
    pass!()
}

pub fn test_block_device_lookup_bounds() -> TestResult {
    assert_test!(
        virtio_blk::blk_device_by_index(BlockDeviceIndex(0)).is_some(),
        "disk0 (root fs) must be present"
    );
    assert_test!(
        virtio_blk::blk_device_by_index(SCRATCH).is_some(),
        "disk1 (scratch) must be present in the test harness"
    );
    assert_test!(
        virtio_blk::blk_device_by_index(BlockDeviceIndex(99)).is_none(),
        "an out-of-range index must resolve to None"
    );
    assert_test!(
        virtio_blk::blk_device_count() >= 2,
        "at least the root-fs and scratch devices must be claimed"
    );
    pass!()
}

/// 8 KiB is two data descriptors in one chain — one request where the
/// pre-scatter-gather driver needed two.
pub fn test_virtio_blk_single_request_over_one_page() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    const SPAN: usize = 8192;
    let offset = 3072u64 * 512;
    let mut pattern = assert_ok!(KVec::<u8>::zeroed(SPAN), "pattern buffer");
    for (i, b) in pattern.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31) ^ (i >> 5)) as u8;
    }

    assert_test!(
        token.write_at(offset, &pattern).is_ok(),
        "an 8 KiB write must succeed as one scatter-gather chain"
    );

    let mut readback = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback buffer");
    assert_test!(
        token.read_at(offset, &mut readback).is_ok(),
        "an 8 KiB read must succeed as one scatter-gather chain"
    );
    assert_test!(
        readback[..] == pattern[..],
        "a span larger than one bounce page must round-trip byte for byte"
    );
    pass!()
}

/// `write_vectored` gathers non-adjacent kernel buffers into one contiguous
/// device extent, each segment at its own offset in the run.
pub fn test_virtio_blk_write_vectored_gathers_one_extent() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    const A: usize = 1024;
    const B: usize = 2048;
    const C: usize = 1536;
    let offset = 4608u64 * 512;

    let mut a = assert_ok!(KVec::<u8>::zeroed(A), "segment a");
    let mut b = assert_ok!(KVec::<u8>::zeroed(B), "segment b");
    let mut c = assert_ok!(KVec::<u8>::zeroed(C), "segment c");
    for (i, v) in a.iter_mut().enumerate() {
        *v = 0xA0u8.wrapping_add(i as u8);
    }
    for (i, v) in b.iter_mut().enumerate() {
        *v = 0xB0u8.wrapping_sub(i as u8);
    }
    for (i, v) in c.iter_mut().enumerate() {
        *v = (i as u8) ^ 0x5A;
    }

    // Three separate allocations: nothing about the run is contiguous in memory.
    let segs: [&[u8]; 3] = [&a, &b, &c];
    assert_test!(
        token.write_vectored(offset, &segs).is_ok(),
        "a vectored write of three segments must succeed"
    );

    let mut back_a = assert_ok!(KVec::<u8>::zeroed(A), "readback a");
    let mut back_b = assert_ok!(KVec::<u8>::zeroed(B), "readback b");
    let mut back_c = assert_ok!(KVec::<u8>::zeroed(C), "readback c");
    assert_test!(
        token.read_at(offset, &mut back_a).is_ok()
            && token.read_at(offset + A as u64, &mut back_b).is_ok()
            && token.read_at(offset + (A + B) as u64, &mut back_c).is_ok(),
        "per-segment readback must succeed"
    );
    assert_test!(
        back_a[..] == a[..],
        "segment 0 must land at the start of the extent"
    );
    assert_test!(
        back_b[..] == b[..],
        "segment 1 must land directly after segment 0"
    );
    assert_test!(
        back_c[..] == c[..],
        "segment 2 must land directly after segment 1"
    );
    pass!()
}

/// Two chains in flight at once, each completing with its own data:
/// `blk_submit_read` returns without parking, which the old global `io_lock`
/// made impossible.
pub fn test_virtio_blk_concurrent_requests_complete() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    const SPAN: usize = 4096;
    const SECTOR_ONE: u64 = 5120;
    const SECTOR_TWO: u64 = 5632;

    let mut first = assert_ok!(KVec::<u8>::zeroed(SPAN), "first pattern");
    let mut second = assert_ok!(KVec::<u8>::zeroed(SPAN), "second pattern");
    for (i, v) in first.iter_mut().enumerate() {
        *v = (i as u8) ^ 0x11;
    }
    for (i, v) in second.iter_mut().enumerate() {
        *v = (i as u8).wrapping_mul(3) ^ 0xEE;
    }
    assert_test!(
        token.write_at(SECTOR_ONE * 512, &first).is_ok()
            && token.write_at(SECTOR_TWO * 512, &second).is_ok(),
        "seeding both regions must succeed"
    );

    let in_flight_one = match virtio_blk::blk_submit_read(handle, SECTOR_ONE, SPAN) {
        Ok(r) => r,
        Err(e) => return fail!("first submission failed: {:?}", e),
    };
    let in_flight_two = match virtio_blk::blk_submit_read(handle, SECTOR_TWO, SPAN) {
        Ok(r) => r,
        Err(e) => return fail!("second submission while the first is in flight: {:?}", e),
    };

    let mut got_one = assert_ok!(KVec::<u8>::zeroed(SPAN), "first readback");
    let mut got_two = assert_ok!(KVec::<u8>::zeroed(SPAN), "second readback");
    if let Err(e) = in_flight_one.complete(&mut got_one) {
        return fail!("first concurrent request did not complete: {:?}", e);
    }
    if let Err(e) = in_flight_two.complete(&mut got_two) {
        return fail!("second concurrent request did not complete: {:?}", e);
    }

    assert_test!(
        got_one[..] == first[..],
        "the first concurrent request must return its own extent"
    );
    assert_test!(
        got_two[..] == second[..],
        "the second concurrent request must return its own extent"
    );
    pass!()
}

/// A span that runs past the end of the medium is a bounds error, not the
/// catch-all `InvalidBuffer` every failure used to collapse into.
pub fn test_virtio_blk_read_past_capacity_is_out_of_bounds() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let capacity = virtio_blk::blk_capacity(handle);
    assert_test!(capacity >= 512, "scratch device must report a capacity");

    let mut buf = [0u8; 512];
    match virtio_blk::blk_read(handle, capacity - 256, &mut buf) {
        Err(BlockDeviceError::OutOfBounds) => {}
        other => return fail!("want OutOfBounds for a span past capacity, got {:?}", other),
    }
    match virtio_blk::blk_read(handle, u64::MAX - 16, &mut buf) {
        Err(BlockDeviceError::OutOfBounds) => pass!(),
        other => fail!("want OutOfBounds for an overflowing span, got {:?}", other),
    }
}

/// A completion landing while the timeout epilogue allocates the slot's
/// replacement page set used to leave the slot in a state nothing reclaims:
/// one of four gone per event, then `Busy` forever.
///
/// A device that answers in microseconds cannot be made to complete inside a
/// real five-second window, so the hook enters the epilogue where the race
/// leaves it. The rounds run past `NUM_REQUEST_SLOTS` so a per-event leak
/// wedges the device inside the test rather than after it.
pub fn test_virtio_blk_late_completion_keeps_slot() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    const SPAN: usize = 1024;
    const SECTOR: u64 = 6144;

    let mut pattern = assert_ok!(KVec::<u8>::zeroed(SPAN), "pattern buffer");
    for (i, v) in pattern.iter_mut().enumerate() {
        *v = (i as u8).wrapping_mul(7) ^ 0x3C;
    }
    assert_test!(
        token.write_at(SECTOR * 512, &pattern).is_ok(),
        "seeding the region must succeed"
    );

    let slots = virtio_blk::blk_available_slots(handle);
    assert_test!(slots > 0, "the device must start with a free request slot");

    for round in 0..slots + 2 {
        let mut got = assert_ok!(KVec::<u8>::zeroed(SPAN), "readback buffer");
        if let Err(e) =
            virtio_blk::blk_read_completing_in_replacement_window(handle, SECTOR, &mut got)
        {
            return fail!(
                "round {}: a completion inside the replacement window must be handed to the \
                 caller, got {:?}",
                round,
                e
            );
        }
        assert_test!(
            got[..] == pattern[..],
            "the late completion must carry the chain's own payload"
        );
        assert_eq_test!(
            virtio_blk::blk_available_slots(handle),
            slots,
            "the slot must return to service after the epilogue"
        );
    }

    let mut after = assert_ok!(KVec::<u8>::zeroed(SPAN), "post-round readback");
    assert_test!(
        token.read_at(SECTOR * 512, &mut after).is_ok() && after[..] == pattern[..],
        "the device must still answer ordinary requests afterwards"
    );
    pass!()
}

const KILLED_SECTOR: u64 = 12288;
/// Enough writes that, abandoned rather than waited out, one would still be in
/// the device when its waiter gave up.
const KILLED_WRITES: u64 = 64;
static KILLED_PATTERN: [u8; 4096] = [0x6B; 4096];
const REFUSED_SECTOR: u64 = KILLED_SECTOR + KILLED_WRITES * 8;
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
    let Some(token) =
        virtio_blk::blk_device_by_index(SCRATCH).and_then(|h| virtio_blk::open_writer(h).ok())
    else {
        return KILLED_FAILED;
    };
    if token
        .write_at(REFUSED_SECTOR * 512, &REFUSED_OLDER)
        .is_err()
    {
        return KILLED_FAILED;
    }
    for i in 0..KILLED_WRITES {
        virtio_blk::blk_kill_after_next_submit();
        let wrote = token.write_at((KILLED_SECTOR + i * 8) * 512, &KILLED_PATTERN);
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
    let refused = token.write_at(REFUSED_SECTOR * 512, &REFUSED_NEWER);
    mark_current_killed(false);
    match refused {
        Err(BlockDeviceError::Interrupted) => KILLED_PASS,
        Ok(()) => KILLED_SUBMITTED,
        Err(_) => KILLED_FAILED,
    }
}

/// A requester killed with a write in the device waits it out, and one killed
/// before a write reaches the device sends nothing. Abandoned, a write could
/// land after a later one to the same sectors — which is how a directory block
/// came back as it was before an `unlink`.
pub fn test_virtio_blk_killed_write_is_waited_out() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };

    KILLED_OUTCOME.store(KILLED_PENDING, Ordering::Release);
    if slopos_ostd::task::spawn("killed-writer", killed_writer, TaskPriority::Normal).is_err() {
        return fail!("could not spawn the writer thread");
    }
    let finished = crate::virtio::hpet_poll_wait(
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
        virtio_blk::blk_quarantine_count(handle),
        0,
        "a waited-out write must leave no chain quarantined"
    );
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };
    let mut readback = assert_ok!(KVec::<u8>::zeroed(KILLED_PATTERN.len()), "readback buffer");
    let last = (KILLED_SECTOR + (KILLED_WRITES - 1) * 8) * 512;
    assert_test!(
        token.read_at(last, &mut readback).is_ok() && readback[..] == KILLED_PATTERN[..],
        "the killed requester's writes must be on the device"
    );
    assert_test!(
        token
            .read_at(REFUSED_SECTOR * 512, &mut readback[..512])
            .is_ok()
            && readback[..512] == REFUSED_OLDER[..],
        "the refused write must not be on the device"
    );
    pass!()
}

const FENCED_SECTOR: u64 = 7176;
const FENCED_OLDER: [u8; 512] = [0x3C; 512];
const FENCED_NEWER: [u8; 512] = [0xC3; 512];
/// The writer's span: inside one sector, so the write is a read-modify-write.
const FENCED_SPAN: core::ops::Range<usize> = 128..384;
/// Long enough for an unfenced write to have landed.
const FENCE_HOLD_MS: u32 = 100;

const FENCED_PENDING: u8 = 0;
const FENCED_AFTER_RETURN: u8 = 1;
const FENCED_BEFORE_RETURN: u8 = 2;
const FENCED_INTERRUPTED: u8 = 3;
const FENCED_FAILED: u8 = 4;

static FENCE_HEAD: AtomicU32 = AtomicU32::new(u32::MAX);
static FENCE_RETURNED: AtomicBool = AtomicBool::new(false);
static FENCED_STARTED: AtomicBool = AtomicBool::new(false);
static FENCED_OUTCOME: AtomicU8 = AtomicU8::new(FENCED_PENDING);

/// Hand the staged write back to the device model. Whoever swaps the head out
/// owns it, so it is returned exactly once on every path.
fn return_abandoned_write() {
    let head = FENCE_HEAD.swap(u32::MAX, Ordering::AcqRel);
    if head == u32::MAX {
        return;
    }
    FENCE_RETURNED.store(true, Ordering::Release);
    if let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) {
        virtio_blk::blk_return_abandoned_write(handle, head as u16);
    }
}

/// A kernel thread, so the write parks on the fence rather than polling it.
fn fenced_writer() {
    let outcome = match virtio_blk::blk_device_by_index(SCRATCH)
        .and_then(|h| virtio_blk::open_writer(h).ok())
    {
        Some(token) => {
            FENCED_STARTED.store(true, Ordering::Release);
            let offset = FENCED_SECTOR * 512 + FENCED_SPAN.start as u64;
            match token.write_at(offset, &FENCED_NEWER[FENCED_SPAN]) {
                Ok(()) if FENCE_RETURNED.load(Ordering::Acquire) => FENCED_AFTER_RETURN,
                Ok(()) => FENCED_BEFORE_RETURN,
                Err(BlockDeviceError::Interrupted) => FENCED_INTERRUPTED,
                Err(_) => FENCED_FAILED,
            }
        }
        None => FENCED_FAILED,
    };
    FENCED_OUTCOME.store(outcome, Ordering::Release);
}

/// Run [`fenced_writer`] against a freshly staged abandoned write, `kill` it
/// once it is waiting if asked, then return the abandoned write.
fn fenced_write(kill: bool) -> Result<u8, &'static str> {
    let handle = virtio_blk::blk_device_by_index(SCRATCH).ok_or("scratch device absent")?;
    let head = virtio_blk::blk_stage_abandoned_write(handle).ok_or("could not stage")?;
    FENCE_RETURNED.store(false, Ordering::Release);
    FENCED_STARTED.store(false, Ordering::Release);
    FENCED_OUTCOME.store(FENCED_PENDING, Ordering::Release);
    FENCE_HEAD.store(u32::from(head), Ordering::Release);

    let writer = slopos_ostd::task::spawn("fenced-writer", fenced_writer, TaskPriority::Normal);
    let started = writer.is_ok()
        && crate::virtio::hpet_poll_wait(&|| FENCED_STARTED.load(Ordering::Acquire), 10_000);
    crate::virtio::hpet_poll_wait(&|| false, FENCE_HOLD_MS);
    let mut readback = [0u8; 100];
    let read = virtio_blk::blk_read(handle, FENCED_SECTOR * 512 + 1, &mut readback);
    if kill && let Ok(id) = writer {
        kill_task(id.as_u32());
    }
    return_abandoned_write();

    if !started {
        return Err("the writer thread never started");
    }
    if read.is_err() {
        return Err("a read waited behind an abandoned write");
    }
    if !crate::virtio::hpet_poll_wait(
        &|| FENCED_OUTCOME.load(Ordering::Acquire) != FENCED_PENDING,
        10_000,
    ) {
        return Err("the fenced writer never finished");
    }
    Ok(FENCED_OUTCOME.load(Ordering::Acquire))
}

/// A write a timeout abandoned may still land, so a later write waits until
/// the device has returned it — a sub-sector one before it reads the sector it
/// will write back — a killed one gives up without sending, and a read, which
/// cannot reorder the medium, does not wait at all.
pub fn test_virtio_blk_waits_out_an_abandoned_write() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    {
        let token = match virtio_blk::open_writer(handle) {
            Ok(t) => t,
            Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
        };
        assert_test!(
            token.write_at(FENCED_SECTOR * 512, &FENCED_OLDER).is_ok(),
            "seeding the sector must succeed"
        );
    }

    let _ = virtio_blk::blk_take_rmw_read_past_fence();
    match fenced_write(true) {
        Ok(FENCED_INTERRUPTED) => {}
        Ok(FENCED_AFTER_RETURN | FENCED_BEFORE_RETURN) => {
            return fail!("a writer killed behind the fence still sent its write");
        }
        Ok(_) => return fail!("the killed fenced writer failed"),
        Err(msg) => return fail!("{}", msg),
    }
    let mut readback = [0u8; 512];
    assert_test!(
        virtio_blk::blk_read(handle, FENCED_SECTOR * 512, &mut readback).is_ok()
            && readback == FENCED_OLDER,
        "the killed writer's write must not be on the device"
    );

    match fenced_write(false) {
        Ok(FENCED_AFTER_RETURN) => {}
        Ok(FENCED_BEFORE_RETURN) => {
            return fail!("a write reached the device while an earlier one could still land");
        }
        Ok(_) => return fail!("the fenced write failed once the abandoned one was returned"),
        Err(msg) => return fail!("{}", msg),
    }
    assert_test!(
        !virtio_blk::blk_take_rmw_read_past_fence(),
        "a read-modify-write read its sector while an abandoned write could still land"
    );
    let mut expected = FENCED_OLDER;
    expected[FENCED_SPAN].copy_from_slice(&FENCED_NEWER[FENCED_SPAN]);
    assert_test!(
        virtio_blk::blk_read(handle, FENCED_SECTOR * 512, &mut readback).is_ok()
            && readback == expected,
        "the later write must be what the sector holds"
    );
    pass!()
}

/// One count is one device request: the counters live inside the chain loop,
/// so a span wider than 32 KiB costs more than one write and a sub-sector
/// write pays for its read-modify-write pair. Counted at the `BlockDevice`
/// boundary instead, both cost exactly one — which is what made the graded
/// request count blind to a block-layer regression.
pub fn test_virtio_blk_counters_are_per_device_request() -> TestResult {
    let Some(handle) = virtio_blk::blk_device_by_index(SCRATCH) else {
        return fail!("scratch block device (disk1) not present");
    };
    let token = match virtio_blk::open_writer(handle) {
        Ok(t) => t,
        Err(e) => return fail!("open_writer(scratch) failed: {:?}", e),
    };

    // Sector-aligned, so the chain split is the only thing the counter sees.
    const WIDE: usize = 64 * 1024;
    const SECTOR: u64 = 6144;
    let wide = assert_ok!(KVec::<u8>::zeroed(WIDE), "wide payload");

    // Deltas rather than absolutes, and lower bounds rather than equalities:
    // the counters are process-global and the ext2 flusher writes to another
    // device while this runs.
    let before = stats::snapshot();
    assert_test!(
        token.write_at(SECTOR * 512, &wide).is_ok(),
        "the two-chain write must succeed"
    );
    let after = stats::snapshot();
    assert_test!(
        after.write_requests - before.write_requests >= 2,
        "a {}-byte span is more than one 32 KiB chain, so it must count more \
         than one device write",
        WIDE
    );
    assert_test!(
        after.blocks_written - before.blocks_written >= (WIDE / 512) as u64,
        "the sector total must still be the bytes the request carried"
    );

    let before = stats::snapshot();
    assert_test!(
        token.write_at(SECTOR * 512 + 3, &wide[..8]).is_ok(),
        "the sub-sector write must succeed"
    );
    let after = stats::snapshot();
    assert_test!(
        after.read_requests - before.read_requests >= 1
            && after.write_requests - before.write_requests >= 1,
        "a write inside one sector is a read-modify-write pair at the device, \
         and both halves must be counted"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_edge_event_new_not_signaled,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_signal_then_consume,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_double_consume,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_reset_clears_signal,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_signal_after_reset,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_multiple_signals,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_wait_presignaled,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_edge_event_wait_timeout,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_lock_unlock,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_try_lock_contention,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_sleep_mutex_relock_after_try,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtqueue_unready_alloc_none,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_hpet_period_fs_nonzero,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_hpet_period_fs_matches_full_name,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_read_interrupt_driven,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_consecutive_reads,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_write_readback_interrupt_driven,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_multisector_write_readback,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_flush_completes,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_block_device_exclusive_write_claim,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_block_device_lookup_bounds,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_single_request_over_one_page,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_write_vectored_gathers_one_extent,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_concurrent_requests_complete,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_read_past_capacity_is_out_of_bounds,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_late_completion_keeps_slot,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_counters_are_per_device_request,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_killed_write_is_waited_out,
    suite = virtio_completion
);
slopos_testing::stest!(
    name = test_virtio_blk_waits_out_an_abandoned_write,
    suite = virtio_completion
);
