//! The standing proof that a C++ exception crosses a `dlopen` boundary.
//!
//! The work is in `/bin/cxx_probe`, which reaches the C library through
//! `libc.so` and the C++ library through `libc++.so` — the shape every
//! cross-built C++ program has — and in `/bin/cxx_static_probe`, which links
//! both as archives instead. This binary is static so it can report through
//! the harness, and it grades their exit status.
//!
//! Nobody else has this test. `unwinding`'s C++ claim is a README usage note
//! with no C++ exception test in the crate; Redox's only C++ test contains no
//! `throw`.

use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use slopos_slibc::test_harness::note;

const SIGABRT: i32 = slopos_abi::signal::SIGABRT as i32;

const PROBE: &str = "/bin/cxx_probe";
const STATIC_PROBE: &str = "/bin/cxx_static_probe";
const LIBCXX: &str = "/lib/libc++.so";
const FIXTURE: &str = "/lib/libcxxtest.so";

fn the_cxx_runtime_is_installed() -> bool {
    for path in [LIBCXX, FIXTURE, PROBE, STATIC_PROBE] {
        if let Err(e) = std::fs::metadata(path) {
            note(&format!("{path}: {e}"));
            return false;
        }
    }
    true
}

/// Twelve ordered checks, ending in the one this phase exists for: a
/// `CxxTestError` thrown inside `libcxxtest.so` and caught by type in the
/// executable that loaded it. A non-zero status is the number of the check
/// that failed.
fn a_cxx_program_throws_across_dlopen() -> bool {
    // Captured rather than inherited: a utest's stdio is init's console, not
    // the serial line this run is read from, so the probe's own account of
    // which check failed reaches the report only by being carried there.
    match Command::new(PROBE).output() {
        Ok(out) => {
            if out.status.code() == Some(0) {
                return true;
            }
            let said = String::from_utf8_lossy(&out.stderr);
            match out.status.code() {
                Some(code) => note(&format!("check {code}: {}", said.trim())),
                None => note(&format!(
                    "died by {:?}: {}",
                    out.status.signal(),
                    said.trim()
                )),
            }
            false
        }
        Err(e) => {
            note(&format!("spawning {PROBE} failed: {e}"));
            false
        }
    }
}

/// An exception with no handler reaches `std::terminate`, which aborts.
/// Which signal matters: a probe that failed to start also dies without an
/// exit code, and would otherwise pass this case.
fn an_uncaught_exception_terminates() -> bool {
    match Command::new(PROBE).arg("terminate").status() {
        Ok(status) => {
            if let Some(code) = status.code() {
                note(&format!(
                    "an uncaught throw exited {code} instead of aborting"
                ));
                return false;
            }
            if status.signal() != Some(SIGABRT) {
                note(&format!(
                    "an uncaught throw died by {:?}, wanted SIGABRT",
                    status.signal()
                ));
                return false;
            }
            true
        }
        Err(e) => {
            note(&format!("spawning {PROBE} terminate failed: {e}"));
            false
        }
    }
}

/// The same runtime as archives. Two things only this case reaches: `libc++.a`
/// and `libc.a`, which nothing else links, and the frame finder's `AT_PHDR`
/// road, a static program being in no object table at all. The destructor's
/// mark is on stdout because it is written after `main` has returned, where an
/// exit status can no longer say anything.
fn a_static_cxx_program_unwinds() -> bool {
    match Command::new(STATIC_PROBE).output() {
        Ok(out) => {
            let said = String::from_utf8_lossy(&out.stderr);
            match out.status.code() {
                Some(0) => {}
                Some(code) => {
                    note(&format!("check {code}: {}", said.trim()));
                    return false;
                }
                None => {
                    note(&format!(
                        "died by {:?}: {}",
                        out.status.signal(),
                        said.trim()
                    ));
                    return false;
                }
            }
            if !String::from_utf8_lossy(&out.stdout).contains("static-dtor-ran") {
                note("the static object's destructor did not run at exit");
                return false;
            }
            true
        }
        Err(e) => {
            note(&format!("spawning {STATIC_PROBE} failed: {e}"));
            false
        }
    }
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("the_cxx_runtime_is_installed", the_cxx_runtime_is_installed),
    (
        "a_cxx_program_throws_across_dlopen",
        a_cxx_program_throws_across_dlopen,
    ),
    (
        "an_uncaught_exception_terminates",
        an_uncaught_exception_terminates,
    ),
    ("a_static_cxx_program_unwinds", a_static_cxx_program_unwinds),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
