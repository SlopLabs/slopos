use core::sync::atomic::{AtomicU32, Ordering};

/// Per-NIC NAPI instrumentation: budget cap + processed counter.
///
/// No Idle/Scheduled/Polling state machine: a single IRQ producer and a single
/// kthread consumer parked on the waker's `armed` flag make it redundant.
pub struct NapiContext {
    budget: u32,
    processed: AtomicU32,
}

impl NapiContext {
    pub const fn new(budget: u32) -> Self {
        Self {
            budget,
            processed: AtomicU32::new(0),
        }
    }

    #[inline]
    pub fn budget(&self) -> u32 {
        self.budget
    }

    #[inline]
    pub fn processed(&self) -> u32 {
        self.processed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn add_processed(&self, count: u32) {
        self.processed.fetch_add(count, Ordering::Relaxed);
    }
}

// The wait-predicate purity gate (`scripts/check_wait_predicate_purity.sh`)
// forbids calling the NAPI dispatch function inside a
// `wait_event{,_timeout,_until}` closure — predicates must observe state, not
// side-effect.
//
// There is deliberately no synchronous-drain counterpart to `wake_napi`: a
// caller that drains on its own behalf is compensating for the netpoll kthread
// not running.

/// IRQ-safe: wake the netpoll kthread. Does not poll synchronously — the
/// kthread drains when scheduled.
#[inline]
pub fn wake_napi() {
    crate::nic::NAPI_WAKER.arm_and_wake();
}

/// Drain the software devices — today just `lo` — through the ordinary ingress
/// pipeline, returning the packet count.
pub fn poll_software_devices(budget: usize) -> u32 {
    let Some(handle) = crate::loopback::handle() else {
        return 0;
    };
    let packets = handle.poll_rx(budget, &crate::pool::PACKET_POOL);
    let count = packets.len() as u32;
    for pkt in packets {
        crate::ingress::net_rx(handle, pkt);
    }
    count
}
