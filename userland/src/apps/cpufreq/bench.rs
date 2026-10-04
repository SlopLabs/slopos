//! `cpufreq bench`: what a thread gets from each kind of CPU, what sharing a
//! core with a sibling thread costs it, and where the scheduler puts it.
//!
//! One fixed integer workload throughout: four independent xorshift streams,
//! so issue width counts as well as the clock, indexing loads and stores into
//! a table that stays in L1, so it measures the core rather than memory. Every
//! figure is the median of `REPEATS` timed runs after a warm-up long enough
//! for the clock to ramp.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use slopos_cpufreq_core::CpuClass;

use crate::syscall::{UserCpuPerf, core as sys_core};

use super::sample::{Snapshot, Usage, class_name, errno_text, fmt_opt, type_name};
use super::status::placement_name;

const REPEATS: usize = 5;
const WARM_UP: Duration = Duration::from_millis(100);
const CALIBRATION_FLOOR: Duration = Duration::from_millis(10);
const MIN_ITERS: u64 = 1 << 14;
/// `sched_setaffinity` carries a 32-bit mask.
const PINNABLE_CPUS: u32 = 32;

pub(super) struct Options {
    /// Length of one timed run.
    pub run_ms: u64,
    /// Unpinned runs the placement measurement makes.
    pub placed_runs: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            run_ms: 50,
            placed_runs: 20,
        }
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

#[inline(never)]
fn workload(iters: u64) -> u64 {
    let iters = black_box(iters);
    let mut table = [0u32; 256];
    let mut streams: [u64; 4] = black_box([
        0x9E37_79B9_7F4A_7C15,
        0xBF58_476D_1CE4_E5B9,
        0x94D0_49BB_1331_11EB,
        0x2545_F491_4F6C_DD1D,
    ]);
    let mut acc: u64 = 0;
    for i in 0..iters {
        streams = streams.map(xorshift);
        let [a, b, c, d] = streams;
        let read = table[((a ^ c) >> 56) as usize];
        let slot = ((b ^ d) >> 56) as usize;
        table[slot] = table[slot].wrapping_add(read ^ i as u32);
        acc = acc.wrapping_add(u64::from(read)).rotate_left(1);
    }
    black_box(&table);
    acc
}

/// Nanoseconds per iteration of one run.
fn timed(iters: u64) -> f64 {
    let start = Instant::now();
    black_box(workload(iters));
    start.elapsed().as_nanos() as f64 / iters as f64
}

fn warm_up(iters: u64) {
    let start = Instant::now();
    while start.elapsed() < WARM_UP {
        black_box(workload(iters / 8 + 1));
    }
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// Iterations one run of `run_ms` takes on the CPU this thread is on.
fn calibrate(run_ms: u64) -> u64 {
    warm_up(MIN_ITERS);
    let mut iters = MIN_ITERS;
    loop {
        let start = Instant::now();
        black_box(workload(iters));
        let elapsed = start.elapsed();
        if elapsed >= CALIBRATION_FLOOR {
            let scaled = iters as u128 * u128::from(run_ms) * 1_000_000 / elapsed.as_nanos();
            return (scaled as u64).max(MIN_ITERS);
        }
        iters *= 2;
    }
}

fn pin_self(cpu: u32) -> Result<(), String> {
    if cpu >= PINNABLE_CPUS {
        return Err(format!("CPU {cpu} is beyond the affinity mask"));
    }
    let rc = sys_core::set_cpu_affinity(0, 1 << cpu);
    if rc < 0 {
        return Err(format!("pinning to CPU {cpu}: {}", errno_text(rc)));
    }
    for _ in 0..1000 {
        if sys_core::get_current_cpu() == cpu {
            return Ok(());
        }
        thread::yield_now();
    }
    Err(format!("pinned to CPU {cpu} but never ran there"))
}

fn on_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    thread::Builder::new()
        .spawn(work)
        .map_err(|e| format!("cannot start a worker thread: {e}"))?
        .join()
        .map_err(|_| "a worker thread panicked".to_owned())?
}

struct Pinned {
    ns: f64,
    mhz: Option<u64>,
}

fn measure_pinned(cpu: u32, iters: u64) -> Result<Pinned, String> {
    on_thread(move || {
        pin_self(cpu)?;
        warm_up(iters);
        let before = Snapshot::take()?;
        let samples: Vec<f64> = (0..REPEATS).map(|_| timed(iters)).collect();
        let after = Snapshot::take()?;
        Ok(Pinned {
            ns: median(samples),
            mhz: Usage::between(&before, &after, cpu).eff_mhz(&after.info),
        })
    })
}

