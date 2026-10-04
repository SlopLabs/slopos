//! Snapshots of what every CPU's counters said, and what one CPU did between
//! two of them.
//!
//! Each CPU records `IA32_APERF`, `IA32_MPERF` and the TSC together at its
//! own timer ticks and idle entries, so a snapshot holds per-CPU samples of
//! slightly different ages. Differences are taken per CPU, between that CPU's
//! own samples: what it did in its window, which tiles the time a reader
//! spans. Where the part has no APERF/MPERF pair (a hypervisor, most often),
//! the scheduler's tick accounting still says how busy each CPU was, so busy
//! time falls back to it and the frequency is reported unknown.

use std::sync::LazyLock;

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    CPU_PERF_F_APERF_MPERF, CPU_PERF_F_DTS, CPU_PERF_F_HYBRID, CPU_PERF_F_PTM,
};
use slopos_cpufreq_core::therm::Condition;
use slopos_cpufreq_core::{CoreType, CpuClass, ThermStatus, freq};

use crate::syscall::{
    UserCpuInfo, UserCpuPerf, UserCpuPerfInfo, UserPerCpuStats, core as sys_core,
};

/// The most CPUs a snapshot asks the kernel for, when `cpu_info` cannot say.
const MAX_CPUS: usize = 256;

/// `-errno` as `Description (ENAME)`.
pub fn errno_text(rc: i64) -> String {
    match Errno::from_raw(rc as i32) {
        Some(e) => format!("{} ({})", e.description(), e.name()),
        None => format!("error {rc}"),
    }
}

/// CPUs the kernel brought up: the buffer a snapshot passes.
static CPU_SLOTS: LazyLock<usize> = LazyLock::new(|| {
    let mut info = UserCpuInfo::default();
    if sys_core::cpu_info(&mut info) >= 0 && info.cpu_count > 0 {
        info.cpu_count as usize
    } else {
        MAX_CPUS
    }
});

pub fn cpu_slots() -> usize {
    *CPU_SLOTS
}

pub struct Snapshot {
    pub info: UserCpuPerfInfo,
    /// The CPUs that have recorded their state, by CPU number.
    pub cpus: Vec<UserCpuPerf>,
    stats: Vec<UserPerCpuStats>,
}

impl Snapshot {
    pub fn take() -> Result<Self, String> {
        let slots = cpu_slots();
        let mut info = UserCpuPerfInfo::default();
        let mut cpus = vec![UserCpuPerf::default(); slots];
        let written = sys_core::cpu_perf(Some(&mut info), &mut cpus);
        if written < 0 {
            return Err(format!(
                "cannot read the CPUs' performance state: {}",
                errno_text(written)
            ));
        }
        cpus.truncate(written as usize);
        cpus.retain(|cpu| cpu.online != 0);
        let mut stats = vec![UserPerCpuStats::default(); slots];
        let written = sys_core::percpu_stats(&mut stats);
        stats.truncate(written.max(0) as usize);
        Ok(Self { info, cpus, stats })
    }

    /// The APERF/MPERF pair is sampled: effective frequency is known.
    pub fn counters(&self) -> bool {
        self.info.flags & CPU_PERF_F_APERF_MPERF != 0
    }

    pub fn hybrid(&self) -> bool {
        self.info.flags & CPU_PERF_F_HYBRID != 0
    }

    pub fn class(&self, cpu: &UserCpuPerf) -> CpuClass {
        CpuClass::of(self.hybrid(), CoreType::from_raw(cpu.core_type))
    }

    pub fn cpu(&self, cpu: u32) -> Option<&UserCpuPerf> {
        self.cpus.iter().find(|c| c.cpu == cpu)
    }

    /// `cpu`'s thermal status, `None` without a per-core sensor.
    pub fn cpu_therm(&self, cpu: &UserCpuPerf) -> Option<ThermStatus> {
        (self.info.flags & CPU_PERF_F_DTS != 0).then(|| ThermStatus::from_msr(cpu.therm_status))
    }

    /// The package's thermal status, `None` without a package sensor.
    pub fn package_therm(&self) -> Option<ThermStatus> {
        (self.info.flags & CPU_PERF_F_PTM != 0)
            .then(|| ThermStatus::from_msr(self.info.package_therm_status))
    }

