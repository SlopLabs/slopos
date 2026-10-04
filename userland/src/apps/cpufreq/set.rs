//! `cpufreq set`: one `cpu_perf_ctl` change, waited for on every CPU.

use std::thread::sleep;
use std::time::{Duration, Instant};

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    CPU_PERF_F_HWP, CPU_PERF_F_HWP_EPP, CPU_PERF_OP_EPP, CPU_PERF_OP_HWP, CPU_PERF_OP_LIMITS,
    CPU_PERF_OP_PLACEMENT,
};
use slopos_cpufreq_core::{Limits, Placement, hwp};

use crate::syscall::core as sys_core;

use super::sample::{Snapshot, errno_text};
use super::status::print_settings;

/// How long every CPU gets to reach its next tick or idle entry.
const APPLY_TIMEOUT: Duration = Duration::from_secs(1);
const APPLY_POLL: Duration = Duration::from_millis(10);

fn level(text: &str) -> Result<u8, String> {
    text.parse()
        .map_err(|_| format!("an HWP level is 0-255 (0 is the CPU's own bound), not {text:?}"))
}

fn request(args: &[String]) -> Result<Option<(u64, u64, String)>, String> {
    Ok(Some(match args {
        [what] if what == "hwp" => (CPU_PERF_OP_HWP, 0, "HWP".into()),
        [what, value] if what == "epp" => {
            let epp = hwp::parse_epp(value).ok_or(format!(
                "an energy-performance preference is performance, balance_performance, \
                 balance_power, power or 0-255, not {value:?}"
            ))?;
            (CPU_PERF_OP_EPP, u64::from(epp), format!("epp {epp}"))
        }
        [what, min, max] if what == "limits" => {
            let limits = Limits {
                min: level(min)?,
                max: level(max)?,
            };
            (
                CPU_PERF_OP_LIMITS,
                limits.packed(),
                format!("limits {}-{}", limits.min, limits.max),
            )
        }
        [what, mode] if what == "placement" => {
            let placement = match mode.as_str() {
                "flat" => Placement::Flat,
                "ranked" => Placement::Ranked,
                _ => return Err(format!("placement is flat or ranked, not {mode:?}")),
            };
            (
                CPU_PERF_OP_PLACEMENT,
                u64::from(placement.raw()),
                format!("placement {}", placement.name()),
            )
        }
        _ => return Ok(None),
    }))
}

fn refusal(rc: i64, op: u64, what: &str) -> String {
    let errno = Errno::from_raw(rc as i32);
    if errno == Some(Errno::EPERM) {
        return "changing the CPUs' performance settings needs the Power grant: \
                run it as /bin/cpufreq from the shell or remoted"
            .into();
    }
    if errno == Some(Errno::EOPNOTSUPP) {
        let flags = Snapshot::take().map_or(0, |snap| snap.info.flags);
        return if flags & CPU_PERF_F_HWP == 0 {
            format!("{what}: this CPU has no HWP (hardware-controlled performance states)")
        } else if op == CPU_PERF_OP_EPP && flags & CPU_PERF_F_HWP_EPP == 0 {
            format!("{what}: this CPU's HWP takes no energy-performance preference")
        } else {
            format!("{what}: not supported on this CPU")
        };
    }
    if errno == Some(Errno::EINVAL) {
        return format!("{what}: the kernel refused the value as out of range");
    }
    format!("{what}: {}", errno_text(rc))
}

/// CPUs whose `applied` has not reached `generation`.
fn pending(snap: &Snapshot, generation: u32) -> Vec<u32> {
    snap.cpus
        .iter()
        .filter(|cpu| (cpu.applied.wrapping_sub(generation) as i32) < 0)
        .map(|cpu| cpu.cpu)
        .collect()
}

pub(super) fn set(args: &[String]) -> Result<i32, String> {
    let Some((op, value, what)) = request(args)? else {
        return Ok(super::usage());
    };
    let rc = sys_core::cpu_perf_ctl(op, value);
    if rc < 0 {
        return Err(refusal(rc, op, &what));
    }
    let generation = rc as u32;
    let started = Instant::now();
    let snap = loop {
        let snap = Snapshot::take()?;
        let late = pending(&snap, generation);
        if late.is_empty() {
            break snap;
        }
        if started.elapsed() >= APPLY_TIMEOUT {
            let late: Vec<String> = late.iter().map(u32::to_string).collect();
            return Err(format!(
                "{what}: generation {generation} not applied after {} ms by CPU {}",
                APPLY_TIMEOUT.as_millis(),
                late.join(",")
            ));
        }
        sleep(APPLY_POLL);
    };
    println!(
        "{what}: applied by {} CPUs in {} ms (generation {generation})",
        snap.cpus.len(),
        started.elapsed().as_millis()
    );
    print_settings(&snap.info);
    Ok(0)
}
