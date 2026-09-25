//! Where a busy machine's time goes, sampled: what every CPU was doing at each
//! timer tick, how long each spent halted, and — whenever a CPU goes idle —
//! where every blocked user task is parked.
//!
//! Off unless the command line says `prof=on`; `report` prints `PROF[...]`
//! lines. Every table is fixed-size and lock-free, because the tick half runs
//! in the timer ISR and the park-site half from the idle loop.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use slopos_abi::task::BlockReason;
use slopos_arch::MAX_CPUS;
use slopos_arch::tsc::rdtsc;
use slopos_ostd::klog_info;
use slopos_ostd::stacktrace::{StacktraceEntry, stacktrace_capture_from};
use slopos_ostd::string::bytes_as_str;

use crate::task::{TASK_FLAG_USER_MODE, TaskStatus, task_for_each_active};
use crate::task_struct::{Current, Idle};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STARTED_TSC: AtomicU64 = AtomicU64::new(0);
/// The first TSC and millisecond clock readings taken once the clock runs,
/// which is after the command line enables profiling: what converts cycles
/// to time.
static CLOCK_TSC: AtomicU64 = AtomicU64::new(0);
static CLOCK_MS: AtomicU64 = AtomicU64::new(0);

pub fn enable() {
    STARTED_TSC.store(rdtsc(), Ordering::Relaxed);
    ENABLED.store(true, Ordering::Release);
}

/// Take the clock anchor on the first reading past zero.
fn anchor_clock(now_ms: u64) {
    if now_ms != 0 && CLOCK_MS.load(Ordering::Relaxed) == 0 {
        CLOCK_TSC.store(rdtsc(), Ordering::Relaxed);
        CLOCK_MS.store(now_ms, Ordering::Release);
    }
}

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

struct CpuTime {
    user: AtomicU64,
    kernel: AtomicU64,
    idle: AtomicU64,
    halted_cycles: AtomicU64,
    /// TSC at the open halt, zero when none is open.
    halt_began: AtomicU64,
}

static CPUS: [CpuTime; MAX_CPUS] = [const {
    CpuTime {
        user: AtomicU64::new(0),
        kernel: AtomicU64::new(0),
        idle: AtomicU64::new(0),
        halted_cycles: AtomicU64::new(0),
        halt_began: AtomicU64::new(0),
    }
}; MAX_CPUS];

/// An open-addressed counter table keyed by a non-zero `u64`, safe to bump
/// from an ISR on any CPU: a slot's key is claimed once by CAS and never
/// released.
struct Histogram<const N: usize> {
    keys: [AtomicU64; N],
    counts: [AtomicU32; N],
    dropped: AtomicU64,
}

const PROBES: usize = 32;

impl<const N: usize> Histogram<N> {
    const fn new() -> Self {
        Self {
            keys: [const { AtomicU64::new(0) }; N],
            counts: [const { AtomicU32::new(0) }; N],
            dropped: AtomicU64::new(0),
        }
    }

