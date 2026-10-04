//! CPU frequency control and core-type topology.
//!
//! HWP is enabled and asked for autonomous selection between the CPU's own
//! limits at the configured energy-performance preference; the APERF/MPERF
//! pair is sampled so userland can tell the clock a CPU actually ran at; and
//! each CPU's core type and SMT position rank it for placement on a hybrid
//! part. Every register involved is per logical CPU and reachable only from
//! that CPU, so each CPU programs and records its own: at bring-up
//! ([`init_this_cpu`]) and at every timer tick and idle entry ([`on_tick`],
//! [`on_idle`]). A settings change ([`control`]) is a generation bump each CPU
//! applies there; the caller wakes the others so an idle one does not keep the
//! old settings.
//!
//! Register access is gated on CPUID alone: an MSR a CPU does not enumerate is
//! a #GP. `IA32_MISC_ENABLE`, `IA32_PERF_CTL` and `MSR_PLATFORM_INFO` are read
//! only on an Intel part, and the last only on bare metal, where it exists on
//! every part with HWP or Enhanced SpeedStep.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use slopos_abi::errno::Errno;
use slopos_abi::syscall::{
    CPU_PERF_F_APERF_MPERF, CPU_PERF_F_DTS, CPU_PERF_F_EIST, CPU_PERF_F_HWP, CPU_PERF_F_HWP_ACTIVE,
    CPU_PERF_F_HWP_EPP, CPU_PERF_F_HWP_FIRMWARE, CPU_PERF_F_HYBRID, CPU_PERF_F_HYPERVISOR,
    CPU_PERF_F_PTM, CPU_PERF_F_TURBO, CPU_PERF_F_TURBO_DISABLED, CPU_PERF_OP_EPP, CPU_PERF_OP_HWP,
    CPU_PERF_OP_LIMITS, CPU_PERF_OP_PLACEMENT, UserCpuPerf, UserCpuPerfInfo,
};
use slopos_arch::MAX_CPUS;
use slopos_arch::cpu::cpuid::{cpu_vendor_string, cpuid, cpuid_count, hypervisor_present};
use slopos_arch::cpu::msr::{Msr, read_msr, write_msr};
use slopos_arch::tsc::rdtsc;
use slopos_cpufreq_core::cpuid::{has_eist, is_hybrid};
use slopos_cpufreq_core::freq::MISC_ENABLE_TURBO_DISABLE;
use slopos_cpufreq_core::hwp::autonomous_request;
use slopos_cpufreq_core::place::score;
use slopos_cpufreq_core::{
    Config, CoreType, CpuClass, HwpCaps, HwpRequest, Limits, Placement, Policy, PowerFeatures,
    SmtTopology,
};
use slopos_ostd::klog_info;

const PM_ENABLE_HWP: u64 = 1;

struct CpuState {
    online: AtomicBool,
    apic_id: AtomicU32,
    core_type: AtomicU8,
    smt_thread: AtomicU8,
    /// Whether the CPU shares its core with another hardware thread.
    has_sibling: AtomicBool,
    core_id: AtomicU32,
    applied: AtomicU32,
    aperf: AtomicU64,
    mperf: AtomicU64,
    tsc: AtomicU64,
    hwp_caps: AtomicU64,
    hwp_request: AtomicU64,
    boot_hwp_request: AtomicU64,
    boot_perf_ctl: AtomicU64,
    therm_status: AtomicU64,
}

impl CpuState {
    const fn new() -> Self {
        Self {
            online: AtomicBool::new(false),
            apic_id: AtomicU32::new(0),
            core_type: AtomicU8::new(0),
            smt_thread: AtomicU8::new(0),
            has_sibling: AtomicBool::new(false),
            core_id: AtomicU32::new(0),
            applied: AtomicU32::new(0),
            aperf: AtomicU64::new(0),
            mperf: AtomicU64::new(0),
            tsc: AtomicU64::new(0),
            hwp_caps: AtomicU64::new(0),
            hwp_request: AtomicU64::new(0),
            boot_hwp_request: AtomicU64::new(0),
            boot_perf_ctl: AtomicU64::new(0),
            therm_status: AtomicU64::new(0),
        }
    }
}

