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

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_arch::cpu::cpuid::cpuid;
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

static READY: InitFlag = InitFlag::new();
/// Nanoseconds per TSC cycle, as a 32.32 fixed-point fraction.
static MULT: AtomicU64 = AtomicU64::new(0);
static BASE_TSC: AtomicU64 = AtomicU64::new(0);
static BASE_NS: AtomicU64 = AtomicU64::new(0);

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

/// Measure the TSC against the HPET and switch the clock over to it, when the
/// TSC can be trusted. Once, after the HPET is running.
pub fn calibrate() {
    if READY.is_set() || !hpet::is_available() {
        return;
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
    klog_info!(
        "CLOCK: monotonic clock on the TSC at {} kHz",
        cycles.saturating_mul(1_000_000) / nanos
    );
}

/// Nanoseconds on the monotonic clock: the TSC once calibrated, the HPET
/// before.
#[inline]
pub fn monotonic_ns() -> u64 {
    if !READY.is_set() {
        return hpet::nanoseconds(hpet::read_counter());
    }
    let delta = rdtsc().saturating_sub(BASE_TSC.load(Ordering::Relaxed));
    let scaled = (delta as u128 * MULT.load(Ordering::Relaxed) as u128) >> 32;
    BASE_NS
        .load(Ordering::Relaxed)
        .saturating_add(scaled as u64)
}
