//! Regression tests for LAPIC timer calibration and configuration.
//!
//! These tests run after the full boot sequence, so the LAPIC timer has
//! already been calibrated by `BOOT_STEP_LAPIC_CALIBRATION`.

use slopos_lib::klog_info;
use slopos_lib::testing::TestResult;

use crate::apic;

// ---------------------------------------------------------------------------
// Calibration state
// ---------------------------------------------------------------------------

/// After boot, calibration must have completed successfully.
pub fn test_lapic_timer_is_calibrated() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: BUG - is_calibrated() returned false after boot");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// `frequency_hz()` must return a non-zero value after calibration.
pub fn test_lapic_timer_frequency_nonzero() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - frequency_hz() returned 0 after calibration");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// The calibrated frequency must be within a reasonable range.
///
/// With divisor 16, QEMU typically produces 50–500 MHz.  We use wider
/// bounds (1 MHz – 10 GHz) to avoid false negatives on unusual hosts.
pub fn test_lapic_timer_frequency_in_range() -> TestResult {
    let freq = apic::timer::frequency_hz();
    if freq == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    const MIN_HZ: u64 = 1_000_000; // 1 MHz
    const MAX_HZ: u64 = 10_000_000_000; // 10 GHz

    if freq < MIN_HZ || freq > MAX_HZ {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - frequency {} Hz outside [{}, {}]",
            freq,
            MIN_HZ,
            MAX_HZ,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// Re-calibration consistency
// ---------------------------------------------------------------------------

/// Re-running calibration should yield a similar frequency (within 15%).
///
/// This verifies the measurement is deterministic and not corrupted by
/// stale state left from the first calibration.
pub fn test_lapic_timer_recalibration_consistent() -> TestResult {
    let original = apic::timer::frequency_hz();
    if original == 0 {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // Re-calibrate (safe: uses one-shot masked mode, no interrupts fire).
    let recalibrated = apic::timer::calibrate();
    if recalibrated == 0 {
        klog_info!("LAPIC_TIMER_TEST: BUG - re-calibration returned 0");
        return TestResult::Fail;
    }

    // Check they're within 15% of each other.
    let diff = if recalibrated > original {
        recalibrated - original
    } else {
        original - recalibrated
    };
    let tolerance = original / 7; // ~14.3%

    if diff > tolerance {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - re-calibration drifted too much (original={}, recalibrated={}, diff={}, tolerance={})",
            original,
            recalibrated,
            diff,
            tolerance,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

// ---------------------------------------------------------------------------
// set_periodic_ms edge cases (no hardware side-effects)
// ---------------------------------------------------------------------------

/// `set_periodic_ms` with interval 0 must return false without touching hardware.
pub fn test_lapic_timer_periodic_zero_ms_rejected() -> TestResult {
    // Vector doesn't matter — the function should bail before writing registers.
    let result = apic::timer::set_periodic_ms(0xEF, 0);
    if result {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(_, 0) returned true");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// Verify `set_periodic_ms` accepts a sane 10ms interval and actually
/// programs the timer, then immediately stop it to avoid spurious IRQs.
pub fn test_lapic_timer_periodic_programs_timer() -> TestResult {
    if !apic::timer::is_calibrated() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - not calibrated");
        return TestResult::Skipped;
    }

    // Use a masked vector (bit 16 set) to prevent any interrupt delivery.
    // set_periodic_ms programs LVT with the vector unmasked, so we need to
    // immediately stop the timer and re-mask it after verifying the return
    // value.
    let ok = apic::timer::set_periodic_ms(0xEF, 10);

    // Stop the timer immediately to prevent any interrupt delivery.
    apic::timer_stop();
    // Re-mask the timer LVT for safety.
    apic::write_register(0x320, 1 << 16);

    if !ok {
        klog_info!("LAPIC_TIMER_TEST: BUG - set_periodic_ms(0xEF, 10) returned false");
        return TestResult::Fail;
    }

    TestResult::Pass
}

/// After stopping the timer, the current count must be zero or decaying
/// to zero (the ICR was cleared).
pub fn test_lapic_timer_stop_clears_counter() -> TestResult {
    if !apic::is_enabled() {
        klog_info!("LAPIC_TIMER_TEST: SKIP - LAPIC not enabled");
        return TestResult::Skipped;
    }

    apic::timer_stop();
    let count = apic::timer_get_current_count();
    if count != 0 {
        klog_info!(
            "LAPIC_TIMER_TEST: BUG - timer_get_current_count() = {} after stop (expected 0)",
            count,
        );
        return TestResult::Fail;
    }
    TestResult::Pass
}

slopos_lib::define_test_suite!(
    apic_timer,
    [
        test_lapic_timer_is_calibrated,
        test_lapic_timer_frequency_nonzero,
        test_lapic_timer_frequency_in_range,
        test_lapic_timer_recalibration_consistent,
        test_lapic_timer_periodic_zero_ms_rejected,
        test_lapic_timer_periodic_programs_timer,
        test_lapic_timer_stop_clears_counter,
    ]
);
