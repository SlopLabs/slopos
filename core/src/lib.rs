#![no_std]
#![forbid(unsafe_code)]
#![feature(try_trait_v2)]
#![feature(try_trait_v2_residual)]

pub mod driver_hooks;
pub mod efivar;
pub mod exec;
pub mod irq;
pub mod kconsole;
pub mod oom;
pub mod seat_file_ops;
#[cfg(feature = "test-hooks")]
pub mod tests;
#[macro_use]
pub mod syscall;

#[cfg(feature = "test-hooks")]
pub mod utests;

/// Register a userland test binary as a `TestDesc` in `.test_registry`.
///
/// Lives here rather than in `slopos-testing` because the runner needs
/// core-internal APIs; a testing → core dep would cycle.
///
/// ```ignore
/// slopos_core::utest!(name = utest_heap_allocator, bin = "/bin/heap_allocator_test");
/// slopos_core::utest!(
///     name = utest_with_args,
///     bin = "/bin/foo",
///     argv = &["foo", "--flag"],
/// );
/// slopos_core::utest!(name = utest_hours_long, bin = "/bin/bar", uncaptured);
/// slopos_core::utest!(name = utest_host_driven, bin = "/bin/baz", explicit);
/// slopos_core::utest!(name = utest_as_the_person, bin = "/bin/qux", caps = none, uid = 1000);
/// ```
///
/// `explicit` runs only when `tests.run` names the test in full, its log
/// reaching the console as it is written: a host drives it. A test runs as
/// uid 0 holding every capability unless it declares `caps`, the gated
/// capabilities it holds beside the universal ones, and `uid`, which is its
/// gid too.
#[macro_export]
macro_rules! utest {
    (name = $ident:ident, bin = $bin:literal $(, $($opt:tt)*)?) => {
        $crate::utest!(
            @opts $ident, $bin, (&[$bin]), (0), ($crate::__ostd::authority::CAP_MASK_ALL), (0);
            $($($opt)*)?
        );
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);) => {
        $crate::utest!(@desc $ident, $bin, $argv, $flags, $caps, $uid);
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        argv = &[$($arg:literal),* $(,)?] $(, $($rest:tt)*)?) => {
        $crate::utest!(@opts $ident, $bin, (&[$($arg),*]), ($flags), ($caps), ($uid); $($($rest)*)?);
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        uncaptured $(, $($rest:tt)*)?) => {
        $crate::utest!(
            @opts $ident, $bin, ($argv), ($flags | $crate::__testing::FLAG_UNCAPTURED), ($caps), ($uid);
            $($($rest)*)?
        );
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        explicit $(, $($rest:tt)*)?) => {
        $crate::utest!(
            @opts $ident, $bin, ($argv),
            ($flags | $crate::__testing::FLAG_EXPLICIT | $crate::__testing::FLAG_UNCAPTURED),
            ($caps), ($uid);
            $($($rest)*)?
        );
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        caps = none $(, $($rest:tt)*)?) => {
        $crate::utest!(
            @opts $ident, $bin, ($argv), ($flags), ($crate::__ostd::authority::CAP_NONE), ($uid);
            $($($rest)*)?
        );
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        caps = $declared:expr $(, $($rest:tt)*)?) => {
        $crate::utest!(@opts $ident, $bin, ($argv), ($flags), ($declared), ($uid); $($($rest)*)?);
    };

    (@opts $ident:ident, $bin:literal, ($argv:expr), ($flags:expr), ($caps:expr), ($uid:expr);
        uid = $declared:literal $(, $($rest:tt)*)?) => {
        $crate::utest!(@opts $ident, $bin, ($argv), ($flags), ($caps), ($declared); $($($rest)*)?);
    };

    (@desc $ident:ident, $bin:literal, $argv:expr, $flags:expr, $caps:expr, $uid:expr) => {
        $crate::__paste::paste! {
            fn [<__utest_thunk_ $ident>]() -> $crate::__testing::TestResult {
                $crate::exec::utest::run_thunk(
                    &[<TEST_DESC_ $ident>],
                    $crate::exec::utest::Principal {
                        caps: $caps,
                        uid: $uid,
                    },
                )
            }

            $crate::__ostd::registry_entry! {
                tests,
                #[allow(non_upper_case_globals)]
                pub static [<TEST_DESC_ $ident>]: $crate::__testing::TestDesc =
                    $crate::__testing::TestDesc {
                        name: stringify!($ident),
                        module: module_path!(),
                        file: file!(),
                        line: line!(),
                        run: [<__utest_thunk_ $ident>],
                        kind: $crate::__testing::TestKind::Userland,
                        flags: $flags,
                        bin: ::core::option::Option::Some($bin),
                        argv: $argv,
                    };
            }
        }
    };
}

#[doc(hidden)]
pub use slopos_ostd as __ostd;

#[doc(hidden)]
pub use slopos_testing as __testing;
#[doc(hidden)]
pub use slopos_testing::paste as __paste;
