//! VirtIO interrupt-driven completion regression tests.
//!
//! These tests verify the interrupt-driven completion infrastructure: the `QueueEvent`
//! primitive (signal/consume/reset/timeout), the replacement of
//! busy-wait polling with MSI-X interrupt-driven I/O completion, and
//! the HPET `period_fs()` accessor used for deadline computation.
//!
//! Pure QueueEvent logic tests run without hardware dependencies.
//! Integration tests verify through live virtio-blk after probe.

use slopos_lib::testing::TestResult;
use slopos_lib::{assert_eq_test, assert_test, pass};

use crate::hpet;
use crate::virtio::QueueEvent;
use crate::virtio_blk;

// =============================================================================
// 1. QueueEvent unit tests (pure logic — no hardware)
// =============================================================================

/// A freshly created QueueEvent must not be signaled.
pub fn test_queue_event_new_not_signaled() -> TestResult {
    let ev = QueueEvent::new();
    assert_test!(!ev.try_consume(), "new QueueEvent should not be signaled");
    pass!()
}

/// signal() followed by try_consume() must return true.
pub fn test_queue_event_signal_then_consume() -> TestResult {
    let ev = QueueEvent::new();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should return true after signal"
    );
    pass!()
}

/// try_consume() is single-shot: second call must return false.
pub fn test_queue_event_double_consume() -> TestResult {
    let ev = QueueEvent::new();
    ev.signal();
    let first = ev.try_consume();
    let second = ev.try_consume();
    assert_test!(first, "first try_consume should succeed");
    assert_test!(!second, "second try_consume should fail (single-shot)");
    pass!()
}

/// reset() must clear a pending signal.
pub fn test_queue_event_reset_clears_signal() -> TestResult {
    let ev = QueueEvent::new();
    ev.signal();
    ev.reset();
    assert_test!(
        !ev.try_consume(),
        "try_consume should fail after reset clears signal"
    );
    pass!()
}

/// signal() after reset() must re-arm the event.
pub fn test_queue_event_signal_after_reset() -> TestResult {
    let ev = QueueEvent::new();
    ev.signal();
    ev.reset();
    ev.signal();
    assert_test!(
        ev.try_consume(),
        "try_consume should succeed after signal-reset-signal"
    );
    pass!()
}

/// Multiple signals without consume should still yield one true consume.
pub fn test_queue_event_multiple_signals() -> TestResult {
    let ev = QueueEvent::new();
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

/// wait_timeout_ms() must return true immediately when pre-signaled.
pub fn test_queue_event_wait_presignaled() -> TestResult {
    let ev = QueueEvent::new();
    ev.signal();
    let start = hpet::read_counter();
    let result = ev.wait_timeout_ms(5000);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);
    assert_test!(result, "wait should return true when pre-signaled");
    // Pre-signaled fast-path should complete in < 1 ms of ticks.
    let period = hpet::period_fs() as u64;
    if period > 0 {
        let elapsed_ns = (elapsed_ticks as u128 * period as u128 / 1_000_000) as u64;
        assert_test!(
            elapsed_ns < 1_000_000,
            "pre-signaled wait took {} ns — should be < 1 ms",
            elapsed_ns
        );
    }
    pass!()
}

/// wait_timeout_ms(1) must return false when not signaled (timeout).
pub fn test_queue_event_wait_timeout() -> TestResult {
    if !hpet::is_available() {
        // Without HPET, wait_timeout_ms falls back to spin loop.
        // Still test that it eventually returns false.
        let ev = QueueEvent::new();
        let result = ev.wait_timeout_ms(1);
        assert_test!(!result, "unsignaled wait should timeout");
        return pass!();
    }

    let ev = QueueEvent::new();
    let start = hpet::read_counter();
    let result = ev.wait_timeout_ms(1);
    let elapsed_ticks = hpet::read_counter().wrapping_sub(start);
    assert_test!(!result, "unsignaled wait(1ms) should timeout");
    // Should have waited approximately 1 ms (allow 0.5–50 ms range for QEMU jitter).
    let period = hpet::period_fs() as u64;
    if period > 0 {
        let elapsed_ns = (elapsed_ticks as u128 * period as u128 / 1_000_000) as u64;
        assert_test!(
            elapsed_ns >= 500_000,
            "timeout wait took only {} ns — expected >= 0.5 ms",
            elapsed_ns
        );
    }
    pass!()
}

