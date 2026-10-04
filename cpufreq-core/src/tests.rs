use crate::config::{Config, Policy};
use crate::cpuid::{CoreType, PowerFeatures, SmtTopology, is_hybrid};
use crate::freq::{busy_permille, effective_khz, max_non_turbo_ratio, perf_scaling_khz};
use crate::hwp::{
    EPP_BALANCE_PERFORMANCE, EPP_POWER, HwpCaps, HwpRequest, Limits, autonomous_request, epp_name,
    parse_epp,
};
use crate::place::{Candidate, CpuClass, Placement, pick_idle, score};
use crate::therm::{Condition, ThermStatus, throttle_point};

/// A Raptor Lake-P P-core's capabilities: highest 59, guaranteed 24,
/// efficient 8, lowest 1.
const P_CAPS: u64 = 0x0108_183B;

#[test]
fn power_features_read_their_bits() {
    let f = PowerFeatures::from_cpuid6(0x0000_0FC3, 0x0000_0009);
    assert!(f.dts && f.turbo && f.ptm);
    assert!(f.hwp && f.hwp_notify && f.hwp_epp && f.hwp_pkg);
    assert!(f.aperf_mperf && f.epb);
    assert_eq!(PowerFeatures::from_cpuid6(0, 0), PowerFeatures::default());
    let no_hwp = PowerFeatures::from_cpuid6(1 << 1, 1);
    assert!(!no_hwp.hwp && no_hwp.turbo && no_hwp.aperf_mperf && !no_hwp.dts);
}

#[test]
fn thermal_status_reads_conditions_and_temperature() {
    // Power-limited now, thermally throttled at some point, 37 degrees below
    // a 100 degree throttling point, reading valid.
    let raw = 1 << 31 | 37 << 16 | 1 << 10 | 1 << 11 | 1 << 1;
    let status = ThermStatus::from_msr(raw);
    assert_eq!(
        status.power_limit,
        Condition {
            now: true,
            since_boot: true
        }
    );
    assert_eq!(
        status.thermal,
        Condition {
            now: false,
            since_boot: true
        }
    );
    assert_eq!(status.prochot, Condition::default());
    assert_eq!(status.below_target, Some(37));
    let target = 100 << 16;
    assert_eq!(throttle_point(target), 100);
    assert_eq!(status.celsius(target), Some(63));
    // An invalid reading, or no known target, is no temperature.
    assert_eq!(ThermStatus::from_msr(37 << 16).celsius(target), None);
    assert_eq!(status.celsius(0), None);
}

#[test]
fn hybrid_core_types_decode() {
    assert!(is_hybrid(1 << 15));
    assert!(!is_hybrid(!(1 << 15)));
    assert_eq!(CoreType::from_cpuid1a(0x4000_0001), CoreType::Core);
    assert_eq!(CoreType::from_cpuid1a(0x2000_0001), CoreType::Atom);
    assert_eq!(CoreType::from_cpuid1a(0), CoreType::Unreported);
    assert_eq!(CoreType::from_cpuid1a(0x3000_0000), CoreType::Other(0x30));
    assert_eq!(CoreType::from_raw(CoreType::Atom.raw()), CoreType::Atom);
}

#[test]
fn smt_level_splits_the_x2apic_id() {
    // One SMT bit: x2APIC 9 is thread 1 of core 4.
    let t = SmtTopology::from_cpuid_b(1, 2, 0x100, 9).unwrap();
    assert_eq!((t.thread, t.core, t.x2apic_id), (1, 4, 9));
    // E-cores report a one-thread SMT level: shift 0, every ID its own core.
    let e = SmtTopology::from_cpuid_b(0, 1, 0x100, 0x12).unwrap();
    assert_eq!((e.thread, e.core), (0, 0x12));
    // A subleaf that is not the SMT level, or reports no processors, is no
    // topology at all.
    assert_eq!(SmtTopology::from_cpuid_b(1, 2, 0x200, 9), None);
    assert_eq!(SmtTopology::from_cpuid_b(1, 0, 0x100, 9), None);
}

