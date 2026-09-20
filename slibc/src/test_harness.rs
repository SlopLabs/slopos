//! Userland-side bridge to the kernel test harness.
//!
//! Test binaries call [`run`] with `(name, fn() -> bool)` pairs; each result
//! goes to the kernel via `SYSCALL_TEST_REPORT` and becomes a KTAP subtest
//! line.

use core::cell::SyncUnsafeCell;

use crate::pal::{Pal, Sys};
use crate::process;

/// Wire-format status passed to `SYSCALL_TEST_REPORT`. Values must match
/// [`slopos_abi::syscall::TestReportStatus`].
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum TestStatus {
    Pass = 0,
    Fail = 1,
    Skip = 2,
}

/// Best-effort: a kernel-side failure to record is swallowed and the caller
/// continues.
pub fn report(status: TestStatus, name: &str, msg: &str) {
    let _ = Sys::test_report(status as u32, name.as_bytes(), msg.as_bytes());
}

fn progress_write(bytes: &[u8]) {
    let _ = Sys::write(2, bytes.as_ptr(), bytes.len());
}

fn progress(prefix: &str, name: &str, phase: &str) {
    progress_write(b"utest-progress: ");
    progress_write(prefix.as_bytes());
    progress_write(b"::");
    progress_write(name.as_bytes());
    progress_write(b" ");
    progress_write(phase.as_bytes());
    progress_write(b"\n");
}

/// The message the running case attached, if any.
///
/// A utest's own `stderr` is init's console rather than the serial line the
/// run is read from, so a case that fails silently is a name and nothing
/// else. This is the one channel that reaches the KTAP line, which is where a
/// case that drives a second program has to say what that program answered.
static NOTE: SyncUnsafeCell<[u8; NOTE_MAX]> = SyncUnsafeCell::new([0; NOTE_MAX]);
static NOTE_LEN: SyncUnsafeCell<usize> = SyncUnsafeCell::new(0);

const NOTE_MAX: usize = 256;

/// Attach `msg` to the report of the case that is running. A second call
/// replaces the first, and anything past [`NOTE_MAX`] is dropped.
pub fn note(msg: &str) {
    // Truncating on a character boundary: a note cut mid-sequence is not UTF-8,
    // and the reader drops the whole message rather than its tail.
    let mut len = msg.len().min(NOTE_MAX);
    while len > 0 && !msg.is_char_boundary(len) {
        len -= 1;
    }
    // SAFETY: cases run one at a time on one thread, so nothing else touches
    // the buffer while this does.
    unsafe {
        core::ptr::copy_nonoverlapping(msg.as_ptr(), NOTE.get().cast::<u8>(), len);
        *NOTE_LEN.get() = len;
    }
}

fn take_note() -> &'static str {
    // SAFETY: as `note`.
    unsafe {
        let len = core::mem::take(&mut *NOTE_LEN.get());
        let bytes = core::slice::from_raw_parts(NOTE.get().cast::<u8>(), len);
        core::str::from_utf8(bytes).unwrap_or("")
    }
}

/// Runs every case, reports each result, then exits with `failed.min(255)`.
/// The exit code is a coarse fallback for a binary that crashes before
/// reporting; the structured reports are what the kernel runner rolls up.
pub fn run(cases: &[(&'static str, fn() -> bool)]) -> ! {
    run_impl(None, cases)
}

/// Like [`run`], but also prints a best-effort start/end line to stderr so the
/// active case is identifiable if the binary hangs or panics before reporting.
pub fn run_with_progress(prefix: &'static str, cases: &[(&'static str, fn() -> bool)]) -> ! {
    run_impl(Some(prefix), cases)
}

fn run_impl(progress_prefix: Option<&'static str>, cases: &[(&'static str, fn() -> bool)]) -> ! {
    let mut failed: u32 = 0;
    for (name, f) in cases {
        if let Some(prefix) = progress_prefix {
            progress(prefix, name, "start");
        }
        let ok = f();
        let status = if ok {
            TestStatus::Pass
        } else {
            TestStatus::Fail
        };
        if let Some(prefix) = progress_prefix {
            progress(prefix, name, if ok { "pass" } else { "fail" });
        }
        report(status, name, take_note());
        if !ok {
            failed = failed.saturating_add(1);
        }
    }
    let exit_code = failed.min(255) as i32;
    process::shim::exit(exit_code)
}
