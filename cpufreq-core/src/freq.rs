//! Frequency arithmetic: effective frequency from the APERF/MPERF pair, and
//! the ratio fields of the legacy registers.
//!
//! `IA32_MPERF` (`0xE7`) counts at the TSC's rate and `IA32_APERF` (`0xE8`) at
//! the core's actual clock, both only while the CPU executes (C0). Over an
//! interval, `TSC rate * dAPERF / dMPERF` is the average clock while busy, and
//! `dMPERF / dTSC` the share of the interval spent busy.

/// Average clock while executing, in kHz; `0` when the CPU never ran.
pub const fn effective_khz(tsc_khz: u64, d_aperf: u64, d_mperf: u64) -> u64 {
    if d_mperf == 0 {
        return 0;
    }
    let khz = tsc_khz as u128 * d_aperf as u128 / d_mperf as u128;
    if khz > u64::MAX as u128 {
        u64::MAX
    } else {
        khz as u64
    }
}

/// Share of the interval spent executing, in thousandths.
pub const fn busy_permille(d_mperf: u64, d_tsc: u64) -> u32 {
    if d_tsc == 0 {
        return 0;
    }
    let permille = d_mperf as u128 * 1000 / d_tsc as u128;
    if permille > 1000 {
        1000
    } else {
        permille as u32
    }
}

/// `MSR_PLATFORM_INFO` (`0xCE`) bits 15:8: the highest ratio without turbo.
pub const fn max_non_turbo_ratio(platform_info: u64) -> u8 {
    (platform_info >> 8) as u8
}

/// `IA32_PERF_CTL` (`0x199`) bits 15:8: the ratio legacy control asks for.
pub const fn perf_ctl_ratio(perf_ctl: u64) -> u8 {
    (perf_ctl >> 8) as u8
}

/// `IA32_MISC_ENABLE` (`0x1A0`) bit 38: turbo disabled (by firmware, usually).
pub const MISC_ENABLE_TURBO_DISABLE: u64 = 1 << 38;

/// The bus clock a ratio multiplies on every part with HWP.
pub const BUS_KHZ: u32 = 100_000;

/// kHz per HWP performance level. Hybrid parts scale a P-core's levels so the
/// two core types share one performance scale; the factors are the
/// per-generation values Linux reports these parts' frequencies with. An
/// E-core, and any part not listed, counts one level per bus-clock ratio.
pub const fn perf_scaling_khz(family: u8, model: u8, performance_core: bool) -> u32 {
    if family != 6 || !performance_core {
        return BUS_KHZ;
    }
    match model {
        // Alder Lake, Alder Lake-L, Raptor Lake, Raptor Lake-P, Raptor
        // Lake-S, Bartlett Lake.
        0x97 | 0x9A | 0xB7 | 0xBA | 0xBF | 0xD7 => 78_741,
        // Meteor Lake-L.
        0xAA => 80_000,
        // Lunar Lake-M.
        0xBD => 86_957,
        _ => BUS_KHZ,
    }
}
