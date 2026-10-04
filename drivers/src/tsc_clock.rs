//! The monotonic clock, read off the TSC once the TSC has been measured
//! against the HPET.
//!
//! Every idle-loop iteration, every sleep and every `clock_gettime` reads the
//! monotonic clock, and an HPET read is an MMIO access — under a hypervisor a
//! VM exit to the device model, microseconds each. The TSC is a register read.
//! It is only trusted where it runs at a constant rate across every CPU: an
//! invariant TSC (CPUID `0x8000_0007` EDX bit 8), or KVM's word that its TSC
//! is stable (`KVM_FEATURE_CLOCKSOURCE_STABLE_BIT`). Anywhere else the HPET
//! stays the clock.
//!
//! Each CPU reads its own TSC, so the clock is one clock only while every
//! CPU's TSC reads the same, and a laptop's BSP was measured 884 ms behind its
//! APs. A write to one CPU's `IA32_TIME_STAMP_COUNTER` moves that CPU's
//! `IA32_TSC_ADJUST` by the same amount, so every CPU zeroes it before reading
//! the clock, and each AP is then held to the BSP at bring-up; one that still
//! disagrees hands the clock back to the HPET.

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use slopos_arch::MAX_CPUS;
use slopos_arch::cpu::cpuid::{cpuid, cpuid_count};
use slopos_arch::cpu::msr::{Msr, read_msr, write_msr};
use slopos_arch::tsc::rdtsc;
use slopos_ostd::klog_info;
use slopos_ostd::sync::InitFlag;

use crate::hpet;

/// How long the calibration measures: the error is the HPET read latency
/// over this span.
const CALIBRATION_MS: u32 = 100;

const CPUID_LEAF_POWER: u32 = 0x8000_0007;
const INVARIANT_TSC: u32 = 1 << 8;
const CPUID_HYPERVISOR_BASE: u32 = 0x4000_0000;
const KVM_SIGNATURE: [u32; 3] = [0x4b4d_564b, 0x564b_4d56, 0x0000_004d];
const KVM_FEATURE_CLOCKSOURCE_STABLE: u32 = 1 << 24;
/// CPUID `(7, 0)` EBX: `IA32_TSC_ADJUST` exists.
const TSC_ADJUST_SUPPORTED: u32 = 1 << 1;
/// How far two TSCs may seem apart before they are called out of step: the
/// cache-line hand-off between the readings, with room for an unordered
/// `RDTSC`. Tens of microseconds at laptop clocks.
const SYNC_SLACK_CYCLES: u64 = 100_000;

static READY: InitFlag = InitFlag::new();
/// Nanoseconds per TSC cycle, as a 32.32 fixed-point fraction.
static MULT: AtomicU64 = AtomicU64::new(0);
static BASE_TSC: AtomicU64 = AtomicU64::new(0);
static BASE_NS: AtomicU64 = AtomicU64::new(0);

/// Set once some CPU's TSC was found out of step; the HPET answers from then
/// on, offset by `HPET_BIAS_NS` so the clock does not step back.
static UNSTABLE: AtomicBool = AtomicBool::new(false);
static UNSTABLE_CLAIMED: AtomicBool = AtomicBool::new(false);
static HPET_BIAS_NS: AtomicU64 = AtomicU64::new(0);

/// The BSP's latest TSC reading, published while it waits for the APs.
static BSP_TSC: AtomicU64 = AtomicU64::new(0);
/// Each AP's TSC as it entered, for the BSP to compare against its own.
static AP_TSC: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// What each CPU's `IA32_TSC_ADJUST` held before it was zeroed.
static FOUND_ADJUST: [AtomicI64; MAX_CPUS] = [const { AtomicI64::new(0) }; MAX_CPUS];
/// How far each AP was found behind the BSP's published TSC, in cycles.
static FOUND_BEHIND: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

fn tsc_is_stable() -> bool {
    let (max_ext, _, _, _) = cpuid(0x8000_0000);
    if max_ext >= CPUID_LEAF_POWER && cpuid(CPUID_LEAF_POWER).3 & INVARIANT_TSC != 0 {
        return true;
    }
    let (max_hv, ebx, ecx, edx) = cpuid(CPUID_HYPERVISOR_BASE);
    if [ebx, ecx, edx] != KVM_SIGNATURE || max_hv < CPUID_HYPERVISOR_BASE + 1 {
        return false;
    }
    cpuid(CPUID_HYPERVISOR_BASE + 1).0 & KVM_FEATURE_CLOCKSOURCE_STABLE != 0
}

/// A TSC reading and the HPET time it was taken at.
fn paired_sample() -> (u64, u64) {
    let before = hpet::read_counter();
    let tsc = rdtsc();
    let after = hpet::read_counter();
    (tsc, hpet::nanoseconds(before + (after - before) / 2))
}

/// Zero this CPU's `IA32_TSC_ADJUST`, keeping what firmware left there for
/// the boot log. Each CPU runs it before it first reads the clock: the BSP
/// first thing in boot, an AP first thing on entry.
pub fn sanitize_this_cpu(cpu: usize) {
    let (max_leaf, _, _, _) = cpuid(0);
    if max_leaf < 7 || cpuid_count(7, 0).1 & TSC_ADJUST_SUPPORTED == 0 {
        return;
    }
    let found = read_msr(Msr::TSC_ADJUST) as i64;
    if found == 0 {
        return;
    }
    write_msr(Msr::TSC_ADJUST, 0);
    if let Some(slot) = FOUND_ADJUST.get(cpu) {
        slot.store(found, Ordering::Relaxed);
    }
}