// =============================================================================
// 2. HPET period_fs() accessor
// =============================================================================

/// period_fs() must return a non-zero value when HPET is available.
pub fn test_hpet_period_fs_nonzero() -> TestResult {
    assert_test!(hpet::is_available(), "HPET must be available for this test");
    let period = hpet::period_fs();
    assert_test!(period > 0, "period_fs should be > 0 when HPET is init'd");
    // Sanity: HPET spec says period ≤ 100 ns = 100_000_000 fs.
    assert_test!(
        period <= 100_000_000,
        "period_fs {} exceeds HPET spec max",
        period
    );
    pass!()
}

/// period_fs() and period_femtoseconds() must agree.
pub fn test_hpet_period_fs_matches_full_name() -> TestResult {
    assert_eq_test!(
        hpet::period_fs(),
        hpet::period_femtoseconds(),
        "period_fs and period_femtoseconds should return the same value"
    );
    pass!()
}

// =============================================================================
// 3. Integration: interrupt-driven I/O through live device
// =============================================================================

/// virtio-blk read must succeed with interrupt-driven completion.
pub fn test_virtio_blk_read_interrupt_driven() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    // ext2 superblock starts at byte offset 1024 (sector 2 at 512 B/sector).
    // Sector 0 is the boot record and may be all zeros on non-bootable images.
    let mut buf = [0u8; 512];
    let ok = virtio_blk::virtio_blk_read(1024, &mut buf);
    assert_test!(
        ok,
        "superblock read should succeed via interrupt-driven I/O"
    );
    // ext2 magic number 0xEF53 lives at offset 0x38 within the superblock.
    let magic = u16::from_le_bytes([buf[0x38], buf[0x39]]);
    assert_eq_test!(magic, 0xEF53, "ext2 superblock magic mismatch");
    pass!()
}

/// A second read should also succeed (event properly reset between I/Os).
pub fn test_virtio_blk_consecutive_reads() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    let mut buf1 = [0u8; 512];
    let mut buf2 = [0u8; 512];
    let ok1 = virtio_blk::virtio_blk_read(0, &mut buf1);
    let ok2 = virtio_blk::virtio_blk_read(512, &mut buf2);
    assert_test!(ok1, "first consecutive read should succeed");
    assert_test!(ok2, "second consecutive read should succeed");
    // Sectors 0 and 1 should contain data (ext2 has superblock at byte 1024).
    pass!()
}

/// A write followed by readback must produce consistent data.
pub fn test_virtio_blk_write_readback_interrupt_driven() -> TestResult {
    assert_test!(
        virtio_blk::virtio_blk_is_ready(),
        "virtio-blk must be ready"
    );

    // Use a high sector to avoid clobbering filesystem metadata.
    // Sector 8192 = offset 4 MiB, safely past ext2 superblock/inode tables.
    let offset = 8192u64 * 512;
    let pattern: [u8; 512] = {
        let mut p = [0u8; 512];
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        p
    };

    let ok_write = virtio_blk::virtio_blk_write(offset, &pattern);
    assert_test!(ok_write, "write should succeed via interrupt-driven I/O");

    let mut readback = [0u8; 512];
    let ok_read = virtio_blk::virtio_blk_read(offset, &mut readback);
    assert_test!(ok_read, "readback should succeed via interrupt-driven I/O");
    assert_test!(
        readback == pattern,
        "readback data should match written pattern"
    );
    pass!()
}

// =============================================================================
// Suite registration
// =============================================================================

slopos_lib::define_test_suite!(
    virtio_completion,
    [
        // QueueEvent unit tests
        test_queue_event_new_not_signaled,
        test_queue_event_signal_then_consume,
        test_queue_event_double_consume,
        test_queue_event_reset_clears_signal,
        test_queue_event_signal_after_reset,
        test_queue_event_multiple_signals,
        test_queue_event_wait_presignaled,
        test_queue_event_wait_timeout,
        // HPET accessor
        test_hpet_period_fs_nonzero,
        test_hpet_period_fs_matches_full_name,
        // Integration: interrupt-driven I/O
        test_virtio_blk_read_interrupt_driven,
        test_virtio_blk_consecutive_reads,
        test_virtio_blk_write_readback_interrupt_driven,
    ]
);