    /// The slot holding `key`, claiming a free one if it has none.
    fn slot(&self, key: u64) -> Option<usize> {
        let start = (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize % N;
        for i in 0..PROBES.min(N) {
            let slot = (start + i) % N;
            let held = self.keys[slot].load(Ordering::Relaxed);
            if held == key {
                return Some(slot);
            }
            if held == 0 {
                match self.keys[slot].compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed)
                {
                    Ok(_) => return Some(slot),
                    Err(now) if now == key => return Some(slot),
                    Err(_) => {}
                }
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn bump(&self, key: u64) {
        if let Some(slot) = self.slot(key) {
            self.counts[slot].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Hand `f` the `k` largest entries, largest first, one scan per entry,
    /// so a report needs no array on its stack.
    fn for_each_top(&self, k: usize, mut f: impl FnMut(u64, u32)) {
        let mut bound = (u32::MAX, u64::MAX);
        for _ in 0..k {
            let mut best = (0u32, 0u64);
            for slot in 0..N {
                let entry = (
                    self.counts[slot].load(Ordering::Relaxed),
                    self.keys[slot].load(Ordering::Relaxed),
                );
                if entry.0 != 0 && entry.1 != 0 && entry < bound && entry > best {
                    best = entry;
                }
            }
            if best.0 == 0 {
                return;
            }
            f(best.1, best.0);
            bound = best;
        }
    }
}

/// Kernel RIPs of non-idle kernel ticks.
static KERNEL_RIPS: Histogram<4096> = Histogram::new();
/// User ticks by the first eight bytes of the running task's name.
static USER_TASKS: Histogram<256> = Histogram::new();

/// Frames recorded per park site: deep enough to get past the blocking
/// primitive and the syscall it serves to the operation that waited.
const CHAIN_DEPTH: usize = 24;
const CHAINS: usize = 512;

/// Park sites of blocked user tasks, keyed by a hash of the chain.
static CHAIN_HITS: Histogram<CHAINS> = Histogram::new();
static CHAIN_FRAMES: [[AtomicU64; CHAIN_DEPTH]; CHAINS] =
    [const { [const { AtomicU64::new(0) }; CHAIN_DEPTH] }; CHAINS];

/// One park-site sampler at a time; the chain frames have a single writer.
static SAMPLING: AtomicBool = AtomicBool::new(false);
static LAST_SAMPLE_MS: AtomicU64 = AtomicU64::new(0);
const SAMPLE_SPACING_MS: u64 = 10;

static SAMPLES: AtomicU64 = AtomicU64::new(0);
/// User tasks seen Ready but not on a CPU, summed over samples: a queue that
/// stays long while a CPU idles is a placement problem, not a wait.
static READY_WAITING: AtomicU64 = AtomicU64::new(0);
static BLOCKED_BY_REASON: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// Syscall numbers accounted individually; the private range starts at 1024.
const SYSCALLS: usize = 2048;
static SYSCALL_CALLS: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];
static SYSCALL_CYCLES: [AtomicU64; SYSCALLS] = [const { AtomicU64::new(0) }; SYSCALLS];

/// What a user page fault turned out to need.
#[derive(Clone, Copy)]
pub enum FaultKind {
    /// Resolved without I/O: zero fill, copy on write, a page already present.
    Inline,
    /// Populated from a file.
    File,
}

static FAULT_CALLS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static FAULT_CYCLES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The TSC now, or zero when profiling is off: what a timed section starts
/// from.
#[inline]
pub fn stamp() -> u64 {
    if enabled() { rdtsc() } else { 0 }
}

/// One syscall, blocking included, from its [`stamp`].
#[inline]
pub fn note_syscall(sysno: u64, began: u64) {
    if began == 0 {
        return;
    }
    let Some(calls) = SYSCALL_CALLS.get(sysno as usize) else {
        return;
    };
    calls.fetch_add(1, Ordering::Relaxed);
    SYSCALL_CYCLES[sysno as usize].fetch_add(rdtsc().saturating_sub(began), Ordering::Relaxed);
}

/// One user page fault, from its [`stamp`].
#[inline]
pub fn note_fault(kind: FaultKind, began: u64) {
    if began == 0 {
        return;
    }
    FAULT_CALLS[kind as usize].fetch_add(1, Ordering::Relaxed);
    FAULT_CYCLES[kind as usize].fetch_add(rdtsc().saturating_sub(began), Ordering::Relaxed);
}

fn name_key(name: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name.len())
        .min(8);
    bytes[..n].copy_from_slice(&name[..n]);
    u64::from_le_bytes(bytes) | 1 << 63
}

/// Classify one timer tick on this CPU.
pub fn note_tick(rip: u64, cs: u64) {
    if !enabled() {
        return;
    }
    let cpu = slopos_arch::pcr::get_current_cpu();
    let Some(time) = CPUS.get(cpu) else {
        return;
    };
    if cs & 3 == 3 {
        time.user.fetch_add(1, Ordering::Relaxed);
        if let Some(current) = Current::get() {
            USER_TASKS.bump(name_key(&current.task().name));
        }
        return;
    }
    let idle = match (Current::get(), Idle::current()) {
        (Some(current), Some(idle)) => current.addr() == idle.addr(),
        _ => false,
    };
    if idle {
        time.idle.fetch_add(1, Ordering::Relaxed);
        return;
    }
    time.kernel.fetch_add(1, Ordering::Relaxed);
    KERNEL_RIPS.bump(rip);
}

/// This CPU is about to halt.
#[inline]
pub fn halt_begin(cpu: usize) {
    if !enabled() {
        return;
    }
    if let Some(time) = CPUS.get(cpu) {
        time.halt_began.store(rdtsc(), Ordering::Relaxed);
    }
}

/// This CPU's halt is over: it is back in the idle loop, or dispatching from
/// the trap exit of the interrupt that woke it. Whichever comes first closes
/// the interval.
#[inline]
pub fn halt_end(cpu: usize) {
    if !enabled() {
        return;
    }
    let Some(time) = CPUS.get(cpu) else {
        return;
    };
    let began = time.halt_began.swap(0, Ordering::Relaxed);
    if began != 0 {
        time.halted_cycles
            .fetch_add(rdtsc().saturating_sub(began), Ordering::Relaxed);
    }
}

/// Record where every blocked user task is parked. Called by an idle CPU, so
/// the samples describe exactly the moments some CPU had nothing to run.
pub fn sample_park_sites() {
    if !enabled() {
        return;
    }
    let now_ms = crate::sleep::sleep_queue_now_ms();
    anchor_clock(now_ms);
    let last = LAST_SAMPLE_MS.load(Ordering::Relaxed);
    if now_ms < last.saturating_add(SAMPLE_SPACING_MS) {
        return;
    }
    if SAMPLING
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    LAST_SAMPLE_MS.store(now_ms, Ordering::Relaxed);
    SAMPLES.fetch_add(1, Ordering::Relaxed);
    task_for_each_active(|task| {
        if task.flags & TASK_FLAG_USER_MODE == 0 {
            return;
        }
        match task.status() {
            TaskStatus::Ready if !task.on_cpu() => {
                READY_WAITING.fetch_add(1, Ordering::Relaxed);
            }
            TaskStatus::Blocked => {
                let reason = task.load_block_reason().as_u8() as usize;
                if let Some(counter) = BLOCKED_BY_REASON.get(reason) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                record_chain(task.switch_ctx_rbp(), task.switch_ctx_rip_rsp().0);
            }
            _ => {}
        }
    });
    SAMPLING.store(false, Ordering::Release);
}

fn record_chain(rbp: u64, rip: u64) {
    let mut entries = [StacktraceEntry {
        frame_pointer: 0,
        return_address: 0,
    }; CHAIN_DEPTH];
    let captured = if rbp == 0 {
        0
    } else {
        stacktrace_capture_from(rbp, entries.as_mut_ptr(), CHAIN_DEPTH as core::ffi::c_int).max(0)
            as usize
    };
    let mut frames = [0u64; CHAIN_DEPTH];
    frames[0] = rip;
    for (k, entry) in entries
        .iter()
        .take(captured.min(CHAIN_DEPTH - 1))
        .enumerate()
    {
        frames[k + 1] = entry.return_address;
    }
    let key = frames.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &f| {
        (h ^ f).wrapping_mul(0x0000_0100_0000_01B3)
    }) | 1;
    let Some(slot) = CHAIN_HITS.slot(key) else {
        return;
    };
    if CHAIN_HITS.counts[slot].fetch_add(1, Ordering::Relaxed) == 0 {
        for (k, frame) in frames.iter().enumerate() {
            CHAIN_FRAMES[slot][k].store(*frame, Ordering::Relaxed);
        }
    }
}

/// Frames of a park site worth printing, past the switch plumbing every one
/// of them shares.
const PRINTED_FRAMES: usize = 12;

/// The context switch and the scheduler's blocking entry, which every park
/// site starts with and which say nothing about what the task waits for.
fn is_park_plumbing(addr: u64) -> bool {
    const PLUMBING: [&str; 5] = [
        "slopos_ostd::task::switch::",
        "slopos_sched::scheduler::",
        "<slopos_ostd::cpu::x86_64::interrupts::IrqDisabled>::with",
        "slopos_kernel_services::driver_runtime::yield",
        "<slopos_ostd::sync::wait_queue::",
    ];
    slopos_ostd::ksym::lookup(addr)
        .is_some_and(|s| PLUMBING.iter().any(|prefix| s.symbol.starts_with(prefix)))
}

fn symbolized(phase: &str, lead: core::fmt::Arguments<'_>, addr: u64) {
    match slopos_ostd::ksym::lookup(addr) {
        Some(s) => klog_info!(
            "PROF[{}]: {} 0x{:016x} <{}+0x{:x}>",
            phase,
            lead,
            addr,
            s.symbol,
            s.offset
        ),
        None => klog_info!("PROF[{}]: {} 0x{:016x}", phase, lead, addr),
    }
}

/// Print everything sampled so far.
pub fn report(phase: &str) {
    if !enabled() {
        return;
    }
    report_cpus(phase);
    report_ticks(phase);
    report_calls(phase);
    report_parks(phase);
    klog_info!(
        "PROF[{}]: dropped kernel={} user={} park={}",
        phase,
        KERNEL_RIPS.dropped.load(Ordering::Relaxed),
        USER_TASKS.dropped.load(Ordering::Relaxed),
        CHAIN_HITS.dropped.load(Ordering::Relaxed),
    );
}

#[inline(never)]
fn report_cpus(phase: &str) {
    let elapsed = rdtsc()
        .saturating_sub(STARTED_TSC.load(Ordering::Relaxed))
        .max(1);
    klog_info!(
        "PROF[{}]: span_ms={}",
        phase,
        elapsed / cycles_per_ms().max(1)
    );
    let cpus = slopos_arch::pcr::get_cpu_count().min(MAX_CPUS);
    for (cpu, time) in CPUS.iter().enumerate().take(cpus) {
        let halted = time.halted_cycles.load(Ordering::Relaxed);
        klog_info!(
            "PROF[{}]: cpu={} ticks user={} kernel={} idle={} halted={}.{}%",
            phase,
            cpu,
            time.user.load(Ordering::Relaxed),
            time.kernel.load(Ordering::Relaxed),
            time.idle.load(Ordering::Relaxed),
            halted * 100 / elapsed,
            halted * 1000 / elapsed % 10,
        );
    }
}

/// TSC cycles per millisecond over the profiled span, or zero before the
/// clock has advanced.
fn cycles_per_ms() -> u64 {
    let now_ms = crate::sleep::sleep_queue_now_ms();
    anchor_clock(now_ms);
    let ms = now_ms.saturating_sub(CLOCK_MS.load(Ordering::Acquire));
    let cycles = rdtsc().saturating_sub(CLOCK_TSC.load(Ordering::Relaxed));
    if ms == 0 { 0 } else { cycles / ms }
}

#[inline(never)]
fn report_calls(phase: &str) {
    let per_ms = cycles_per_ms().max(1);
    for (kind, name) in [(FaultKind::Inline, "inline"), (FaultKind::File, "file")] {
        let calls = FAULT_CALLS[kind as usize].load(Ordering::Relaxed);
        let cycles = FAULT_CYCLES[kind as usize].load(Ordering::Relaxed);
        klog_info!(
            "PROF[{}]: faults {} calls={} total_ms={} avg_us={}",
            phase,
            name,
            calls,
            cycles / per_ms,
            cycles * 1000 / per_ms / calls.max(1),
        );
    }
    let mut bound = u64::MAX;
    for _ in 0..32 {
        let mut best = (0u64, 0usize);
        for (nr, total) in SYSCALL_CYCLES.iter().enumerate() {
            let total = total.load(Ordering::Relaxed);
            if total < bound && total > best.0 {
                best = (total, nr);
            }
        }
        if best.0 == 0 {
            return;
        }
        let calls = SYSCALL_CALLS[best.1].load(Ordering::Relaxed);
        klog_info!(
            "PROF[{}]: syscall nr={} calls={} total_ms={} avg_us={}",
            phase,
            best.1,
            calls,
            best.0 / per_ms,
            best.0 * 1000 / per_ms / calls.max(1),
        );
        bound = best.0;
    }
}

#[inline(never)]
fn report_ticks(phase: &str) {
    USER_TASKS.for_each_top(16, |key, count| {
        let bytes = (key & !(1 << 63)).to_le_bytes();
        klog_info!(
            "PROF[{}]: user ticks {:>7} task '{}'",
            phase,
            count,
            bytes_as_str(&bytes)
        );
    });
    KERNEL_RIPS.for_each_top(40, |rip, count| {
        symbolized(phase, format_args!("kernel tick {:>7}", count), rip);
    });
}

#[inline(never)]
fn report_parks(phase: &str) {
    let samples = SAMPLES.load(Ordering::Relaxed).max(1);
    let waiting = READY_WAITING.load(Ordering::Relaxed);
    let per = |reason: BlockReason| {
        BLOCKED_BY_REASON[reason.as_u8() as usize].load(Ordering::Relaxed) * 100 / samples
    };
    klog_info!(
        "PROF[{}]: park samples={} ready-waiting/sample={}.{:02} blocked/sample sleep={} io={} mutex={} ipc={} generic={} futex={} (x100)",
        phase,
        samples,
        waiting / samples,
        waiting * 100 / samples % 100,
        per(BlockReason::Sleep),
        per(BlockReason::IoWait),
        per(BlockReason::MutexWait),
        per(BlockReason::IpcWait),
        per(BlockReason::Generic),
        per(BlockReason::FutexWait),
    );
    let mut rank = 0u32;
    CHAIN_HITS.for_each_top(24, |key, count| {
        klog_info!("PROF[{}]: park site #{} seen {} times", phase, rank, count);
        rank += 1;
        let Some(slot) = CHAIN_HITS.slot(key) else {
            return;
        };
        let mut printed = 0;
        for frame in CHAIN_FRAMES[slot].iter() {
            let addr = frame.load(Ordering::Relaxed);
            if addr == 0 || printed == PRINTED_FRAMES {
                break;
            }
            if printed == 0 && is_park_plumbing(addr) {
                continue;
            }
            symbolized(phase, format_args!("   "), addr);
            printed += 1;
        }
    });
}
