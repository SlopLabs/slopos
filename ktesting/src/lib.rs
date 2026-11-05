#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api)]

pub mod capture;
pub mod config;
pub mod filter;
pub mod harness;
pub mod kernel_phase_summary;
pub mod ktap;
pub mod registry;
mod result;
mod runner;

pub mod assertions;
#[cfg(feature = "qemu-exit")]
pub mod qemu_signal;

#[cfg(feature = "tests")]
pub mod bootstrap_tests;
#[cfg(feature = "tests")]
pub mod exception_tests;
#[cfg(feature = "tests")]
pub mod fpu_tests;
#[cfg(feature = "tests")]
pub mod lockdep_tests;
#[cfg(feature = "tests")]
pub mod xsave_tests;

pub use config::{config_from_cmdline, TestConfig, Verbosity};
pub use harness::{
    cycles_to_ms, estimate_cycles_per_ms, measure_elapsed_ms, tests_mark_panic,
    tests_request_shutdown, tests_reset_panic_state, tests_run_all, tests_run_userland,
    TestRunSummary,
};
pub use registry::{
    TestDesc, TestKind, TestOrder, FLAG_EXPECTED_PANIC, FLAG_EXPLICIT, FLAG_FIXTURE,
    FLAG_UNCAPTURED,
};
pub use result::TestResult;
pub use runner::execute_test;
pub use slopos_service_core::paste;

#[macro_export]
macro_rules! pass {
    () => {
        $crate::TestResult::Pass
    };
}

#[macro_export]
macro_rules! fail {
    () => {
        $crate::TestResult::Fail
    };
    ($msg:expr) => {{
        slopos_ostd::klog_info!("TEST FAIL: {}", $msg);
        $crate::TestResult::Fail
    }};
    ($fmt:expr, $($arg:tt)*) => {{
        slopos_ostd::klog_info!(concat!("TEST FAIL: ", $fmt), $($arg)*);
        $crate::TestResult::Fail
    }};
}

/// Register a `fn() -> TestResult` as a `TestDesc` in `.test_registry`.
///
/// Options, in any order after `name`:
/// - `flags = <FLAG_* bits>`;
/// - `kind = Userland` runs a kernel test in the userland phase, sorted among
///   the userland tests;
/// - `suite = <ident>` disambiguates the generated statics when two modules
///   register functions of one name;
/// - `order = first` runs the test ahead of every other test, and a failure
///   bails the phase; `order = last` runs it at the end of the phase;
/// - `rank = <u8>` runs it after every lower-ranked test of its module and
///   order, for a test that reads what others in its module left behind.
///
/// ```ignore
/// fn my_test() -> TestResult { TestResult::Pass }
/// slopos_testing::stest!(name = my_test);
/// slopos_testing::stest!(name = audit_after_my_test, rank = 1);
/// ```
#[macro_export]
macro_rules! stest {
    (name = $ident:ident $(, $($opt:tt)*)?) => {
        $crate::stest!(@opts $ident, (), (0), (Kernel), (Sorted), (0); $($($opt)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);) => {
        $crate::paste::paste! {
            fn [<__stest_thunk_ $($suite _)? $ident>]() -> $crate::TestResult {
                $crate::execute_test($ident as fn() -> $crate::TestResult)
            }

            slopos_ostd::registry_entry! {
            tests,
            #[allow(non_upper_case_globals)]
            pub static [<TEST_DESC_ $($suite _)? $ident>]: $crate::TestDesc = $crate::TestDesc {
                name: stringify!($ident),
                module: module_path!(),
                file: file!(),
                line: line!(),
                run: [<__stest_thunk_ $($suite _)? $ident>],
                kind: $crate::TestKind::$kind,
                order: $crate::TestOrder::$order,
                rank: $rank,
                flags: $flags,
                bin: None,
                argv: &[],
            };
            }
        }
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        flags = $set:expr $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($($suite)?), ($set), ($kind), ($order), ($rank); $($($rest)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        kind = $set:ident $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($($suite)?), ($flags), ($set), ($order), ($rank); $($($rest)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        suite = $set:ident $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($set), ($flags), ($kind), ($order), ($rank); $($($rest)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        order = first $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($($suite)?), ($flags), ($kind), (First), ($rank); $($($rest)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        order = last $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($($suite)?), ($flags), ($kind), (Last), ($rank); $($($rest)*)?);
    };

    (@opts $ident:ident, ($($suite:ident)?), ($flags:expr), ($kind:ident), ($order:ident), ($rank:expr);
        rank = $set:literal $(, $($rest:tt)*)?) => {
        $crate::stest!(@opts $ident, ($($suite)?), ($flags), ($kind), ($order), ($set); $($($rest)*)?);
    };
}

/// Register shared setup for a phase: `setup`, a `fn() -> bool`, runs before
/// the phase's first test whenever the phase runs any, whatever the filter,
/// so no test has to be named, filtered in or sorted first to provide it. A
/// fixture is not a test: it reports `KTAP\t# fixture <path> ok`, and a
/// `false` or a panic bails the phase.
///
/// ```ignore
/// fn mount_root() -> bool { /* … */ true }
/// slopos_testing::fixture!(name = root_mounted, phase = kernel, setup = mount_root);
/// ```
#[macro_export]
macro_rules! fixture {
    (name = $ident:ident, phase = kernel, setup = $setup:path $(,)?) => {
        $crate::fixture!(@desc $ident, Kernel, $setup);
    };

    (name = $ident:ident, phase = userland, setup = $setup:path $(,)?) => {
        $crate::fixture!(@desc $ident, Userland, $setup);
    };

    (@desc $ident:ident, $kind:ident, $setup:path) => {
        $crate::paste::paste! {
            fn [<__fixture_body_ $ident>]() -> $crate::TestResult {
                if $setup() {
                    $crate::TestResult::Pass
                } else {
                    $crate::TestResult::Fail
                }
            }

            fn [<__fixture_thunk_ $ident>]() -> $crate::TestResult {
                $crate::execute_test([<__fixture_body_ $ident>])
            }

            slopos_ostd::registry_entry! {
            tests,
            #[allow(non_upper_case_globals)]
            pub static [<FIXTURE_DESC_ $ident>]: $crate::TestDesc = $crate::TestDesc {
                name: stringify!($ident),
                module: module_path!(),
                file: file!(),
                line: line!(),
                run: [<__fixture_thunk_ $ident>],
                kind: $crate::TestKind::$kind,
                order: $crate::TestOrder::First,
                rank: 0,
                flags: $crate::FLAG_FIXTURE,
                bin: None,
                argv: &[],
            };
            }
        }
    };
}
