//! The system load average: the number of tasks ready to run or running,
//! damped over one, five and fifteen minutes and sampled every five seconds,
//! in the 11-bit fixed point Unix has always kept it in.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::per_cpu;

pub const FSHIFT: u32 = 11;
pub const FIXED_1: u64 = 1 << FSHIFT;
const SAMPLE_INTERVAL_MS: u64 = 5_000;
/// `FIXED_1 / exp(5 s / period)` for periods of one, five and fifteen minutes.
pub const DECAY: [u64; 3] = [1884, 2014, 2037];

static AVERAGES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static NEXT_SAMPLE_MS: AtomicU64 = AtomicU64::new(SAMPLE_INTERVAL_MS);

/// One damping step: `load * decay + active * (1 - decay)`, rounded up while
/// the load rises and down while it falls, so a steady load is reached and an
/// idle machine decays to zero instead of settling a step short of either.
pub fn damp(load: u64, decay: u64, active: u64) -> u64 {
    let next = load * decay + active * (FIXED_1 - decay);
    let round = if active >= load { FIXED_1 - 1 } else { 0 };
    (next + round) >> FSHIFT
}

/// Called from each CPU's timer tick once it has sampled what it holds
/// runnable; CPU 0 folds every CPU's sample into the averages once a sample
/// interval is due.
pub fn tick(cpu_id: usize, now_ms: u64) {
    if cpu_id != 0 {
        return;
    }
    let due = NEXT_SAMPLE_MS.load(Ordering::Relaxed);
    if now_ms < due {
        return;
    }
    NEXT_SAMPLE_MS.store(now_ms + SAMPLE_INTERVAL_MS, Ordering::Relaxed);
    let active = u64::from(runnable_total()) * FIXED_1;
    for (average, decay) in AVERAGES.iter().zip(DECAY) {
        average.store(
            damp(average.load(Ordering::Relaxed), decay, active),
            Ordering::Relaxed,
        );
    }
}

fn runnable_total() -> u32 {
    (0..slopos_arch::pcr::get_cpu_count())
        .filter_map(|cpu_id| per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.runnable_sample()))
        .sum()
}

/// The one-, five- and fifteen-minute averages, [`FIXED_1`] meaning one task.
pub fn load_averages() -> [u64; 3] {
    AVERAGES
        .each_ref()
        .map(|average| average.load(Ordering::Relaxed))
}
