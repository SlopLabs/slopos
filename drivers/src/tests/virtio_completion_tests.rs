//! VirtIO completion primitive regression tests: `IrqEdgeEvent`, the sleeping
//! `Mutex`, virtqueue descriptor free-list invariants and HPET `period_fs()`.

use slopos_ostd::lock_class;
use slopos_ostd::sync::lock_tracking::LOCK_LEVEL_RESOURCE;
use slopos_testing::TestResult;
use slopos_testing::{assert_eq_test, assert_test, fail, pass};

use slopos_ostd::sync::Mutex;

use crate::hpet;
use crate::virtio::queue::Virtqueue;
use crate::virtio::{EdgeWait, IrqEdgeEvent};

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
