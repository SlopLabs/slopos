use std::collections::{HashMap, VecDeque};

use slopos_abi::PAGE_SIZE;
use slopos_abi::syscall::{
    CPU_PERF_F_DTS, CPU_PERF_F_HYBRID, CPU_PERF_F_PTM, DISPLAY_ADAPTER_NONE,
};
use slopos_abi::task::{INVALID_PROCESS_ID, TaskStatus};
use slopos_cpufreq_core::{CoreType, CpuClass, ThermStatus};

use crate::apps::cpufreq::sample::Usage;
use crate::syscall::{UserBlockStats, UserDisplayStats, UserTaskEntry};

use super::format::{c_text, task_name_string, task_state};
use super::sample::{Iface, Sample};
use super::selection::TaskKey;

pub const WINDOW: usize = 60;
/// Shares are in thousandths.
pub const WHOLE: u64 = 1000;

#[derive(Clone, Debug, Default)]
pub struct History(VecDeque<u64>);

impl History {
    pub fn push(&mut self, value: u64) {
        if self.0.len() == WINDOW {
            self.0.pop_front();
        }
        self.0.push_back(value);
    }

    pub fn values(&self) -> Vec<u64> {
        self.0.iter().copied().collect()
    }

    pub fn last(&self) -> u64 {
        self.0.back().copied().unwrap_or(0)
    }

    pub fn peak(&self) -> u64 {
        self.0.iter().copied().max().unwrap_or(0)
    }
}

#[derive(Clone, Debug)]
pub struct Processor {
    pub cpu: u32,
    pub busy: History,
    pub class: Option<CpuClass>,
}

#[derive(Clone, Debug, Default)]
pub struct Cpu {
    pub busy: History,
    pub processors: Vec<Processor>,
    pub mhz: Option<u64>,
    pub celsius: Option<u8>,
    pub switches_per_second: u64,
}

#[derive(Clone)]
pub struct Adapter {
    pub ifindex: u32,
    pub name: String,
    /// Bytes a second, as `sent`.
    pub received: History,
    pub sent: History,
    pub now: Iface,
}

#[derive(Clone)]
pub struct DiskActivity {
    pub name: String,
    pub model: Option<String>,
    pub active: History,
    /// Bytes a second, as `written`.
    pub read: History,
    pub written: History,
    pub now: UserBlockStats,
}

#[derive(Clone)]
pub struct Display {
    pub busy: History,
    pub frames: History,
    pub now: UserDisplayStats,
}

#[derive(Clone, Debug)]
pub struct Process {
    pub key: TaskKey,
    pub name: String,
    pub uid: u32,
    pub kernel: bool,
    pub threads: u32,
    /// Of every CPU's last interval.
    pub cpu: u32,
    pub memory: Option<u64>,
    pub cpu_time_us: u64,
    pub state: TaskStatus,
}

#[derive(Clone, Default)]
pub struct Monitor {
    pub intervals: u64,
    pub now: Sample,
    pub cpu: Cpu,
    pub memory: History,
    pub adapters: Vec<Adapter>,
    pub disks: Vec<DiskActivity>,
    pub display: Option<Display>,
    pub processes: Vec<Process>,
}

impl Monitor {
    /// The first sample only sets the start every figure is measured from.
    pub fn record(&mut self, sample: Sample) {
        let prev = std::mem::replace(&mut self.now, sample);
        let interval_ms = match prev.at_ms {
            0 => None,
            at => Some(self.now.at_ms.saturating_sub(at).max(1)),
        };
        if interval_ms.is_some() {
            self.intervals += 1;
        }
        self.record_cpu(&prev, interval_ms);
        if interval_ms.is_some() {
            self.memory.push(used_bytes(&self.now));
        }
        self.record_adapters(&prev, interval_ms);
        self.record_disks(&prev, interval_ms);
        self.record_display(&prev, interval_ms);
        self.processes = processes(&prev, &self.now, interval_ms);
    }

    pub fn process_count(&self) -> usize {
        self.processes.iter().filter(|p| !p.kernel).count()
    }

    pub fn thread_count(&self) -> usize {
        self.now.tasks.len()
    }

