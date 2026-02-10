#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_drivers::interrupt_test::interrupt_test_request_shutdown;
pub use slopos_lib::testing::{
    HARNESS_MAX_SUITES, TestConfig, TestRunSummary, TestSuiteDesc, TestSuiteResult, Verbosity,
    measure_elapsed_ms,
};
use slopos_lib::{StateFlag, klog_info};

pub mod exception_tests;
pub mod fpu_tests;

pub const TESTS_MAX_SUITES: usize = HARNESS_MAX_SUITES;

static PANIC_SEEN: StateFlag = StateFlag::new();
static PANIC_REPORTED: AtomicBool = AtomicBool::new(false);

pub fn tests_reset_panic_state() {
    PANIC_SEEN.set_inactive();
    PANIC_REPORTED.store(false, Ordering::Relaxed);
}

pub fn tests_run_all(
    config: *const TestConfig,
    summary: *mut TestRunSummary,
    registry_start: *const TestSuiteDesc,
    registry_end: *const TestSuiteDesc,
) -> i32 {
    if config.is_null() {
        return -1;
    }

    let mut local_summary = TestRunSummary::default();
    let summary = if summary.is_null() {
        &mut local_summary
    } else {
        unsafe {
            *summary = TestRunSummary::default();
            &mut *summary
        }
    };

    let cfg = unsafe { &*config };
    if !cfg.enabled {
        klog_info!("TESTS: Harness disabled");
        return 0;
    }

    klog_info!("TESTS: Starting test suites");

    let start_cycles = slopos_lib::tsc::rdtsc();
    let mut idx = 0usize;
    let mut cursor = registry_start;
    while cursor < registry_end {
        if PANIC_SEEN.is_active() {
            summary.unexpected_exceptions = summary.unexpected_exceptions.saturating_add(1);
            summary.failed = summary.failed.saturating_add(1);
            if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                klog_info!("TESTS: panic flagged, stopping suite execution");
            }
            break;
        }

        let desc = unsafe { &*cursor };

        let suite_start = slopos_lib::tsc::rdtsc();
        let mut res = TestSuiteResult::default();
        res.name = desc.name;

        if let Some(run) = desc.run {
            let config_ptr = config as *const ();
            let suite_result = slopos_lib::catch_panic!({
                run(config_ptr, &mut res);
                0
            });
            if suite_result != 0 {
                res.unexpected_exceptions = res.unexpected_exceptions.saturating_add(1);
                res.failed = res.failed.saturating_add(1);
                klog_info!("TESTS: suite panic caught, continuing");
            }
        }

        if PANIC_SEEN.is_active() {
            res.unexpected_exceptions = res.unexpected_exceptions.saturating_add(1);
            res.failed = res.failed.saturating_add(1);
        }

        if cfg.timeout_ms != 0 {
            let elapsed = measure_elapsed_ms(suite_start, slopos_lib::tsc::rdtsc());
            if elapsed > cfg.timeout_ms {
                res.timed_out = 1;
                res.failed = res.failed.saturating_add(1);
                if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
                    klog_info!("TESTS: suite timeout exceeded");
                }
            }
        }

        if summary.suite_count < TESTS_MAX_SUITES {
            summary.suites[summary.suite_count] = res;
            summary.suite_count += 1;
        }

        klog_info!(
            "SUITE{} total={} pass={} fail={} elapsed={}ms",
            idx as u32,
            res.total,
            res.passed,
            res.failed,
            res.elapsed_ms,
        );
        summary.add_suite_result(&res);

        idx += 1;
        cursor = unsafe { cursor.add(1) };
    }
    let end_cycles = slopos_lib::tsc::rdtsc();
    let overall_ms = measure_elapsed_ms(start_cycles, end_cycles);
    if overall_ms > summary.elapsed_ms {
        summary.elapsed_ms = overall_ms;
    }

    klog_info!(
        "TESTS SUMMARY: total={} passed={} failed={} elapsed_ms={}",
        summary.total_tests,
        summary.passed,
        summary.failed,
        summary.elapsed_ms,
    );

    if summary.failed == 0 { 0 } else { -1 }
}

pub fn tests_request_shutdown(failed: i32) {
    interrupt_test_request_shutdown(failed);
}

pub fn tests_mark_panic() {
    PANIC_SEEN.set_active();
    if !PANIC_REPORTED.swap(true, Ordering::Relaxed) {
        klog_info!("TESTS: panic observed");
    }
}