static CPUS: [CpuState; MAX_CPUS] = [const { CpuState::new() }; MAX_CPUS];

/// What the boot CPU found: `CPU_PERF_F_*` bits the hardware enumerates, and
/// the firmware's registers before the kernel wrote any.
static DETECTED: AtomicBool = AtomicBool::new(false);
static FLAGS: AtomicU32 = AtomicU32::new(0);
/// `CPUID.06H:EAX[8]`, kept apart from `FLAGS`, which userland reads.
static HWP_NOTIFY: AtomicBool = AtomicBool::new(false);
static BOOT_PM_ENABLE: AtomicU64 = AtomicU64::new(0);
static BOOT_MISC_ENABLE: AtomicU64 = AtomicU64::new(0);
static PLATFORM_INFO: AtomicU64 = AtomicU64::new(0);
static CPUID16: AtomicU64 = AtomicU64::new(0);
static TEMPERATURE_TARGET: AtomicU64 = AtomicU64::new(0);
static PACKAGE_THERM_STATUS: AtomicU64 = AtomicU64::new(0);

/// The settings every CPU converges on; `GENERATION` names a version of them.
static POLICY: AtomicU32 = AtomicU32::new(Policy::Hwp.raw());
static EPP: AtomicU32 = AtomicU32::new(Config::DEFAULT.epp as u32);
static LIMITS: AtomicU32 = AtomicU32::new(0);
static PLACEMENT: AtomicU32 = AtomicU32::new(Placement::Ranked.raw());
static GENERATION: AtomicU32 = AtomicU32::new(1);
/// Set once two online CPUs differ in class or share a core, the only case in
/// which ranking idle CPUs changes a choice.
static HETEROGENEOUS: AtomicBool = AtomicBool::new(false);

/// The command line's settings; before any CPU is initialised.
pub fn configure(config: Config) {
    POLICY.store(config.policy.raw(), Ordering::Relaxed);
    EPP.store(config.epp as u32, Ordering::Relaxed);
    PLACEMENT.store(config.placement.raw(), Ordering::Relaxed);
}

fn max_leaf() -> u32 {
    cpuid(0).0
}

fn power_features() -> PowerFeatures {
    if max_leaf() < PowerFeatures::LEAF {
        return PowerFeatures::default();
    }
    let (eax, _, ecx, _) = cpuid(PowerFeatures::LEAF);
    PowerFeatures::from_cpuid6(eax, ecx)
}

fn has(flag: u32) -> bool {
    FLAGS.load(Ordering::Relaxed) & flag != 0
}

/// Read what the hardware enumerates and what the firmware left, once, on the
/// boot CPU before it writes anything.
fn detect() {
    let features = power_features();
    let intel = &cpu_vendor_string()[..12] == b"GenuineIntel";
    let hypervisor = hypervisor_present();
    let leaf = max_leaf();
    let mut flags = 0;
    let mut set = |on: bool, flag: u32| {
        if on {
            flags |= flag;
        }
    };
    set(features.aperf_mperf, CPU_PERF_F_APERF_MPERF);
    set(features.hwp, CPU_PERF_F_HWP);
    set(features.hwp && features.hwp_epp, CPU_PERF_F_HWP_EPP);
    set(features.turbo, CPU_PERF_F_TURBO);
    set(hypervisor, CPU_PERF_F_HYPERVISOR);
    // A hypervisor that enumerates a thermal sensor need not emulate its MSRs.
    set(features.dts && !hypervisor, CPU_PERF_F_DTS);
    set(features.ptm && !hypervisor, CPU_PERF_F_PTM);
    set(intel && has_eist(cpuid(1).2), CPU_PERF_F_EIST);
    set(
        leaf >= 7 && is_hybrid(cpuid_count(7, 0).3),
        CPU_PERF_F_HYBRID,
    );

    if features.hwp {
        let pm_enable = read_msr(Msr::PM_ENABLE);
        BOOT_PM_ENABLE.store(pm_enable, Ordering::Relaxed);
        if pm_enable & PM_ENABLE_HWP != 0 {
            flags |= CPU_PERF_F_HWP_FIRMWARE;
        }
    }
    if intel {
        let misc = read_msr(Msr::MISC_ENABLE);
        BOOT_MISC_ENABLE.store(misc, Ordering::Relaxed);
        if misc & MISC_ENABLE_TURBO_DISABLE != 0 {
            flags |= CPU_PERF_F_TURBO_DISABLED;
        }
        if !hypervisor && (features.hwp || flags & CPU_PERF_F_EIST != 0) {
            PLATFORM_INFO.store(read_msr(Msr::PLATFORM_INFO), Ordering::Relaxed);
        }
        if flags & CPU_PERF_F_DTS != 0 {
            TEMPERATURE_TARGET.store(read_msr(Msr::TEMPERATURE_TARGET), Ordering::Relaxed);
        }
    }
    if leaf >= 0x16 {
        let (base, max, bus, _) = cpuid(0x16);
        let packed =
            (base & 0xFFFF) as u64 | ((max & 0xFFFF) as u64) << 16 | ((bus & 0xFFFF) as u64) << 32;
        CPUID16.store(packed, Ordering::Relaxed);
    }
    HWP_NOTIFY.store(features.hwp_notify, Ordering::Relaxed);
    FLAGS.store(flags, Ordering::Relaxed);
    DETECTED.store(true, Ordering::Release);
}

