//! Per-test execution thunk.

use slopos_ostd::catch_panic;

use crate::result::TestResult;

/// Run `f` under `catch_panic!`: `TestResult::Panic` if it panicked, otherwise
/// what it returned.
pub fn execute_test(f: fn() -> TestResult) -> TestResult {
    catch_panic!({ f() }).unwrap_or(TestResult::Panic)
}