    /// Degrees Celsius a status reads, given this machine's throttling point.
    pub fn celsius(&self, status: Option<ThermStatus>) -> Option<u8> {
        status?.celsius(self.info.temperature_target)
    }

    /// Whether either thermal sensor reports at all.
    pub fn thermal(&self) -> bool {
        self.info.flags & (CPU_PERF_F_DTS | CPU_PERF_F_PTM) != 0
    }

    /// What holds the package or any CPU below its requested frequency now.
    pub fn limited_now(&self) -> Limited {
        self.cpus
            .iter()
            .filter_map(|cpu| self.cpu_therm(cpu))
            .chain(self.package_therm())
            .fold(Limited::default(), |seen, status| {
                seen.union(Limited::now(status))
            })
    }

    /// What the package or any CPU has logged since boot.
    pub fn limited_since_boot(&self) -> Limited {
        self.cpus
            .iter()
            .filter_map(|cpu| self.cpu_therm(cpu))
            .chain(self.package_therm())
            .fold(Limited::default(), |seen, status| {
                seen.union(Limited::since_boot(status))
            })
    }

    fn stats(&self, cpu: u32) -> Option<&UserPerCpuStats> {
        self.stats.iter().find(|s| s.cpu_id == cpu)
    }
}

/// What a CPU did over an interval, or the sum of several.
#[derive(Clone, Copy, Default)]
pub struct Usage {
    pub aperf: u64,
    pub mperf: u64,
    pub tsc: u64,
    /// Scheduler ticks that found the CPU running something, of `ticks`.
    pub busy_ticks: u64,
    pub ticks: u64,
}

impl Usage {
    /// `cpu` between two snapshots. A counter that went backwards (its CPU
    /// was reset under the reader) leaves that source empty for the interval
    /// rather than inventing a delta.
    pub fn between(old: &Snapshot, new: &Snapshot, cpu: u32) -> Usage {
        let mut usage = match (old.cpu(cpu), new.cpu(cpu)) {
            (Some(a), Some(b)) => Usage::of_samples(a, b),
            _ => Usage::default(),
        };
        if let (Some(a), Some(b)) = (old.stats(cpu), new.stats(cpu))
            && b.total_ticks >= a.total_ticks
            && b.idle_ticks >= a.idle_ticks
        {
            usage.ticks = b.total_ticks - a.total_ticks;
            let idle = b.idle_ticks - a.idle_ticks;
            usage.busy_ticks = usage.ticks.saturating_sub(idle);
        }
        usage
    }

    /// The counters alone, between two samples of one CPU.
    pub fn of_samples(old: &UserCpuPerf, new: &UserCpuPerf) -> Usage {
        if new.aperf < old.aperf || new.mperf < old.mperf || new.tsc < old.tsc {
            return Usage::default();
        }
        Usage {
            aperf: new.aperf - old.aperf,
            mperf: new.mperf - old.mperf,
            tsc: new.tsc - old.tsc,
            ..Usage::default()
        }
    }

    pub fn add(&mut self, other: &Usage) {
        self.aperf += other.aperf;
        self.mperf += other.mperf;
        self.tsc += other.tsc;
        self.busy_ticks += other.busy_ticks;
        self.ticks += other.ticks;
    }

    /// Average clock while busy; `None` when the counters are not sampled,
    /// the TSC rate is unmeasured, or the CPU never ran. A sum over several
    /// CPUs is their busy-weighted average.
    pub fn eff_mhz(&self, info: &UserCpuPerfInfo) -> Option<u64> {
        if info.flags & CPU_PERF_F_APERF_MPERF == 0 || info.tsc_khz == 0 || self.mperf == 0 {
            return None;
        }
        Some(freq::effective_khz(info.tsc_khz, self.aperf, self.mperf) / 1000)
    }

