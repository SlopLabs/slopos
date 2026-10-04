//! `cpufreq run -- CMD`: what the CPUs did while CMD ran.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use slopos_cpufreq_core::CpuClass;

use crate::syscall::UserCpuPerfInfo;

use super::sample::{
    Limited, Snapshot, Usage, class_name, fmt_celsius, fmt_opt, fmt_permille, type_name,
};
use super::status::{epp_in_effect, placement_name, policy_name};

/// Differences are summed interval by interval, so a counter one CPU reset
/// mid-run costs that CPU one interval rather than the whole run.
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);

type Totals = BTreeMap<u32, Usage>;

fn accumulate(totals: &mut Totals, old: &Snapshot, new: &Snapshot) {
    for cpu in &new.cpus {
        totals
            .entry(cpu.cpu)
            .or_default()
            .add(&Usage::between(old, new, cpu.cpu));
    }
}

/// The hottest the package read and what limited any CPU, over the samples.
#[derive(Default)]
struct Thermal {
    package_max: Option<u8>,
    limited: Limited,
}

impl Thermal {
    fn note(&mut self, snap: &Snapshot) {
        if let Some(celsius) = snap.celsius(snap.package_therm()) {
            self.package_max = Some(self.package_max.map_or(celsius, |max| max.max(celsius)));
        }
        self.limited = self.limited.union(snap.limited_now());
    }
}

/// `name` as `execvp` finds it: a name with a slash as given, otherwise the
/// first `$PATH` entry holding it.
fn resolve(name: &str) -> PathBuf {
    if name.contains('/') {
        return PathBuf::from(name);
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin:/usr/bin".into());
    path.split(':')
        .map(|dir| Path::new(if dir.is_empty() { "." } else { dir }).join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Per-class busy time and busy-weighted frequency.
#[derive(Default)]
struct Class {
    cpus: usize,
    usage: Usage,
    busy_us: Option<u64>,
}

impl Class {
    fn add(&mut self, usage: &Usage, busy_us: Option<u64>) {
        self.cpus += 1;
        self.usage.add(usage);
        if let Some(us) = busy_us {
            *self.busy_us.get_or_insert(0) += us;
        }
    }

    fn busy_ms(&self) -> Option<u64> {
        self.busy_us.map(|us| us / 1000)
    }

    fn eff_mhz(&self, info: &UserCpuPerfInfo) -> Option<u64> {
        self.usage.eff_mhz(info)
    }
}

pub(super) fn run(command: &[String]) -> Result<i32, String> {
    let first = Snapshot::take()?;
    // A sticky log bit first set during the run is a limit the 1 s samples
    // may have missed.
    let logged_before = first.limited_since_boot();
    let (stop, stopped) = mpsc::channel::<()>();
    let sampler = thread::Builder::new()
        .name("cpufreq-sampler".into())
        .spawn(move || {
            let mut thermal = Thermal::default();
            thermal.note(&first);
            let mut previous = first;
            let mut totals = Totals::new();
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(SAMPLE_PERIOD) {
                if let Ok(now) = Snapshot::take() {
                    accumulate(&mut totals, &previous, &now);
                    thermal.note(&now);
                    previous = now;
                }
            }
            (previous, totals, thermal)
        })
        .map_err(|e| format!("cannot start the sampler thread: {e}"))?;

    let started = Instant::now();
    let spawned = Command::new(resolve(&command[0]))
        .args(&command[1..])
        .spawn();
    let waited = spawned.and_then(|mut child| child.wait());
    let wall = started.elapsed();
    drop(stop);
    let (previous, mut totals, mut thermal) = sampler
        .join()
        .map_err(|_| "the sampler thread panicked".to_owned())?;

    let status = match waited {
        Ok(status) => status,
        Err(e) => {
            eprintln!("cpufreq: {}: {e}", command[0]);
            return Ok(if e.kind() == ErrorKind::NotFound {
                127
            } else {
                126
            });
        }
    };
    let last = Snapshot::take()?;
    accumulate(&mut totals, &previous, &last);
    thermal.note(&last);
    thermal.limited = thermal
        .limited
        .union(last.limited_since_boot().without(logged_before));

    let (code, ended) = match (status.code(), status.signal()) {
        (Some(code), _) => (code, format!("exited {code}")),
        (None, Some(signal)) => (128 + signal, format!("was killed by signal {signal}")),
        (None, None) => (1, "ended without a status".to_owned()),
    };
    report(command, &last, &totals, &thermal, wall, code, &ended);
    Ok(code)
}

fn report(
    command: &[String],
    last: &Snapshot,
    totals: &Totals,
    thermal: &Thermal,
    wall: Duration,
    code: i32,
    ended: &str,
) {
    let info = &last.info;
    let wall_us = wall.as_micros() as u64;
    let source = if last.counters() {
        "APERF/MPERF"
    } else {
        "scheduler ticks; no APERF/MPERF, frequency unknown"
    };
    eprintln!(
        "cpufreq: {} {ended} after {}.{:03} s (busy time from {source})",
        command.join(" "),
        wall.as_secs(),
        wall.subsec_millis()
    );
    eprintln!("CPU T    BUSY  BUSY-MS    MHZ");
    let (mut p, mut e, mut all) = (Class::default(), Class::default(), Class::default());
    for cpu in &last.cpus {
        let usage = totals.get(&cpu.cpu).copied().unwrap_or_default();
        let busy_us = usage.busy_us(info, wall_us);
        eprintln!(
            "{:>3} {:1} {:>7} {:>8} {:>6}",
            cpu.cpu,
            type_name(cpu),
            fmt_permille(usage.busy_permille(info)),
            fmt_opt(busy_us.map(|us| us / 1000)),
            fmt_opt(usage.eff_mhz(info))
        );
        match last.class(cpu) {
            CpuClass::Performance => p.add(&usage, busy_us),
            CpuClass::Efficiency => e.add(&usage, busy_us),
        }
        all.add(&usage, busy_us);
    }
    eprintln!("CLASS CPUS  BUSY-MS    MHZ");
    for (name, class) in [
        (class_name(CpuClass::Performance), &p),
        (class_name(CpuClass::Efficiency), &e),
        ("all", &all),
    ] {
        eprintln!(
            "{name:<5} {:>4} {:>8} {:>6}",
            class.cpus,
            fmt_opt(class.busy_ms()),
            fmt_opt(class.eff_mhz(info))
        );
    }
    let limited = if last.thermal() {
        thermal.limited.letters()
    } else {
        "-".into()
    };
    eprintln!(
        "package max {}; limited during the run: {limited}",
        fmt_celsius(thermal.package_max)
    );
    eprintln!(
        "CPUFREQ[run]: wall_ms={} busy_ms={} eff_mhz={} p_busy_ms={} p_eff_mhz={} \
         e_busy_ms={} e_eff_mhz={} policy={} epp={} placement={} pkg_temp_max={} \
         limited={limited} status={code}",
        wall.as_millis(),
        fmt_opt(all.busy_ms()),
        fmt_opt(all.eff_mhz(info)),
        fmt_opt(p.busy_ms()),
        fmt_opt(p.eff_mhz(info)),
        fmt_opt(e.busy_ms()),
        fmt_opt(e.eff_mhz(info)),
        policy_name(info),
        fmt_opt(epp_in_effect(info)),
        placement_name(info),
        fmt_opt(thermal.package_max),
    );
}
