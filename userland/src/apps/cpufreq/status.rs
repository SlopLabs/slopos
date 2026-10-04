//! `cpufreq status` and `cpufreq watch`.

use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

use slopos_abi::syscall::{
    CPU_PERF_F_APERF_MPERF, CPU_PERF_F_DTS, CPU_PERF_F_EIST, CPU_PERF_F_HWP, CPU_PERF_F_HWP_ACTIVE,
    CPU_PERF_F_HWP_EPP, CPU_PERF_F_HWP_FIRMWARE, CPU_PERF_F_HYBRID, CPU_PERF_F_HYPERVISOR,
    CPU_PERF_F_PTM, CPU_PERF_F_TURBO, CPU_PERF_F_TURBO_DISABLED, CPU_PERF_POLICY_HWP,
};
use slopos_cpufreq_core::{CoreType, CpuClass, HwpCaps, HwpRequest, Limits, Placement, Policy};
use slopos_cpufreq_core::{freq, hwp, therm};

use crate::syscall::{UserCpuInfo, UserCpuPerf, UserCpuPerfInfo, core as sys_core};

use super::sample::{
    Limited, Snapshot, THROTTLE_LEGEND, Usage, fmt_celsius, fmt_opt, fmt_permille, throttle_text,
    type_name,
};

const FLAG_NAMES: [(u32, &str); 12] = [
    (CPU_PERF_F_APERF_MPERF, "aperf-mperf"),
    (CPU_PERF_F_HWP, "hwp"),
    (CPU_PERF_F_HWP_EPP, "hwp-epp"),
    (CPU_PERF_F_HWP_ACTIVE, "hwp-active"),
    (CPU_PERF_F_HWP_FIRMWARE, "hwp-firmware"),
    (CPU_PERF_F_HYBRID, "hybrid"),
    (CPU_PERF_F_TURBO, "turbo"),
    (CPU_PERF_F_TURBO_DISABLED, "turbo-disabled"),
    (CPU_PERF_F_EIST, "eist"),
    (CPU_PERF_F_HYPERVISOR, "hypervisor"),
    (CPU_PERF_F_DTS, "dts"),
    (CPU_PERF_F_PTM, "ptm"),
];

fn flag_list(flags: u32) -> String {
    let names: Vec<&str> = FLAG_NAMES
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|&(_, name)| name)
        .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(" ")
    }
}

pub(super) fn policy_name(info: &UserCpuPerfInfo) -> &'static str {
    Policy::from_raw(info.policy).map_or("-", Policy::name)
}

pub(super) fn placement_name(info: &UserCpuPerfInfo) -> &'static str {
    Placement::from_raw(info.placement).map_or("-", Placement::name)
}

/// The preference HWP runs at, `None` unless HWP is on and takes one.
pub(super) fn epp_in_effect(info: &UserCpuPerfInfo) -> Option<u8> {
    let wanted = CPU_PERF_F_HWP_ACTIVE | CPU_PERF_F_HWP_EPP;
    (info.flags & wanted == wanted && info.policy == CPU_PERF_POLICY_HWP).then_some(info.epp as u8)
}

fn epp_text(epp: u8) -> String {
    match hwp::epp_name(epp) {
        Some(name) => format!("{epp} ({name})"),
        None => epp.to_string(),
    }
}

fn limits_text(info: &UserCpuPerfInfo) -> String {
    let limits = Limits::from_packed(u64::from(info.limits));
    let bound = |level: u8, own: &str| {
        if level == 0 {
            own.to_owned()
        } else {
            level.to_string()
        }
    };
    format!(
        "min {}, max {}",
        bound(limits.min, "lowest"),
        bound(limits.max, "highest")
    )
}

/// Every setting `cpu_perf_ctl` changes, one per line.
pub(super) fn print_settings(info: &UserCpuPerfInfo) {
    let hwp = info.flags & CPU_PERF_F_HWP != 0;
    println!("policy:     {}", policy_name(info));
    println!(
        "epp:        {}",
        epp_in_effect(info).map_or_else(|| "-".into(), epp_text)
    );
    println!(
        "limits:     {}",
        if hwp { limits_text(info) } else { "-".into() }
    );
    println!("placement:  {}", placement_name(info));
    println!("generation: {}", info.generation);
}

fn mhz_or_dash(mhz: u32) -> String {
    if mhz == 0 {
        "-".into()
    } else {
        format!("{mhz} MHz")
    }
}