    fn record_cpu(&mut self, prev: &Sample, interval_ms: Option<u64>) {
        let now = &self.now;
        let hybrid = now.perf_info.flags & CPU_PERF_F_HYBRID != 0;
        let mut total = Usage::default();
        let mut processors = Vec::with_capacity(now.cpus.len());
        for stats in &now.cpus {
            let cpu = stats.cpu_id;
            let mut busy = self
                .cpu
                .processors
                .iter()
                .find(|p| p.cpu == cpu)
                .map(|p| p.busy.clone())
                .unwrap_or_default();
            if interval_ms.is_some() {
                let usage = usage(prev, now, cpu);
                total.add(&usage);
                busy.push(u64::from(busy_permille(&usage, now)));
            }
            let class = hybrid
                .then(|| now.perf.iter().find(|p| p.cpu == cpu))
                .flatten()
                .map(|p| CpuClass::of(true, CoreType::from_raw(p.core_type)));
            processors.push(Processor { cpu, busy, class });
        }
        self.cpu.processors = processors;
        self.cpu.celsius = celsius(now);
        let Some(ms) = interval_ms else {
            return;
        };
        self.cpu.busy.push(u64::from(busy_permille(&total, now)));
        self.cpu.mhz = total.eff_mhz(&now.perf_info);
        let switches = now
            .sys
            .task_context_switches
            .saturating_sub(prev.sys.task_context_switches);
        self.cpu.switches_per_second = per_second(switches, ms);
    }

    fn record_adapters(&mut self, prev: &Sample, interval_ms: Option<u64>) {
        let Some(ifaces) = &self.now.ifaces else {
            return;
        };
        let before: &[Iface] = prev.ifaces.as_deref().unwrap_or(&[]);
        self.adapters = ifaces
            .iter()
            .map(|now| {
                let ifindex = now.iface.ifindex;
                let mut adapter = self
                    .adapters
                    .iter()
                    .find(|a| a.ifindex == ifindex)
                    .cloned()
                    .unwrap_or_else(|| Adapter {
                        ifindex,
                        name: c_text(&now.iface.name),
                        received: History::default(),
                        sent: History::default(),
                        now: *now,
                    });
                adapter.now = *now;
                let was = before.iter().find(|b| b.iface.ifindex == ifindex);
                if let (Some(ms), Some(was)) = (interval_ms, was) {
                    let rx = now.iface.rx_bytes.saturating_sub(was.iface.rx_bytes);
                    let tx = now.iface.tx_bytes.saturating_sub(was.iface.tx_bytes);
                    adapter.received.push(per_second(rx, ms));
                    adapter.sent.push(per_second(tx, ms));
                }
                adapter
            })
            .collect();
    }

    fn record_disks(&mut self, prev: &Sample, interval_ms: Option<u64>) {
        self.disks = self
            .now
            .disks
            .iter()
            .map(|disk| {
                let name = c_text(&disk.stats.name);
                let mut activity = self
                    .disks
                    .iter()
                    .find(|d| d.name == name)
                    .cloned()
                    .unwrap_or_else(|| DiskActivity {
                        name: name.clone(),
                        model: disk.model.clone(),
                        active: History::default(),
                        read: History::default(),
                        written: History::default(),
                        now: disk.stats,
                    });
                let was = prev
                    .disks
                    .iter()
                    .find(|d| d.stats.name == disk.stats.name)
                    .map(|d| d.stats);
                if let (Some(ms), Some(was)) = (interval_ms, was) {
                    let now = &disk.stats;
                    let busy_ns = now.busy_ns.saturating_sub(was.busy_ns);
                    activity.active.push(share(busy_ns, ms * 1_000_000));
                    let read = now.read_bytes.saturating_sub(was.read_bytes);
                    let written = now.write_bytes.saturating_sub(was.write_bytes);
                    activity.read.push(per_second(read, ms));
                    activity.written.push(per_second(written, ms));
                }
                activity.now = disk.stats;
                activity.model = disk.model.clone();
                activity
            })
            .collect();
    }

    fn record_display(&mut self, prev: &Sample, interval_ms: Option<u64>) {
        let Some(now) = self
            .now
            .display
            .filter(|d| d.adapter != DISPLAY_ADAPTER_NONE)
        else {
            self.display = None;
            return;
        };
        let mut display = self.display.take().unwrap_or_else(|| Display {
            busy: History::default(),
            frames: History::default(),
            now,
        });
        if let (Some(ms), Some(was)) = (interval_ms, prev.display) {
            let busy_ns = now.present_ns.saturating_sub(was.present_ns);
            display.busy.push(share(busy_ns, ms * 1_000_000));
            let frames = now.frames.saturating_sub(was.frames);
            display.frames.push(per_second(frames, ms));
        }
        display.now = now;
        self.display = Some(display);
    }
}

pub fn used_bytes(sample: &Sample) -> u64 {
    let sys = &sample.sys;
    u64::from(sys.allocated_pages.min(sys.total_pages)).saturating_mul(PAGE_SIZE)
}

pub fn total_bytes(sample: &Sample) -> u64 {
    u64::from(sample.sys.total_pages).saturating_mul(PAGE_SIZE)
}

pub fn free_bytes(sample: &Sample) -> u64 {
    u64::from(sample.sys.free_pages).saturating_mul(PAGE_SIZE)
}

