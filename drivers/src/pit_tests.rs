use core::ffi::c_int;

use slopos_lib::{klog_info, testing::measure_elapsed_ms, tsc::rdtsc};

use crate::pit::{pit_get_frequency, pit_poll_delay_ms};

const DELAY_TEST_ITERATIONS: usize = 50;
const DELAY_MS: u32 = 10;
const MIN_EXPECTED_MS: u32 = 3;

pub fn test_pit_poll_delay_no_early_exit() -> c_int {
    let mut early_exits = 0u32;
    let mut min_elapsed: u32 = u32::MAX;

    for _ in 0..DELAY_TEST_ITERATIONS {
        let start = rdtsc();
        pit_poll_delay_ms(DELAY_MS);
        let end = rdtsc();

        let elapsed = measure_elapsed_ms(start, end);
        if elapsed < min_elapsed {
            min_elapsed = elapsed;
        }

        if elapsed < MIN_EXPECTED_MS {
            early_exits += 1;
        }
    }

    if early_exits > 0 {
        klog_info!(
            "PIT_TEST: BUG - {} early exits in {} iterations (min={}ms, expected>={}ms)",
            early_exits,
            DELAY_TEST_ITERATIONS,
            min_elapsed,
            MIN_EXPECTED_MS
        );
        return -1;
    }

    0
}

pub fn test_pit_poll_delay_timing_consistency() -> c_int {
    let mut total_elapsed: u64 = 0;
    let mut max_deviation: i64 = 0;

    for _ in 0..DELAY_TEST_ITERATIONS {
        let start = rdtsc();
        pit_poll_delay_ms(DELAY_MS);
        let end = rdtsc();

        let elapsed = measure_elapsed_ms(start, end);
        total_elapsed += elapsed as u64;

        let deviation = (elapsed as i64) - (DELAY_MS as i64);
        if deviation.abs() > max_deviation.abs() {
            max_deviation = deviation;
        }
    }

    let avg_elapsed = total_elapsed / (DELAY_TEST_ITERATIONS as u64);
    let tolerance_ms: i64 = 5;

    if max_deviation < -tolerance_ms {
        klog_info!(
            "PIT_TEST: BUG - timing too short (avg={}ms, max_deviation={}ms for {}ms delay)",
            avg_elapsed,
            max_deviation,
            DELAY_MS
        );
        return -1;
    }

    0
}

pub fn test_pit_poll_delay_zero_ms() -> c_int {
    let start = rdtsc();
    pit_poll_delay_ms(0);
    let end = rdtsc();

    let elapsed = measure_elapsed_ms(start, end);
    if elapsed > 5 {
        klog_info!(
            "PIT_TEST: BUG - zero delay took {}ms (should be instant)",
            elapsed
        );
        return -1;
    }

    0
}

pub fn test_pit_frequency_valid() -> c_int {
    let freq = pit_get_frequency();

    if freq == 0 {
        klog_info!("PIT_TEST: BUG - PIT frequency is zero (not initialized?)");
        return -1;
    }

    if freq > 10000 {
        klog_info!(
            "PIT_TEST: WARNING - PIT frequency {} Hz seems unusually high",
            freq
        );
    }

    0
}

pub fn test_pit_poll_delay_stress() -> c_int {
    let iterations = 100;
    let delay_ms = 10u32;
    let min_expected = 2u32;
    let mut failures = 0u32;

    for i in 0..iterations {
        let start = rdtsc();
        pit_poll_delay_ms(delay_ms);
        let end = rdtsc();

        let elapsed = measure_elapsed_ms(start, end);
        if elapsed < min_expected {
            failures += 1;
            if failures <= 3 {
                klog_info!(
                    "PIT_TEST: Early exit at iteration {} ({}ms < {}ms)",
                    i,
                    elapsed,
                    min_expected
                );
            }
        }
    }

    if failures > 0 {
        klog_info!(
            "PIT_TEST: BUG - {} failures in {} iterations (race condition?)",
            failures,
            iterations
        );
        return -1;
    }

    0
}

slopos_lib::define_test_suite!(
    pit,
    [
        test_pit_frequency_valid,
        test_pit_poll_delay_zero_ms,
        test_pit_poll_delay_no_early_exit,
        test_pit_poll_delay_timing_consistency,
        test_pit_poll_delay_stress,
    ]
);