fn topology(apic_id: u32) -> (CoreType, SmtTopology) {
    let leaf = max_leaf();
    let core_type = if has(CPU_PERF_F_HYBRID) && leaf >= CoreType::LEAF {
        CoreType::from_cpuid1a(cpuid(CoreType::LEAF).0)
    } else {
        CoreType::Unreported
    };
    let smt = if leaf >= SmtTopology::LEAF {
        let (eax, ebx, ecx, edx) = cpuid_count(SmtTopology::LEAF, 0);
        SmtTopology::from_cpuid_b(eax, ebx, ecx, edx)
    } else {
        None
    };
    (core_type, smt.unwrap_or(SmtTopology::single(apic_id)))
}

fn class_of(cpu: usize) -> CpuClass {
    let core_type = CoreType::from_raw(CPUS[cpu].core_type.load(Ordering::Relaxed));
    CpuClass::of(has(CPU_PERF_F_HYBRID), core_type)
}

/// Link `cpu` with any online sibling and note whether the CPUs now differ.
fn note_topology(cpu: usize) {
    let core = CPUS[cpu].core_id.load(Ordering::Relaxed);
    let class = class_of(cpu);
    for other in 0..MAX_CPUS {
        if other == cpu || !CPUS[other].online.load(Ordering::Acquire) {
            continue;
        }
        if CPUS[other].core_id.load(Ordering::Relaxed) == core {
            CPUS[other].has_sibling.store(true, Ordering::Relaxed);
            CPUS[cpu].has_sibling.store(true, Ordering::Relaxed);
            HETEROGENEOUS.store(true, Ordering::Relaxed);
        }
        if class_of(other) != class {
            HETEROGENEOUS.store(true, Ordering::Relaxed);
        }
    }
}

/// Record and program the calling CPU. The boot CPU calls it first, with the
/// command line already applied; each AP calls it during its bring-up.
pub fn init_this_cpu(cpu: usize, apic_id: u32) {
    if cpu >= MAX_CPUS {
        return;
    }
    if !DETECTED.load(Ordering::Acquire) {
        detect();
    }
    let state = &CPUS[cpu];
    let (core_type, smt) = topology(apic_id);
    state.apic_id.store(apic_id, Ordering::Relaxed);
    state.core_type.store(core_type.raw(), Ordering::Relaxed);
    state.smt_thread.store(smt.thread as u8, Ordering::Relaxed);
    state.core_id.store(smt.core, Ordering::Relaxed);

    if has(CPU_PERF_F_EIST) {
        state
            .boot_perf_ctl
            .store(read_msr(Msr::PERF_CTL), Ordering::Relaxed);
    }
    // Only a firmware that enabled HWP itself left a request of its own: on an
    // AP whose package the boot CPU enabled, the register holds its reset value.
    if has(CPU_PERF_F_HWP_FIRMWARE) {
        state
            .boot_hwp_request
            .store(read_msr(Msr::HWP_REQUEST), Ordering::Relaxed);
    }

    apply_this_cpu(cpu);
    sample_this_cpu(cpu);
    state.online.store(true, Ordering::Release);
    note_topology(cpu);

    if cpu == 0 {
        log_boot_state();
    }
    let caps = HwpCaps::from_msr(state.hwp_caps.load(Ordering::Relaxed));
    klog_info!(
        "CPUFREQ: cpu {} apic {} type {} core {} thread {} hwp {}/{}/{}/{} request 0x{:x}",
        cpu,
        apic_id,
        core_type.name(),
        smt.core,
        smt.thread,
        caps.highest,
        caps.guaranteed,
        caps.efficient,
        caps.lowest,
        state.hwp_request.load(Ordering::Relaxed),
    );
}