#[test]
fn hwp_request_fields_round_trip() {
    let request = HwpRequest {
        min: 1,
        max: 59,
        desired: 0,
        epp: EPP_BALANCE_PERFORMANCE,
        activity_window: 0x3FF,
        package_control: true,
    };
    let raw = request.to_msr();
    assert_eq!(raw, 0x0000_07FF_8000_3B01);
    assert_eq!(HwpRequest::from_msr(raw), request);
    // Reserved bits above 42 are ignored, not folded into a field.
    assert_eq!(HwpRequest::from_msr(raw | 1 << 50), request);
}

#[test]
fn capabilities_decode_by_byte() {
    let caps = HwpCaps::from_msr(P_CAPS);
    assert_eq!(
        (caps.highest, caps.guaranteed, caps.efficient, caps.lowest),
        (59, 24, 8, 1)
    );
}

#[test]
fn autonomous_request_spans_the_capability_by_default() {
    let caps = HwpCaps::from_msr(P_CAPS);
    let request = autonomous_request(caps, EPP_BALANCE_PERFORMANCE, Limits::default(), true);
    assert_eq!((request.min, request.max, request.desired), (1, 59, 0));
    assert_eq!(request.epp, EPP_BALANCE_PERFORMANCE);
    assert!(!request.package_control);
}

#[test]
fn turbo_disabled_caps_at_guaranteed() {
    let caps = HwpCaps::from_msr(P_CAPS);
    let request = autonomous_request(caps, 0, Limits::default(), false);
    assert_eq!(request.max, 24);
    // An asked ceiling past guaranteed is clamped there too.
    let asked = autonomous_request(caps, 0, Limits { min: 0, max: 50 }, false);
    assert_eq!(asked.max, 24);
}

#[test]
fn limits_clamp_into_the_capability() {
    let caps = HwpCaps::from_msr(P_CAPS);
    let request = autonomous_request(caps, 0, Limits { min: 200, max: 30 }, true);
    // A floor above the ceiling is lowered to it rather than inverting them.
    assert_eq!((request.min, request.max), (30, 30));
    let pinned = autonomous_request(caps, 0, Limits { min: 59, max: 0 }, true);
    assert_eq!((pinned.min, pinned.max), (59, 59));
    assert_eq!(
        Limits::from_packed(Limits { min: 3, max: 40 }.packed()),
        Limits { min: 3, max: 40 }
    );
}

#[test]
fn epp_parses_names_and_bytes() {
    assert_eq!(parse_epp("performance"), Some(0));
    assert_eq!(parse_epp("balance_power"), Some(0xC0));
    assert_eq!(parse_epp("power"), Some(EPP_POWER));
    assert_eq!(parse_epp("102"), Some(102));
    assert_eq!(parse_epp("0x66"), Some(102));
    assert_eq!(parse_epp("256"), None);
    assert_eq!(parse_epp("fast"), None);
    assert_eq!(epp_name(0x80), Some("balance_performance"));
    assert_eq!(epp_name(0x66), None);
}

#[test]
fn effective_frequency_is_tsc_rate_scaled_by_aperf_over_mperf() {
    // A 1.9 GHz TSC, and a core that ran twice as fast as it while busy.
    assert_eq!(effective_khz(1_900_000, 2_000, 1_000), 3_800_000);
    assert_eq!(effective_khz(1_900_000, 500, 1_000), 950_000);
    assert_eq!(effective_khz(1_900_000, 7, 0), 0);
    // Counters large enough to overflow a u64 product stay exact.
    assert_eq!(
        effective_khz(4_000_000, u64::MAX / 2, u64::MAX / 2),
        4_000_000
    );
}