/// Two workers on `a` and `b` at once, each run started together; the mean
/// of their medians.
fn measure_pair(a: u32, b: u32, iters: u64) -> Result<f64, String> {
    let barrier = Arc::new(Barrier::new(2));
    let pinned = Arc::new(AtomicBool::new(true));
    let spawn = |cpu: u32| {
        let (barrier, pinned) = (barrier.clone(), pinned.clone());
        thread::Builder::new().spawn(move || -> Result<f64, String> {
            let result = pin_self(cpu);
            if result.is_err() {
                pinned.store(false, Ordering::Relaxed);
            }
            barrier.wait();
            result?;
            if !pinned.load(Ordering::Relaxed) {
                return Err("the other worker could not be pinned".into());
            }
            warm_up(iters);
            let mut samples = Vec::with_capacity(REPEATS);
            for _ in 0..REPEATS {
                barrier.wait();
                samples.push(timed(iters));
            }
            Ok(median(samples))
        })
    };
    let first = spawn(a).map_err(|e| format!("cannot start a worker thread: {e}"))?;
    let second = spawn(b).map_err(|e| format!("cannot start a worker thread: {e}"))?;
    let first = first.join().map_err(|_| "a worker thread panicked")??;
    let second = second.join().map_err(|_| "a worker thread panicked")??;
    Ok((first + second) / 2.0)
}

/// A core with two online threads, preferring a performance core, and a CPU
/// of the same class on another core.
fn smt_cpus(snap: &Snapshot) -> Option<(u32, u32, Option<u32>)> {
    let pinnable = |cpu: &&UserCpuPerf| cpu.cpu < PINNABLE_CPUS;
    let sibling = |cpu: &UserCpuPerf| {
        snap.cpus
            .iter()
            .filter(pinnable)
            .find(|other| other.core_id == cpu.core_id && other.cpu != cpu.cpu)
            .map(|other| other.cpu)
    };
    let mut firsts: Vec<&UserCpuPerf> = snap
        .cpus
        .iter()
        .filter(pinnable)
        .filter(|cpu| sibling(cpu).is_some())
        .collect();
    firsts.sort_by_key(|cpu| (snap.class(cpu) != CpuClass::Performance, cpu.smt_thread));
    let first = *firsts.first()?;
    let apart = snap
        .cpus
        .iter()
        .filter(pinnable)
        .filter(|cpu| cpu.core_id != first.core_id && snap.class(cpu) == snap.class(first))
        .min_by_key(|cpu| cpu.smt_thread)
        .map(|cpu| cpu.cpu);
    Some((first.cpu, sibling(first)?, apart))
}

struct Placed {
    cpu: u32,
    migrated: bool,
    ns: f64,
}

fn placed_run(iters: u64) -> Result<Placed, String> {
    on_thread(move || {
        let arrived = sys_core::get_current_cpu();
        warm_up(iters);
        let cpu = sys_core::get_current_cpu();
        let ns = timed(iters);
        let left = sys_core::get_current_cpu();
        Ok(Placed {
            cpu,
            migrated: arrived != cpu || cpu != left,
            ns,
        })
    })
}

fn ns_text(ns: Option<f64>) -> String {
    ns.map_or_else(|| "-".into(), |ns| format!("{ns:.3}"))
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, n), v| (sum + v, n + 1));
    (count > 0).then(|| sum / count as f64)
}