fn log_boot_state() {
    let flags = FLAGS.load(Ordering::Relaxed);
    let policy = Policy::from_raw(POLICY.load(Ordering::Relaxed)).unwrap_or(Policy::Hwp);
    klog_info!(
        "CPUFREQ: hwp={} epp={} aperf/mperf={} hybrid={} firmware: hwp={} turbo_disabled={} perf_ctl=0x{:x}; policy {}, epp 0x{:x}",
        flags & CPU_PERF_F_HWP != 0,
        flags & CPU_PERF_F_HWP_EPP != 0,
        flags & CPU_PERF_F_APERF_MPERF != 0,
        flags & CPU_PERF_F_HYBRID != 0,
        flags & CPU_PERF_F_HWP_FIRMWARE != 0,
        flags & CPU_PERF_F_TURBO_DISABLED != 0,
        CPUS[0].boot_perf_ctl.load(Ordering::Relaxed),
        policy.name(),
        EPP.load(Ordering::Relaxed),
    );
}

fn hwp_active() -> bool {
    has(CPU_PERF_F_HWP) && read_msr(Msr::PM_ENABLE) & PM_ENABLE_HWP != 0
}

/// Bring the calling CPU to the current settings generation.
fn apply_this_cpu(cpu: usize) {
    let generation = GENERATION.load(Ordering::Acquire);
    let state = &CPUS[cpu];
    let policy = Policy::from_raw(POLICY.load(Ordering::Relaxed)).unwrap_or(Policy::Firmware);
    if policy == Policy::Hwp && has(CPU_PERF_F_HWP) {
        if read_msr(Msr::PM_ENABLE) & PM_ENABLE_HWP == 0 {
            // HWP's change notifications stay off: nothing here services them.
            if HWP_NOTIFY.load(Ordering::Relaxed) {
                write_msr(Msr::HWP_INTERRUPT, 0);
            }
            write_msr(Msr::PM_ENABLE, PM_ENABLE_HWP);
        }
        // Readable only once HWP is enabled.
        let caps = read_msr(Msr::HWP_CAPABILITIES);
        let turbo = BOOT_MISC_ENABLE.load(Ordering::Relaxed) & MISC_ENABLE_TURBO_DISABLE == 0;
        let current = read_msr(Msr::HWP_REQUEST);
        let mut request = autonomous_request(
            HwpCaps::from_msr(caps),
            EPP.load(Ordering::Relaxed) as u8,
            Limits::from_packed(LIMITS.load(Ordering::Relaxed) as u64),
            turbo,
        );
        if !has(CPU_PERF_F_HWP_EPP) {
            request.epp = HwpRequest::from_msr(current).epp;
        }
        let raw = request.to_msr();
        if raw != current {
            write_msr(Msr::HWP_REQUEST, raw);
        }
        state.hwp_caps.store(caps, Ordering::Relaxed);
        state.hwp_request.store(raw, Ordering::Relaxed);
    } else if has(CPU_PERF_F_HWP) && hwp_active() {
        state
            .hwp_caps
            .store(read_msr(Msr::HWP_CAPABILITIES), Ordering::Relaxed);
        state
            .hwp_request
            .store(read_msr(Msr::HWP_REQUEST), Ordering::Relaxed);
    }
    state.applied.store(generation, Ordering::Release);
}