fn print_firmware(info: &UserCpuPerfInfo) {
    let hwp = if info.flags & CPU_PERF_F_HWP == 0 {
        "- (no HWP)"
    } else if info.flags & CPU_PERF_F_HWP_FIRMWARE != 0 {
        "enabled by the firmware"
    } else {
        "left off by the firmware"
    };
    println!("boot hwp:   {hwp}");
    // Turbo disabled in `IA32_MISC_ENABLE` also clears its CPUID bit.
    let turbo = if info.boot_misc_enable & freq::MISC_ENABLE_TURBO_DISABLE != 0 {
        "disabled by the firmware"
    } else if info.flags & CPU_PERF_F_TURBO != 0 {
        "enabled"
    } else {
        "- (no turbo)"
    };
    println!("boot turbo: {turbo}");
    let ratio = freq::max_non_turbo_ratio(info.platform_info);
    if info.platform_info == 0 || ratio == 0 {
        println!("boot ratio: -");
    } else {
        println!(
            "boot ratio: max non-turbo {ratio} ({} MHz)",
            u32::from(ratio) * freq::BUS_KHZ / 1000
        );
    }
}

/// kHz per HWP level of `cpu`.
fn level_khz(cpuid: &UserCpuInfo, snap: &Snapshot, cpu: &UserCpuPerf) -> u32 {
    let p_core = snap.hybrid() && CoreType::from_raw(cpu.core_type) == CoreType::Core;
    freq::perf_scaling_khz(cpuid.family, cpuid.model, p_core)
}

fn level_text(level: u8, khz: u32) -> String {
    format!("{level} ({})", u64::from(level) * u64::from(khz) / 1000)
}

fn request_text(request: u64, epp: bool) -> String {
    if request == 0 {
        return "-".into();
    }
    let request = HwpRequest::from_msr(request);
    if epp {
        format!("{}-{}/{}", request.min, request.max, request.epp)
    } else {
        format!("{}-{}", request.min, request.max)
    }
}

fn brand(cpuid: &UserCpuInfo) -> String {
    let end = cpuid
        .brand_string
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(cpuid.brand_string.len());
    String::from_utf8_lossy(&cpuid.brand_string[..end])
        .trim()
        .to_owned()
}

const TABLE_HEADER: &str = "CPU APIC T CORE.T  HIGHEST     GUARANTEED  EFFICIENT   LOWEST      REQUEST      GEN BOOT-CTL BOOT-HWP       MHZ   BUSY   TEMP THROTTLE";

fn print_package(snap: &Snapshot) {
    let Some(status) = snap.package_therm() else {
        println!("package:    -");
        return;
    };
    let target = therm::throttle_point(snap.info.temperature_target);
    let point = if target == 0 {
        String::new()
    } else {
        format!(" (throttles at {target} C)")
    };
    println!(
        "package:    {}{point}, throttle {}",
        fmt_celsius(snap.celsius(Some(status))),
        throttle_text(Some(status))
    );
}

/// `pkg:P 3:TP ...` for whatever is limited now, `none` when nothing is and
/// `-` without a thermal sensor.
fn limited_list(snap: &Snapshot) -> String {
    if !snap.thermal() {
        return "-".into();
    }
    let package = snap
        .package_therm()
        .map(Limited::now)
        .filter(|limited| !limited.is_empty())
        .map(|limited| format!("pkg:{}", limited.letters()));
    let cpus = snap.cpus.iter().filter_map(|cpu| {
        let limited = Limited::now(snap.cpu_therm(cpu)?);
        (!limited.is_empty()).then(|| format!("{}:{}", cpu.cpu, limited.letters()))
    });
    let list: Vec<String> = package.into_iter().chain(cpus).collect();
    if list.is_empty() {
        "none".into()
    } else {
        list.join(" ")
    }
}