pub(super) fn bench(options: Options) -> Result<i32, String> {
    let snap = Snapshot::take()?;
    let run_ms = options.run_ms;
    let iters = on_thread(move || Ok(calibrate(run_ms)))?;
    println!(
        "cpufreq bench: {iters} iterations a run (~{} ms), median of {REPEATS} after {} ms warm-up; placement {}",
        options.run_ms,
        WARM_UP.as_millis(),
        placement_name(&snap.info)
    );

    println!("CPU T CORE.T  NS/ITER    MHZ  VS-BEST");
    let mut pinned: Vec<(&UserCpuPerf, Pinned)> = Vec::new();
    for cpu in &snap.cpus {
        match measure_pinned(cpu.cpu, iters) {
            Ok(result) => pinned.push((cpu, result)),
            Err(why) => eprintln!("cpufreq: CPU {} skipped: {why}", cpu.cpu),
        }
    }
    let (best_cpu, best_ns) = pinned
        .iter()
        .map(|(cpu, result)| (cpu.cpu, result.ns))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .ok_or("no CPU could be measured")?;
    for (cpu, result) in &pinned {
        println!(
            "{:>3} {:1} {:>6} {:>8.3} {:>6} {:>7.2}x",
            cpu.cpu,
            type_name(cpu),
            format!("{}.{}", cpu.core_id, cpu.smt_thread),
            result.ns,
            fmt_opt(result.mhz),
            result.ns / best_ns
        );
    }
    let class_ns = |class: CpuClass| {
        mean(
            pinned
                .iter()
                .filter(|(cpu, _)| snap.class(cpu) == class)
                .map(|(_, result)| result.ns),
        )
    };
    let p_ns = class_ns(CpuClass::Performance);
    let e_ns = class_ns(CpuClass::Efficiency);
    let ratio = p_ns.zip(e_ns).map(|(p, e)| e / p);
    for (class, ns) in [(CpuClass::Performance, p_ns), (CpuClass::Efficiency, e_ns)] {
        println!("class {}: mean {} ns/iter", class_name(class), ns_text(ns));
    }
    println!(
        "P:E speed ratio: {}",
        ratio.map_or_else(|| "-".into(), |r| format!("{r:.2}"))
    );

    let (mut pair_ns, mut apart_ns) = (None, None);
    match smt_cpus(&snap) {
        Some((a, b, apart)) => {
            pair_ns = Some(measure_pair(a, b, iters)?);
            if let Some(c) = apart {
                apart_ns = Some(measure_pair(a, c, iters)?);
            }
            println!(
                "SMT: CPUs {a}+{b} on one core {} ns/iter each; {} {} ns/iter each",
                ns_text(pair_ns),
                apart.map_or_else(
                    || "no second core".into(),
                    |c| format!("CPUs {a}+{c} on two cores")
                ),
                ns_text(apart_ns)
            );
        }
        None => println!("SMT: - (no core with two online threads)"),
    }
    let smt_cost = pair_ns
        .zip(apart_ns)
        .map(|(pair, apart)| (pair / apart - 1.0) * 100.0);

    let mut placed = Vec::with_capacity(options.placed_runs);
    for _ in 0..options.placed_runs {
        placed.push(placed_run(iters)?);
    }
    let landed = |class: CpuClass| {
        placed
            .iter()
            .filter(|run| {
                snap.cpu(run.cpu)
                    .is_some_and(|cpu| snap.class(cpu) == class)
            })
            .count()
    };
    let (on_p, on_e) = (landed(CpuClass::Performance), landed(CpuClass::Efficiency));
    let migrated = placed.iter().filter(|run| run.migrated).count();
    let placed_ns = mean(placed.iter().map(|run| run.ns));
    let slowdown = placed_ns.map(|ns| (ns / best_ns - 1.0) * 100.0);
    println!(
        "placement: {} unpinned runs, {on_p} on P, {on_e} on E, {migrated} migrated; mean {} ns/iter, {} vs best pinned CPU {best_cpu}",
        placed.len(),
        ns_text(placed_ns),
        slowdown.map_or_else(|| "-".into(), |s| format!("{s:+.1}%"))
    );
    let cpus: Vec<String> = placed.iter().map(|run| run.cpu.to_string()).collect();
    println!("placement CPUs: {}", cpus.join(" "));

    let pct = |value: Option<f64>| value.map_or_else(|| "-".into(), |v| format!("{v:.1}"));
    println!(
        "CPUFREQ[bench]: cpus={} iters={iters} best_cpu={best_cpu} best_ns={best_ns:.3} \
         p_ns={} e_ns={} p_e_ratio={} smt_pair_ns={} smt_apart_ns={} smt_cost_pct={} \
         runs={} runs_p={on_p} runs_e={on_e} runs_migrated={migrated} placed_ns={} \
         placed_slowdown_pct={} placement={}",
        pinned.len(),
        ns_text(p_ns),
        ns_text(e_ns),
        ratio.map_or_else(|| "-".into(), |r| format!("{r:.3}")),
        ns_text(pair_ns),
        ns_text(apart_ns),
        pct(smt_cost),
        placed.len(),
        ns_text(placed_ns),
        pct(slowdown),
        placement_name(&snap.info),
    );
    Ok(0)
}
