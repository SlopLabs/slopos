//! The `FSPERF[<phase>]` / `FSCAP[<phase>]` / `FSIMG[<phase>]` /
//! `FSACT[<phase>]` report lines `run_tests gate fsperf` parses.
//!
//! Emitted at a phase boundary rather than from inside the measuring test: at
//! the default `tests.verbosity=summary` a passing test's klog capture is
//! never put on the wire, so a report written from inside one would be
//! invisible to exactly the CI capture that grades it.
//!
//! The recording half is test-only and the emitting half is not, so the boot
//! phase report can call it unconditionally.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// What a write cost, and what the same bytes cost at the device underneath.
#[derive(Debug, Copy, Clone, Default)]
pub struct WriteCost {
    pub bytes: u64,
    pub txns: u64,
    pub commits: u64,
    pub devwrites: u64,
    pub devblocks: u64,
    pub barriers: u64,
    pub ns: u64,
    pub rawbytes: u64,
    pub rawns: u64,
}

/// What a volume two orders of magnitude past the appliance root costs to
/// mount, to search and to write.
#[derive(Debug, Copy, Clone, Default)]
pub struct CapacityCost {
    pub blocks: u64,
    pub blocksize: u32,
    pub groups: u32,
    pub cacheentries: u64,
    pub mountreads: u64,
    pub mountns: u64,
    pub dirents: u64,
    pub lookupreads: u64,
    pub bytes: u64,
    pub ns: u64,
    pub files: u64,
    pub treebytes: u64,
}

/// One file's pages faulted in from the root and from an image of it, each
/// timed cold and again warm, in nanoseconds.
#[derive(Debug, Copy, Clone, Default)]
pub struct ImageCost {
    pub bytes: u64,
    pub coldroot: u64,
    pub coldimage: u64,
    pub warmroot: u64,
    pub warmimage: u64,
}

/// What showing many small images cost: the kernel heap they hold against
/// the activations' budget, the pages allocated meanwhile, the reads of
/// their image file and the time, all of them together.
#[derive(Debug, Copy, Clone, Default)]
pub struct ActivationCost {
    pub images: u64,
    pub heldbytes: u64,
    pub pages: u64,
    pub reads: u64,
    pub ns: u64,
}

const WRITE_FIELDS: usize = 9;
const CAP_FIELDS: usize = 12;
const IMAGE_FIELDS: usize = 5;
const ACTIVATION_FIELDS: usize = 5;

static WRITE_RECORDED: AtomicBool = AtomicBool::new(false);
static WRITE_SLOTS: [AtomicU64; WRITE_FIELDS] = [const { AtomicU64::new(0) }; WRITE_FIELDS];
static CAP_RECORDED: AtomicBool = AtomicBool::new(false);
static CAP_SLOTS: [AtomicU64; CAP_FIELDS] = [const { AtomicU64::new(0) }; CAP_FIELDS];
static IMAGE_RECORDED: AtomicBool = AtomicBool::new(false);
static IMAGE_SLOTS: [AtomicU64; IMAGE_FIELDS] = [const { AtomicU64::new(0) }; IMAGE_FIELDS];
static ACTIVATION_RECORDED: AtomicBool = AtomicBool::new(false);
static ACTIVATION_SLOTS: [AtomicU64; ACTIVATION_FIELDS] =
    [const { AtomicU64::new(0) }; ACTIVATION_FIELDS];

#[cfg(feature = "tests")]
pub fn record_activation_cost(cost: &ActivationCost) {
    let values = [cost.images, cost.heldbytes, cost.pages, cost.reads, cost.ns];
    for (slot, value) in ACTIVATION_SLOTS.iter().zip(values) {
        slot.store(value, Ordering::Relaxed);
    }
    ACTIVATION_RECORDED.store(true, Ordering::Release);
}

#[cfg(feature = "tests")]
pub fn record_image_cost(cost: &ImageCost) {
    let values = [
        cost.bytes,
        cost.coldroot,
        cost.coldimage,
        cost.warmroot,
        cost.warmimage,
    ];
    for (slot, value) in IMAGE_SLOTS.iter().zip(values) {
        slot.store(value, Ordering::Relaxed);
    }
    IMAGE_RECORDED.store(true, Ordering::Release);
}

#[cfg(feature = "tests")]
pub fn record_write_cost(cost: &WriteCost) {
    let values = [
        cost.bytes,
        cost.txns,
        cost.commits,
        cost.devwrites,
        cost.devblocks,
        cost.barriers,
        cost.ns,
        cost.rawbytes,
        cost.rawns,
    ];
    for (slot, value) in WRITE_SLOTS.iter().zip(values) {
        slot.store(value, Ordering::Relaxed);
    }
    WRITE_RECORDED.store(true, Ordering::Release);
}

#[cfg(feature = "tests")]
pub fn record_capacity_cost(cost: &CapacityCost) {
    let values = [
        cost.blocks,
        u64::from(cost.blocksize),
        u64::from(cost.groups),
        cost.cacheentries,
        cost.mountreads,
        cost.mountns,
        cost.dirents,
        cost.lookupreads,
        cost.bytes,
        cost.ns,
        cost.files,
        cost.treebytes,
    ];
    for (slot, value) in CAP_SLOTS.iter().zip(values) {
        slot.store(value, Ordering::Relaxed);
    }
    CAP_RECORDED.store(true, Ordering::Release);
}

/// Put whatever was measured on the wire. Silent when nothing was — a kernel
/// with no tests in it, or a run that never reached the measurement.
pub fn fs_cost_report(phase: &str) {
    if WRITE_RECORDED.load(Ordering::Acquire) {
        let v = |i: usize| WRITE_SLOTS[i].load(Ordering::Relaxed);
        slopos_ostd::klog_info!(
            "FSPERF[{}]: bytes={} txns={} commits={} devwrites={} devblocks={} barriers={} ns={} rawbytes={} rawns={}",
            phase,
            v(0),
            v(1),
            v(2),
            v(3),
            v(4),
            v(5),
            v(6),
            v(7),
            v(8)
        );
    }
    if CAP_RECORDED.load(Ordering::Acquire) {
        let v = |i: usize| CAP_SLOTS[i].load(Ordering::Relaxed);
        slopos_ostd::klog_info!(
            "FSCAP[{}]: blocks={} blocksize={} groups={} cacheentries={} mountreads={} mountns={} dirents={} lookupreads={} bytes={} ns={} files={} treebytes={}",
            phase,
            v(0),
            v(1),
            v(2),
            v(3),
            v(4),
            v(5),
            v(6),
            v(7),
            v(8),
            v(9),
            v(10),
            v(11)
        );
    }
    if IMAGE_RECORDED.load(Ordering::Acquire) {
        let v = |i: usize| IMAGE_SLOTS[i].load(Ordering::Relaxed);
        slopos_ostd::klog_info!(
            "FSIMG[{}]: bytes={} coldroot={} coldimage={} warmroot={} warmimage={}",
            phase,
            v(0),
            v(1),
            v(2),
            v(3),
            v(4)
        );
    }
    if ACTIVATION_RECORDED.load(Ordering::Acquire) {
        let v = |i: usize| ACTIVATION_SLOTS[i].load(Ordering::Relaxed);
        slopos_ostd::klog_info!(
            "FSACT[{}]: images={} heldbytes={} pages={} reads={} ns={}",
            phase,
            v(0),
            v(1),
            v(2),
            v(3),
            v(4)
        );
    }
}