pub(super) fn status() -> Result<i32, String> {
    let mut cpuid = UserCpuInfo::default();
    let _ = sys_core::cpu_info(&mut cpuid);
    let before = Snapshot::take()?;
    sleep(Duration::from_secs(1));
    let after = Snapshot::take()?;
    let info = &after.info;

    println!(
        "cpu:        {} (family {} model 0x{:02x} stepping {})",
        brand(&cpuid),
        cpuid.family,
        cpuid.model,
        cpuid.stepping
    );
    println!("flags:      {}", flag_list(info.flags));
    print_settings(info);
    if info.tsc_khz == 0 {
        println!("tsc:        -");
    } else {
        println!(
            "tsc:        {}.{:03} MHz",
            info.tsc_khz / 1000,
            info.tsc_khz % 1000
        );
    }
    println!(
        "cpuid.16h:  base {}, max {}, bus {}",
        mhz_or_dash(info.base_mhz),
        mhz_or_dash(info.max_mhz),
        mhz_or_dash(info.bus_mhz)
    );
    print_firmware(info);
    print_package(&after);
    println!();

    let hwp = info.flags & CPU_PERF_F_HWP != 0;
    let epp = info.flags & CPU_PERF_F_HWP_EPP != 0;
    println!("{TABLE_HEADER}");
    let mut classes = Classes::default();
    for cpu in &after.cpus {
        let usage = Usage::between(&before, &after, cpu.cpu);
        classes.add(after.class(cpu), &usage);
        let caps = if hwp && cpu.hwp_caps != 0 {
            let caps = HwpCaps::from_msr(cpu.hwp_caps);
            let khz = level_khz(&cpuid, &after, cpu);
            [caps.highest, caps.guaranteed, caps.efficient, caps.lowest].map(|l| level_text(l, khz))
        } else {
            ["-", "-", "-", "-"].map(String::from)
        };
        let boot_ctl = if cpu.boot_perf_ctl == 0 {
            "-".into()
        } else {
            freq::perf_ctl_ratio(cpu.boot_perf_ctl).to_string()
        };
        println!(
            "{:>3} {:>4} {:1} {:>6}  {:<11} {:<11} {:<11} {:<11} {:<11} {:>4} {:>8} {:<11} {:>6} {:>6} {:>6} {}",
            cpu.cpu,
            cpu.apic_id,
            type_name(cpu),
            format!("{}.{}", cpu.core_id, cpu.smt_thread),
            caps[0],
            caps[1],
            caps[2],
            caps[3],
            request_text(cpu.hwp_request, epp),
            cpu.applied,
            boot_ctl,
            request_text(cpu.boot_hwp_request, epp),
            fmt_opt(usage.eff_mhz(info)),
            fmt_permille(usage.busy_permille(info)),
            fmt_celsius(after.celsius(after.cpu_therm(cpu))),
            throttle_text(after.cpu_therm(cpu)),
        );
    }
    println!();
    println!("{}", classes.summary(info));
    println!("{THROTTLE_LEGEND}");
    Ok(0)
}

/// Busy-weighted sums by class.
#[derive(Default)]
pub(super) struct Classes {
    pub p: Usage,
    pub e: Usage,
    pub all: Usage,
    pub p_cpus: usize,
    pub e_cpus: usize,
}

impl Classes {
    pub fn add(&mut self, class: CpuClass, usage: &Usage) {
        match class {
            CpuClass::Performance => {
                self.p.add(usage);
                self.p_cpus += 1;
            }
            CpuClass::Efficiency => {
                self.e.add(usage);
                self.e_cpus += 1;
            }
        }
        self.all.add(usage);
    }

    fn line(info: &UserCpuPerfInfo, usage: &Usage, cpus: usize) -> String {
        if cpus == 0 {
            return "-".into();
        }
        format!(
            "{} MHz {} busy",
            fmt_opt(usage.eff_mhz(info)),
            fmt_permille(usage.busy_permille(info))
        )
    }

    fn summary(&self, info: &UserCpuPerfInfo) -> String {
        format!(
            "all {} | P {} | E {}",
            Self::line(info, &self.all, self.p_cpus + self.e_cpus),
            Self::line(info, &self.p, self.p_cpus),
            Self::line(info, &self.e, self.e_cpus)
        )
    }
}

pub(super) fn watch(interval_ms: u64, count: Option<u64>) -> Result<i32, String> {
    const PER_LINE: usize = 4;
    let started = Instant::now();
    let mut previous = Snapshot::take()?;
    let mut shown = 0;
    while count.is_none_or(|count| shown < count) {
        sleep(Duration::from_millis(interval_ms));
        let now = Snapshot::take()?;
        let info = &now.info;
        let mut classes = Classes::default();
        let mut cells = Vec::with_capacity(now.cpus.len());
        for cpu in &now.cpus {
            let usage = Usage::between(&previous, &now, cpu.cpu);
            classes.add(now.class(cpu), &usage);
            cells.push(format!(
                "{:>3} {} {:>5} {:>6}",
                cpu.cpu,
                type_name(cpu),
                fmt_opt(usage.eff_mhz(info)),
                fmt_permille(usage.busy_permille(info))
            ));
        }
        let elapsed = started.elapsed();
        println!(
            "{:>4}.{:02}s pkg {} | {} | limited {}",
            elapsed.as_secs(),
            elapsed.subsec_millis() / 10,
            fmt_celsius(now.celsius(now.package_therm())),
            classes.summary(info),
            limited_list(&now)
        );
        for row in cells.chunks(PER_LINE) {
            println!("  {}", row.join(" |"));
        }
        let _ = std::io::stdout().flush();
        previous = now;
        shown += 1;
    }
    Ok(0)
}