fn per_second(count: u64, ms: u64) -> u64 {
    (u128::from(count) * 1000 / u128::from(ms.max(1))) as u64
}

fn share(part: u64, whole: u64) -> u64 {
    (u128::from(part) * u128::from(WHOLE) / u128::from(whole.max(1))).min(u128::from(WHOLE)) as u64
}

fn usage(prev: &Sample, now: &Sample, cpu: u32) -> Usage {
    let perf = |s: &Sample| s.perf.iter().find(|c| c.cpu == cpu).copied();
    let ticks = |s: &Sample| s.cpus.iter().find(|c| c.cpu_id == cpu).copied();
    let mut usage = match (perf(prev), perf(now)) {
        (Some(a), Some(b)) => Usage::of_samples(&a, &b),
        _ => Usage::default(),
    };
    if let (Some(a), Some(b)) = (ticks(prev), ticks(now))
        && b.total_ticks >= a.total_ticks
        && b.idle_ticks >= a.idle_ticks
    {
        usage.ticks = b.total_ticks - a.total_ticks;
        usage.busy_ticks = usage.ticks.saturating_sub(b.idle_ticks - a.idle_ticks);
    }
    usage
}

fn busy_permille(usage: &Usage, now: &Sample) -> u32 {
    usage
        .busy_permille(&now.perf_info)
        .or_else(|| {
            (usage.ticks > 0).then(|| (usage.busy_ticks * WHOLE / usage.ticks).min(WHOLE) as u32)
        })
        .unwrap_or(0)
}

fn celsius(sample: &Sample) -> Option<u8> {
    let info = &sample.perf_info;
    let read = |msr: u64| ThermStatus::from_msr(msr).celsius(info.temperature_target);
    if info.flags & CPU_PERF_F_PTM != 0
        && let Some(degrees) = read(info.package_therm_status)
    {
        return Some(degrees);
    }
    if info.flags & CPU_PERF_F_DTS == 0 {
        return None;
    }
    sample
        .perf
        .iter()
        .filter_map(|cpu| read(cpu.therm_status))
        .max()
}

/// Process ids and kernel task ids overlap, so neither stands for the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Group {
    Process(u32),
    Kernel(u32),
}

/// A process is named by its first thread, whose task id is what `getpid`
/// answers and `kill` takes.
fn processes(prev: &Sample, now: &Sample, interval_ms: Option<u64>) -> Vec<Process> {
    let before: HashMap<TaskKey, u64> = prev
        .tasks
        .iter()
        .map(|task| (TaskKey::of(task), task.total_runtime_us))
        .collect();
    let cpus = now.cpus.len().max(1) as u128;
    let spent = |task: &UserTaskEntry| -> u64 {
        match before.get(&TaskKey::of(task)) {
            Some(&was) => task.total_runtime_us.saturating_sub(was),
            None if task.creation_time_ms >= prev.at_ms => task.total_runtime_us,
            None => 0,
        }
    };
    let resident: HashMap<u32, u64> = now
        .memory
        .iter()
        .map(|m| (m.process_id, m.resident_pages.saturating_mul(PAGE_SIZE)))
        .collect();

    let mut order: Vec<Group> = Vec::new();
    let mut groups: HashMap<Group, Vec<&UserTaskEntry>> = HashMap::new();
    for task in &now.tasks {
        let group = match task.process_id {
            INVALID_PROCESS_ID => Group::Kernel(task.task_id),
            id => Group::Process(id),
        };
        groups
            .entry(group)
            .or_insert_with(|| {
                order.push(group);
                Vec::new()
            })
            .push(task);
    }

    order
        .into_iter()
        .filter_map(|group| {
            let threads = groups.remove(&group)?;
            let leader = threads.iter().min_by_key(|t| t.task_id)?;
            let spent: u64 = threads.iter().map(|t| spent(t)).sum();
            let cpu = match interval_ms {
                Some(ms) => {
                    let whole = u128::from(ms) * 1000 * cpus;
                    (u128::from(spent) * u128::from(WHOLE) / whole).min(u128::from(WHOLE)) as u32
                }
                None => 0,
            };
            let memory = match group {
                Group::Process(id) => resident.get(&id).copied(),
                Group::Kernel(_) => None,
            };
            Some(Process {
                key: TaskKey::of(leader),
                name: task_name_string(leader),
                uid: leader.uid,
                kernel: matches!(group, Group::Kernel(_)),
                threads: threads.len() as u32,
                cpu,
                memory,
                cpu_time_us: threads.iter().map(|t| t.total_runtime_us).sum(),
                state: threads
                    .iter()
                    .map(|t| TaskStatus::from_u8(t.state))
                    .min_by_key(|s| task_state(*s).0)
                    .unwrap_or(TaskStatus::Invalid),
            })
        })
        .collect()
}