fn sample_this_cpu(cpu: usize) {
    let state = &CPUS[cpu];
    if has(CPU_PERF_F_APERF_MPERF) {
        let tsc = rdtsc();
        state.aperf.store(read_msr(Msr::APERF), Ordering::Relaxed);
        state.mperf.store(read_msr(Msr::MPERF), Ordering::Relaxed);
        state.tsc.store(tsc, Ordering::Relaxed);
    }
    if has(CPU_PERF_F_DTS) {
        state
            .therm_status
            .store(read_msr(Msr::THERM_STATUS), Ordering::Relaxed);
    }
    if cpu == 0 && has(CPU_PERF_F_PTM) {
        PACKAGE_THERM_STATUS.store(read_msr(Msr::PACKAGE_THERM_STATUS), Ordering::Relaxed);
    }
}

#[inline]
fn catch_up(cpu: usize) {
    if cpu >= MAX_CPUS || !CPUS[cpu].online.load(Ordering::Acquire) {
        return;
    }
    if CPUS[cpu].applied.load(Ordering::Relaxed) != GENERATION.load(Ordering::Acquire) {
        apply_this_cpu(cpu);
    }
    sample_this_cpu(cpu);
}

/// From the calling CPU's timer tick.
#[inline]
pub fn on_tick(cpu: usize) {
    catch_up(cpu);
}

/// From the calling CPU's idle loop, just before it halts: the counters stop
/// while it is halted, so this sample stays current until it runs again.
#[inline]
pub fn on_idle(cpu: usize) {
    catch_up(cpu);
}

/// One `CPU_PERF_OP_*` change. Answers the generation every CPU converges on;
/// the calling CPU has applied it on return, the others at their next tick or
/// idle entry, each of which is woken for it.
pub fn control(op: u64, value: u64) -> Result<u32, Errno> {
    match op {
        CPU_PERF_OP_HWP => {
            if !has(CPU_PERF_F_HWP) {
                return Err(Errno::EOPNOTSUPP);
            }
            POLICY.store(Policy::Hwp.raw(), Ordering::Relaxed);
        }
        CPU_PERF_OP_EPP => {
            if !has(CPU_PERF_F_HWP_EPP) {
                return Err(Errno::EOPNOTSUPP);
            }
            let epp = u8::try_from(value).map_err(|_| Errno::EINVAL)?;
            EPP.store(epp as u32, Ordering::Relaxed);
        }
        CPU_PERF_OP_LIMITS => {
            if !has(CPU_PERF_F_HWP) {
                return Err(Errno::EOPNOTSUPP);
            }
            let packed = u16::try_from(value).map_err(|_| Errno::EINVAL)?;
            LIMITS.store(packed as u32, Ordering::Relaxed);
        }
        CPU_PERF_OP_PLACEMENT => {
            let placement = u32::try_from(value)
                .ok()
                .and_then(Placement::from_raw)
                .ok_or(Errno::EINVAL)?;
            PLACEMENT.store(placement.raw(), Ordering::Relaxed);
        }
        _ => return Err(Errno::EINVAL),
    }
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel).wrapping_add(1);

    let flags = slopos_arch::cpu::save_flags_cli();
    let here = slopos_arch::pcr::get_current_cpu();
    catch_up(here);
    slopos_arch::cpu::restore_flags(flags);
    for cpu in 0..slopos_arch::pcr::get_cpu_count() {
        if cpu != here && CPUS[cpu].online.load(Ordering::Acquire) {
            crate::lifecycle::send_reschedule_ipi(cpu);
        }
    }
    Ok(generation)
}

/// Whether placement ranks idle CPUs: asked for, and the CPUs differ.
#[inline]
pub fn ranking_active() -> bool {
    PLACEMENT.load(Ordering::Relaxed) == Placement::Ranked.raw()
        && HETEROGENEOUS.load(Ordering::Relaxed)
}

/// `cpu`'s placement score, given whether a CPU is running something now.
pub fn placement_score(cpu: usize, busy: impl Fn(usize) -> bool) -> u32 {
    if cpu >= MAX_CPUS {
        return 0;
    }
    let state = &CPUS[cpu];
    let sibling_busy = state.has_sibling.load(Ordering::Relaxed) && {
        let core = state.core_id.load(Ordering::Relaxed);
        (0..slopos_arch::pcr::get_cpu_count()).any(|other| {
            other != cpu
                && CPUS[other].online.load(Ordering::Acquire)
                && CPUS[other].core_id.load(Ordering::Relaxed) == core
                && busy(other)
        })
    };
    let highest = HwpCaps::from_msr(state.hwp_caps.load(Ordering::Relaxed)).highest;
    score(class_of(cpu), sibling_busy, highest)
}