/// The BSP's side of the bring-up check: its current TSC, which no AP's may
/// read behind. Published before the APs are started and while it waits.
pub fn publish_bsp_tsc() {
    BSP_TSC.store(rdtsc(), Ordering::Release);
}

/// An AP's first check, on entry after [`sanitize_this_cpu`]: its TSC must
/// not read behind what the BSP published earlier.
pub fn ap_check_behind(cpu: usize) {
    let published = BSP_TSC.load(Ordering::Acquire);
    let mine = rdtsc();
    if published != 0
        && mine.saturating_add(SYNC_SLACK_CYCLES) < published
        && let Some(slot) = FOUND_BEHIND.get(cpu)
    {
        slot.store(published - mine, Ordering::Relaxed);
    }
}

/// An AP's last act before it reports online: its TSC, which the BSP's must
/// not read behind once it has seen the report.
pub fn ap_publish_tsc(cpu: usize) {
    if let Some(slot) = AP_TSC.get(cpu) {
        slot.store(rdtsc(), Ordering::Relaxed);
    }
}

/// Grade AP `cpu` once it has reported online, which orders its
/// [`ap_publish_tsc`] before this: say what its `IA32_TSC_ADJUST` held, and
/// hand the clock to the HPET if its TSC is out of step with the BSP's.
/// `true` while it is in step.
pub fn bsp_check_ap(cpu: usize) -> bool {
    let now = rdtsc();
    let found = FOUND_ADJUST
        .get(cpu)
        .map_or(0, |s| s.load(Ordering::Relaxed));
    if found != 0 {
        klog_info!("CLOCK: cpu {} IA32_TSC_ADJUST was {}; zeroed", cpu, found);
    }
    let theirs = AP_TSC.get(cpu).map_or(0, |s| s.load(Ordering::Relaxed));
    let ahead = theirs.saturating_sub(now);
    let behind = FOUND_BEHIND
        .get(cpu)
        .map_or(0, |s| s.load(Ordering::Relaxed));
    if ahead <= SYNC_SLACK_CYCLES && behind == 0 {
        return true;
    }
    let (cycles, side) = if behind != 0 {
        (behind, "behind")
    } else {
        (ahead, "ahead of")
    };
    klog_info!(
        "CLOCK: cpu {} TSC reads {} cycles {} the BSP's; the HPET is the monotonic clock",
        cpu,
        cycles,
        side
    );
    fall_back_to_hpet();
    false
}

fn fall_back_to_hpet() {
    if UNSTABLE_CLAIMED.swap(true, Ordering::AcqRel) {
        return;
    }
    if READY.is_set() {
        let hpet_ns = hpet::nanoseconds(hpet::read_counter());
        HPET_BIAS_NS.store(tsc_ns().saturating_sub(hpet_ns), Ordering::Relaxed);
    }
    UNSTABLE.store(true, Ordering::Release);
}

/// Measure the TSC against the HPET and switch the clock over to it, when the
/// TSC can be trusted. Once, after the HPET is running.
pub fn calibrate() {
    if READY.is_set() || !hpet::is_available() {
        return;
    }
    let found = FOUND_ADJUST[0].load(Ordering::Relaxed);
    if found != 0 {
        klog_info!("CLOCK: cpu 0 IA32_TSC_ADJUST was {}; zeroed", found);
    }
    if !tsc_is_stable() {
        klog_info!("CLOCK: TSC not invariant; the HPET stays the monotonic clock");
        return;
    }
    let (tsc0, ns0) = paired_sample();
    hpet::delay_ms(CALIBRATION_MS);
    let (tsc1, ns1) = paired_sample();
    let cycles = tsc1.saturating_sub(tsc0);
    let nanos = ns1.saturating_sub(ns0);
    if cycles == 0 || nanos == 0 {
        return;
    }
    let mult = ((nanos as u128) << 32) / cycles as u128;
    let Ok(mult) = u64::try_from(mult) else {
        return;
    };
    MULT.store(mult, Ordering::Relaxed);
    BASE_TSC.store(tsc1, Ordering::Relaxed);
    BASE_NS.store(ns1, Ordering::Relaxed);
    READY.mark_set();
    let khz = cycles.saturating_mul(1_000_000) / nanos;
    slopos_kernel_services::clock::record_measured_tsc_khz(khz);
    if UNSTABLE.load(Ordering::Acquire) {
        klog_info!(
            "CLOCK: TSC at {} kHz; the HPET stays the monotonic clock",
            khz
        );
    } else {
        klog_info!("CLOCK: monotonic clock on the TSC at {} kHz", khz);
    }
}

fn tsc_ns() -> u64 {
    let delta = rdtsc().saturating_sub(BASE_TSC.load(Ordering::Relaxed));
    let scaled = (delta as u128 * MULT.load(Ordering::Relaxed) as u128) >> 32;
    BASE_NS
        .load(Ordering::Relaxed)
        .saturating_add(scaled as u64)
}

/// Nanoseconds on the monotonic clock: the TSC once calibrated while every
/// CPU's is in step, the HPET otherwise.
#[inline]
pub fn monotonic_ns() -> u64 {
    if !READY.is_set() {
        return hpet::nanoseconds(hpet::read_counter());
    }
    if UNSTABLE.load(Ordering::Acquire) {
        return hpet::nanoseconds(hpet::read_counter())
            .saturating_add(HPET_BIAS_NS.load(Ordering::Relaxed));
    }
    tsc_ns()
}