#[test]
fn busy_share_is_mperf_over_tsc() {
    assert_eq!(busy_permille(250, 1_000), 250);
    assert_eq!(busy_permille(0, 1_000), 0);
    // MPERF and the TSC are read a few cycles apart: never above one.
    assert_eq!(busy_permille(1_001, 1_000), 1000);
    assert_eq!(busy_permille(5, 0), 0);
}

#[test]
fn ratio_and_scaling_facts() {
    assert_eq!(max_non_turbo_ratio(0x0804_0838_1300), 0x13);
    assert_eq!(perf_scaling_khz(6, 0xBA, true), 78_741);
    assert_eq!(perf_scaling_khz(6, 0xBA, false), 100_000);
    assert_eq!(perf_scaling_khz(6, 0x55, true), 100_000);
    assert_eq!(perf_scaling_khz(0x19, 0xBA, true), 100_000);
}

#[test]
fn command_line_defaults_and_last_key_wins() {
    let default = Config::parse("root=auto quiet");
    assert_eq!(default, Config::DEFAULT);
    assert_eq!(default.policy, Policy::Hwp);
    assert_eq!(default.placement, Placement::Ranked);

    let appended = Config::parse(
        "cpufreq=hwp cpufreq.epp=power sched.hybrid=on cpufreq=firmware cpufreq.epp=0x10 sched.hybrid=off",
    );
    assert_eq!(appended.policy, Policy::Firmware);
    assert_eq!(appended.epp, 0x10);
    assert_eq!(appended.placement, Placement::Flat);
    assert_eq!(appended.rejected, 0);
}

#[test]
fn a_bad_value_keeps_the_default_and_says_so() {
    let config = Config::parse("cpufreq=turbo cpufreq.epp=fast sched.hybrid=maybe");
    assert_eq!(config.policy, Config::DEFAULT.policy);
    assert_eq!(config.epp, Config::DEFAULT.epp);
    assert_eq!(config.placement, Config::DEFAULT.placement);
    assert_eq!(
        config.rejected,
        Config::BAD_POLICY | Config::BAD_EPP | Config::BAD_PLACEMENT
    );
    // A key that merely starts like one of ours is someone else's.
    assert_eq!(Config::parse("cpufreqx=firmware"), Config::DEFAULT);
}

#[test]
fn idle_p_core_beats_e_core_beats_busy_sibling() {
    let p_idle = score(CpuClass::Performance, false, 46);
    let e_idle = score(CpuClass::Efficiency, false, 255);
    let p_shared = score(CpuClass::Performance, true, 59);
    assert!(p_idle > e_idle && e_idle > p_shared);
    // Within a tier the favoured core, the higher highest level, wins.
    assert!(score(CpuClass::Performance, false, 59) > p_idle);
    // A part that is not hybrid is all performance CPUs.
    assert_eq!(CpuClass::of(false, CoreType::Atom), CpuClass::Performance);
    assert_eq!(CpuClass::of(true, CoreType::Atom), CpuClass::Efficiency);
    assert_eq!(CpuClass::of(true, CoreType::Core), CpuClass::Performance);
}

fn cand(cpu: usize, idle: bool, score: u32) -> Candidate {
    Candidate { cpu, idle, score }
}

#[test]
fn pick_takes_the_best_idle_and_keeps_a_warm_equal() {
    let cpus = [
        cand(0, false, 900),
        cand(1, true, 700),
        cand(2, true, 800),
        cand(3, true, 800),
    ];
    assert_eq!(pick_idle(cpus.into_iter(), None), Some(2));
    // A warm CPU as good as the best keeps the task.
    assert_eq!(pick_idle(cpus.into_iter(), Some(3)), Some(3));
    // A warm CPU worse than an idle alternative loses it.
    assert_eq!(pick_idle(cpus.into_iter(), Some(1)), Some(2));
    // A busy warm CPU is no candidate.
    assert_eq!(pick_idle(cpus.into_iter(), Some(0)), Some(2));
    assert_eq!(pick_idle([cand(0, false, 1)].into_iter(), Some(0)), None);
}