/// What `cpu_perf` reports about the machine.
pub fn info() -> UserCpuPerfInfo {
    let mut flags = FLAGS.load(Ordering::Relaxed);
    if CPUS
        .iter()
        .any(|cpu| cpu.online.load(Ordering::Acquire) && cpu.hwp_caps.load(Ordering::Relaxed) != 0)
    {
        flags |= CPU_PERF_F_HWP_ACTIVE;
    }
    let cpuid16 = CPUID16.load(Ordering::Relaxed);
    UserCpuPerfInfo {
        flags,
        policy: POLICY.load(Ordering::Relaxed),
        epp: EPP.load(Ordering::Relaxed),
        limits: LIMITS.load(Ordering::Relaxed),
        placement: PLACEMENT.load(Ordering::Relaxed),
        generation: GENERATION.load(Ordering::Acquire),
        tsc_khz: slopos_kernel_services::clock::measured_tsc_khz(),
        base_mhz: (cpuid16 & 0xFFFF) as u32,
        max_mhz: ((cpuid16 >> 16) & 0xFFFF) as u32,
        bus_mhz: ((cpuid16 >> 32) & 0xFFFF) as u32,
        _pad0: 0,
        boot_pm_enable: BOOT_PM_ENABLE.load(Ordering::Relaxed),
        boot_misc_enable: BOOT_MISC_ENABLE.load(Ordering::Relaxed),
        platform_info: PLATFORM_INFO.load(Ordering::Relaxed),
        temperature_target: TEMPERATURE_TARGET.load(Ordering::Relaxed),
        package_therm_status: PACKAGE_THERM_STATUS.load(Ordering::Relaxed),
    }
}

/// What `cpu_perf` reports about `cpu`.
pub fn cpu_record(cpu: usize) -> UserCpuPerf {
    let Some(state) = CPUS.get(cpu) else {
        return UserCpuPerf::default();
    };
    UserCpuPerf {
        cpu: cpu as u32,
        apic_id: state.apic_id.load(Ordering::Relaxed),
        core_type: state.core_type.load(Ordering::Relaxed),
        smt_thread: state.smt_thread.load(Ordering::Relaxed),
        online: state.online.load(Ordering::Acquire) as u8,
        _pad0: 0,
        core_id: state.core_id.load(Ordering::Relaxed),
        applied: state.applied.load(Ordering::Acquire),
        _pad1: 0,
        aperf: state.aperf.load(Ordering::Relaxed),
        mperf: state.mperf.load(Ordering::Relaxed),
        tsc: state.tsc.load(Ordering::Relaxed),
        hwp_caps: state.hwp_caps.load(Ordering::Relaxed),
        hwp_request: state.hwp_request.load(Ordering::Relaxed),
        boot_hwp_request: state.boot_hwp_request.load(Ordering::Relaxed),
        boot_perf_ctl: state.boot_perf_ctl.load(Ordering::Relaxed),
        therm_status: state.therm_status.load(Ordering::Relaxed),
    }
}

/// `cpu`'s last APERF/MPERF/TSC sample, for the profiler's report.
pub fn sample(cpu: usize) -> Option<(u64, u64, u64)> {
    let state = CPUS.get(cpu)?;
    if !state.online.load(Ordering::Acquire) || !has(CPU_PERF_F_APERF_MPERF) {
        return None;
    }
    Some((
        state.aperf.load(Ordering::Relaxed),
        state.mperf.load(Ordering::Relaxed),
        state.tsc.load(Ordering::Relaxed),
    ))
}

/// `cpu`'s core type, for the profiler's report.
pub fn core_type(cpu: usize) -> CoreType {
    CPUS.get(cpu).map_or(CoreType::Unreported, |state| {
        CoreType::from_raw(state.core_type.load(Ordering::Relaxed))
    })
}