    /// Share of the interval spent busy, in thousandths, from MPERF where it
    /// is sampled and from the tick accounting otherwise.
    pub fn busy_permille(&self, info: &UserCpuPerfInfo) -> Option<u32> {
        if info.flags & CPU_PERF_F_APERF_MPERF != 0 {
            if self.tsc == 0 {
                return None;
            }
            return Some(freq::busy_permille(self.mperf, self.tsc));
        }
        if self.ticks == 0 {
            return None;
        }
        Some((self.busy_ticks * 1000 / self.ticks).min(1000) as u32)
    }

    /// Busy time of one CPU over an interval of `wall_us`, in microseconds:
    /// MPERF counts at the TSC's rate only while the CPU executes, so where it
    /// is sampled this is exact; otherwise the busy share of the ticks scales
    /// the interval.
    pub fn busy_us(&self, info: &UserCpuPerfInfo, wall_us: u64) -> Option<u64> {
        if info.flags & CPU_PERF_F_APERF_MPERF != 0 && info.tsc_khz != 0 {
            return Some((self.mperf as u128 * 1000 / info.tsc_khz as u128) as u64);
        }
        if self.ticks == 0 {
            return None;
        }
        Some((wall_us as u128 * self.busy_ticks as u128 / self.ticks as u128) as u64)
    }
}

/// `per mille` as `12.3%`, `-` when unknown.
pub fn fmt_permille(permille: Option<u32>) -> String {
    match permille {
        Some(p) => format!("{}.{}%", p / 10, p % 10),
        None => "-".into(),
    }
}

/// A number, `-` when unknown.
pub fn fmt_opt<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "-".into(),
    }
}

/// `P`, `E` or `-` for what `CPUID.1AH` reported about the CPU.
pub fn type_name(cpu: &UserCpuPerf) -> &'static str {
    CoreType::from_raw(cpu.core_type).name()
}

pub fn class_name(class: CpuClass) -> &'static str {
    match class {
        CpuClass::Performance => "P",
        CpuClass::Efficiency => "E",
    }
}

/// What the throttle letters mean.
pub const THROTTLE_LEGEND: &str = "throttle: T thermal, H PROCHOT, C critical, P power limit, \
                                   A current limit; uppercase now, lowercase since boot";

const THROTTLE_LETTERS: [u8; 5] = *b"THCPA";

fn conditions(status: ThermStatus) -> [Condition; 5] {
    [
        status.thermal,
        status.prochot,
        status.critical,
        status.power_limit,
        status.current_limit,
    ]
}

/// A set of throttle conditions, one bit per letter of `THROTTLE_LETTERS`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Limited(u8);

impl Limited {
    fn of(status: ThermStatus, pick: impl Fn(Condition) -> bool) -> Self {
        let bits = conditions(status)
            .iter()
            .enumerate()
            .filter(|(_, condition)| pick(**condition))
            .fold(0, |bits, (i, _)| bits | 1 << i);
        Self(bits)
    }

    pub fn now(status: ThermStatus) -> Self {
        Self::of(status, |c| c.now)
    }

    pub fn since_boot(status: ThermStatus) -> Self {
        Self::of(status, |c| c.since_boot)
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Uppercase letters, `none` for the empty set.
    pub fn letters(self) -> String {
        if self.is_empty() {
            return "none".into();
        }
        THROTTLE_LETTERS
            .iter()
            .enumerate()
            .filter(|(i, _)| self.0 & 1 << i != 0)
            .map(|(_, &letter)| letter as char)
            .collect()
    }
}

/// A status as throttle letters: uppercase for a condition holding now,
/// lowercase for one only logged since boot, `-` for neither or no sensor.
pub fn throttle_text(status: Option<ThermStatus>) -> String {
    let Some(status) = status else {
        return "-".into();
    };
    let text: String = conditions(status)
        .iter()
        .zip(THROTTLE_LETTERS)
        .filter_map(
            |(condition, letter)| match (condition.now, condition.since_boot) {
                (true, _) => Some(letter as char),
                (false, true) => Some(letter.to_ascii_lowercase() as char),
                (false, false) => None,
            },
        )
        .collect();
    if text.is_empty() { "-".into() } else { text }
}

/// Degrees as `67 C`, `-` when unreported.
pub fn fmt_celsius(celsius: Option<u8>) -> String {
    celsius.map_or_else(|| "-".into(), |c| format!("{c} C"))
}
