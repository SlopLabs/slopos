//! The Phase 1 exit criterion: a hand-written build driver running in-guest.
//!
//! A build system is the thing that breaks first when a POSIX floor is
//! incomplete, so the driver is written the way a real one is — relative
//! paths, `mtime` comparison, child exit codes, a lock on its own metadata.
//! Each case below names the Phase 1 workstream it proves.

use slopos_userland as _;

use slopos_abi::fs::AT_FDCWD;
use slopos_abi::signal::SIGSEGV;
use slopos_abi::syscall::posix::{LOCK_EX, LOCK_NB, LOCK_UN};
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::syscall::process::WaitStatus;
use slopos_userland::syscall::{SyscallError, fs as fs_syscall, process};

use std::ffi::CString;
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::{Duration, SystemTime};

/// `current_exe` is unavailable here, and the driver re-invokes itself as the
/// stub compiler.
const SELF_PATH: &str = "/bin/buildctl_test";

/// The writable root on both the disk image and the initramfs. The driver
/// `cd`s here once, so every path below stays relative.
const PROJECT_ROOT: &str = "/var/buildctl";

/// Over 32 bytes, which the old `MAX_NAME_LEN` refused.
const LONG_SOURCE: &str = "src/module_with_a_deliberately_long_name_over_thirty_two_bytes.slop";
const MAIN_SOURCE: &str = "src/main.slop";
const BROKEN_SOURCE: &str = "src/broken.slop";
const FINGERPRINTS: &str = "build/fingerprints";
const HEADER: &str = "shared/common.h";
const HEADER_LINK: &str = "include/common.h";

const SOURCES: &[&str] = &[MAIN_SOURCE, LONG_SOURCE];

/// The stub compiler refuses a source carrying this token, so a failing
/// compile is the child's own decision rather than a spawn failure.
const POISON: &[u8] = b"#error";
const COMPILE_FAILURE_CODE: i32 = 3;

fn object_for(source: &str) -> String {
    let stem = source.rsplit('/').next().unwrap_or(source);
    format!("build/{stem}.o")
}

fn mtime_secs(path: &str) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

fn read_fingerprints() -> Vec<(String, u64)> {
    let Ok(text) = fs::read_to_string(FINGERPRINTS) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let (path, stamp) = line.rsplit_once(' ')?;
            Some((path.to_string(), stamp.parse::<u64>().ok()?))
        })
        .collect()
}

fn write_fingerprints(entries: &[(String, u64)]) -> bool {
    let mut text = String::new();
    for (path, stamp) in entries {
        text.push_str(path);
        text.push(' ');
        text.push_str(&stamp.to_string());
        text.push('\n');
    }
    fs::write(FINGERPRINTS, text).is_ok()
}

/// Runs with the project root as the working directory, so both operands stay
/// relative.
fn compile(source: &str, object: &str) -> Option<i32> {
    let status = Command::new(SELF_PATH)
        .arg("cc")
        .arg(source)
        .arg(object)
        .current_dir(PROJECT_ROOT)
        .status();
    match status {
        Ok(status) => match status.code() {
            Some(code) => Some(code),
            None => {
                eprintln!("buildctl_test: the compiler for {source} died by a signal");
                None
            }
        },
        Err(e) => {
            eprintln!("buildctl_test: could not spawn the compiler for {source}: {e}");
            None
        }
    }
}

fn build_pass() -> Option<Vec<String>> {
    let previous = read_fingerprints();
    let mut current: Vec<(String, u64)> = Vec::new();
    let mut rebuilt: Vec<String> = Vec::new();

    for &source in SOURCES {
        let Some(stamp) = mtime_secs(source) else {
            eprintln!("buildctl_test: {source} has no mtime");
            return None;
        };
        current.push((source.to_string(), stamp));

        let object = object_for(source);
        let unchanged = previous
            .iter()
            .any(|(path, seen)| path.as_str() == source && *seen == stamp)
            && fs::metadata(&object).is_ok();
        if unchanged {
            continue;
        }

        match compile(source, &object) {
            Some(0) => {}
            Some(code) => {
                eprintln!("buildctl_test: the compiler for {source} exited {code}");
                return None;
            }
            None => return None,
        }
        if fs::metadata(&object).is_err() {
            eprintln!("buildctl_test: {object} was not produced");
            return None;
        }
        rebuilt.push(source.to_string());
    }

    if !write_fingerprints(&current) {
        eprintln!("buildctl_test: could not record fingerprints");
        return None;
    }
    Some(rebuilt)
}

/// 1.1/1.5. A source name past 32 bytes, and an include directory that reaches
/// a shared header through a symlink.
fn project_tree_is_created_with_long_names_and_a_symlink() -> bool {
    let _ = fs::remove_dir_all(PROJECT_ROOT);
    if fs::create_dir_all(PROJECT_ROOT).is_err() {
        eprintln!("buildctl_test: could not create {PROJECT_ROOT}");
        return false;
    }
    if std::env::set_current_dir(PROJECT_ROOT).is_err() {
        eprintln!("buildctl_test: could not cd to {PROJECT_ROOT}");
        return false;
    }

    for dir in ["src", "include", "shared", "build"] {
        if fs::create_dir_all(dir).is_err() {
            eprintln!("buildctl_test: could not create {dir}");
            return false;
        }
    }

    let header = b"#define SLOPOS_BUILDCTL 1\n";
    if fs::write(HEADER, header).is_err()
        || fs::write(MAIN_SOURCE, b"#include \"include/common.h\"\nmain\n").is_err()
        || fs::write(LONG_SOURCE, b"#include \"include/common.h\"\nmodule\n").is_err()
    {
        eprintln!("buildctl_test: could not write the source tree");
        return false;
    }

    // `/var` persists, so a boot that was cut short must not fail the next one
    // on a link that already exists.
    let _ = fs::remove_file(HEADER_LINK);
    let target = CString::new("../shared/common.h").unwrap();
    let link = CString::new(HEADER_LINK).unwrap();
    if let Err(e) = fs_syscall::symlinkat(target.as_ptr(), AT_FDCWD, link.as_ptr()) {
        eprintln!("buildctl_test: symlink {HEADER_LINK} -> ../shared/common.h failed: {e}");
        return false;
    }

    let long_name = LONG_SOURCE.rsplit('/').next().unwrap_or(LONG_SOURCE);
    if long_name.len() <= 32 {
        eprintln!(
            "buildctl_test: the long source name is only {} bytes",
            long_name.len()
        );
        return false;
    }

    let Ok(src_entries) = fs::read_dir("src") else {
        eprintln!("buildctl_test: src is not readable");
        return false;
    };
    let saw_long_name = src_entries
        .flatten()
        .any(|e| e.file_name().to_str() == Some(long_name));
    if !saw_long_name {
        eprintln!("buildctl_test: read_dir(src) never listed {long_name}");
        return false;
    }

    let Ok(include_entries) = fs::read_dir("include") else {
        eprintln!("buildctl_test: include is not readable");
        return false;
    };
    let saw_link = include_entries
        .flatten()
        .any(|e| e.file_name().to_str() == Some("common.h"));
    if !saw_link {
        eprintln!("buildctl_test: read_dir(include) never listed the symlink");
        return false;
    }

    match fs::symlink_metadata(HEADER_LINK) {
        Ok(meta) if meta.file_type().is_symlink() => {}
        Ok(_) => {
            eprintln!("buildctl_test: {HEADER_LINK} is not a symlink");
            return false;
        }
        Err(e) => {
            eprintln!("buildctl_test: symlink_metadata({HEADER_LINK}) failed: {e}");
            return false;
        }
    }

    match fs::read(HEADER_LINK) {
        Ok(bytes) if bytes.as_slice() == header.as_slice() => true,
        Ok(_) => {
            eprintln!("buildctl_test: {HEADER_LINK} resolved to the wrong file");
            false
        }
        Err(e) => {
            eprintln!("buildctl_test: reading through {HEADER_LINK} failed: {e}");
            false
        }
    }
}

/// 1.2/1.3. Every source compiles, and every child's success is observed as a
/// zero exit code rather than assumed.
fn a_cold_build_compiles_every_source() -> bool {
    let Some(rebuilt) = build_pass() else {
        return false;
    };
    if rebuilt.len() != SOURCES.len() {
        eprintln!(
            "buildctl_test: cold build compiled {} of {} sources",
            rebuilt.len(),
            SOURCES.len()
        );
        return false;
    }
    true
}

/// 1.2. An `mtime` read back after a compile is the value the compile saw; a
/// clock that does not advance, or a filesystem that restamps, fails here.
fn a_second_run_skips_every_unchanged_input() -> bool {
    let Some(rebuilt) = build_pass() else {
        return false;
    };
    if !rebuilt.is_empty() {
        eprintln!("buildctl_test: the second run recompiled {rebuilt:?}");
        return false;
    }
    true
}

/// 1.2. A changed `mtime` is noticed, for exactly the input that changed.
fn touching_one_input_recompiles_only_that_input() -> bool {
    let Some(before) = mtime_secs(MAIN_SOURCE) else {
        eprintln!("buildctl_test: {MAIN_SOURCE} has no mtime");
        return false;
    };
    let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(before + 120);

    let file = match OpenOptions::new().write(true).open(MAIN_SOURCE) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("buildctl_test: could not open {MAIN_SOURCE} to restamp it: {e}");
            return false;
        }
    };
    if let Err(e) = file.set_times(FileTimes::new().set_modified(newer)) {
        eprintln!("buildctl_test: set_times on {MAIN_SOURCE} failed: {e}");
        return false;
    }
    drop(file);

    match mtime_secs(MAIN_SOURCE) {
        Some(after) if after == before + 120 => {}
        Some(after) => {
            let want = before + 120;
            eprintln!("buildctl_test: {MAIN_SOURCE} reads back mtime {after}, wanted {want}");
            return false;
        }
        None => {
            eprintln!("buildctl_test: {MAIN_SOURCE} lost its mtime");
            return false;
        }
    }

    let Some(rebuilt) = build_pass() else {
        return false;
    };
    if rebuilt != [MAIN_SOURCE.to_string()] {
        eprintln!("buildctl_test: restamping {MAIN_SOURCE} recompiled {rebuilt:?}");
        return false;
    }
    true
}

/// 1.3. A failing child's own exit code, not a flattened zero.
fn a_failing_compile_reports_its_nonzero_code() -> bool {
    if fs::write(BROKEN_SOURCE, b"#error deliberate\n").is_err() {
        eprintln!("buildctl_test: could not write {BROKEN_SOURCE}");
        return false;
    }
    match compile(BROKEN_SOURCE, &object_for(BROKEN_SOURCE)) {
        Some(COMPILE_FAILURE_CODE) => {}
        Some(code) => {
            eprintln!("buildctl_test: a poisoned source compiled with code {code}");
            return false;
        }
        None => return false,
    }
    if fs::metadata(object_for(BROKEN_SOURCE)).is_ok() {
        eprintln!("buildctl_test: a failed compile still produced an object");
        return false;
    }
    let _ = fs::remove_file(BROKEN_SOURCE);
    true
}

/// 1.3/1.4. A death by signal is reported as a signal: `std` invents no exit
/// code for it, and the raw wait status names the signal.
fn a_child_killed_by_sigsegv_is_reported_as_signalled() -> bool {
    let status = Command::new(SELF_PATH)
        .arg("segv")
        .current_dir(PROJECT_ROOT)
        .status();
    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                eprintln!("buildctl_test: a segfaulting child reported exit code {code}");
                return false;
            }
        }
        Err(e) => {
            eprintln!("buildctl_test: could not spawn the segfaulting child: {e}");
            return false;
        }
    }

    let arg0 = *b"buildctl_test\0";
    let arg1 = *b"segv\0";
    let argv = [arg0.as_ptr(), arg1.as_ptr()];
    let actions = [process::clone_fd(1, 1), process::clone_fd(2, 2)];
    let tid = process::spawn_path_with_actions(
        SELF_PATH.as_bytes(),
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    if tid <= 0 {
        eprintln!("buildctl_test: raw spawn of the segfaulting child returned {tid}");
        return false;
    }
    let Some((reaped, raw)) = process::waitpid(tid as u32) else {
        eprintln!("buildctl_test: the segfaulting child could not be reaped");
        return false;
    };
    if reaped != tid as u32 {
        eprintln!("buildctl_test: waitpid reaped {reaped}, expected {tid}");
        return false;
    }
    let report = process::wait_status(raw);
    if report != WaitStatus::Signalled(SIGSEGV) {
        eprintln!("buildctl_test: the segfaulting child reported {report:?}");
        return false;
    }
    true
}

/// 1.5. Two drivers must not share one output tree. The probe is a separate
/// process because an advisory lock is only meaningful across them.
fn the_fingerprint_lock_excludes_a_second_driver() -> bool {
    let held = match File::open(FINGERPRINTS) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("buildctl_test: {FINGERPRINTS} is not open-able: {e}");
            return false;
        }
    };
    if let Err(e) = fs_syscall::flock(held.as_raw_fd(), LOCK_EX | LOCK_NB) {
        eprintln!("buildctl_test: could not take the build lock: {e}");
        return false;
    }

    let probe = Command::new(SELF_PATH)
        .arg("lockprobe")
        .arg(FINGERPRINTS)
        .current_dir(PROJECT_ROOT)
        .status();
    let contended = match probe {
        Ok(status) => match status.code() {
            Some(0) => true,
            Some(1) => {
                eprintln!("buildctl_test: a second driver took a lock the first one held");
                false
            }
            Some(code) => {
                eprintln!("buildctl_test: the lock probe could not run (code {code})");
                false
            }
            None => {
                eprintln!("buildctl_test: the lock probe died by a signal");
                false
            }
        },
        Err(e) => {
            eprintln!("buildctl_test: could not spawn the lock probe: {e}");
            let _ = fs_syscall::flock(held.as_raw_fd(), LOCK_UN);
            return false;
        }
    };

    if let Err(e) = fs_syscall::flock(held.as_raw_fd(), LOCK_UN) {
        eprintln!("buildctl_test: could not release the build lock: {e}");
        return false;
    }
    if !contended {
        return false;
    }

    // Released means re-takeable, or the lock leaks and the next build hangs.
    match fs_syscall::flock(held.as_raw_fd(), LOCK_EX | LOCK_NB) {
        Ok(()) => {
            let _ = fs_syscall::flock(held.as_raw_fd(), LOCK_UN);
            true
        }
        Err(e) => {
            eprintln!("buildctl_test: the released lock could not be retaken: {e}");
            false
        }
    }
}

/// Both operands are relative, so this only works if the spawner's
/// `current_dir` really moved it.
fn compiler_mode(source: &str, object: &str) -> i32 {
    let Ok(text) = fs::read(source) else {
        eprintln!("buildctl_test cc: cannot read {source}");
        return 2;
    };
    if text.windows(POISON.len()).any(|w| w == POISON) {
        eprintln!("buildctl_test cc: {source} is poisoned");
        return COMPILE_FAILURE_CODE;
    }
    let mut out = Vec::new();
    out.extend_from_slice(b"SLOPOBJ\n");
    out.extend_from_slice(&text);
    match File::create(object).and_then(|mut f| f.write_all(&out)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("buildctl_test cc: cannot write {object}: {e}");
            2
        }
    }
}

/// Exits 0 only when the lock was correctly refused, so the parent's assertion
/// cannot pass on a probe that failed for some other reason.
fn lock_probe_mode(path: &str) -> i32 {
    let Ok(file) = File::open(path) else {
        eprintln!("buildctl_test lockprobe: cannot open {path}");
        return 2;
    };
    match fs_syscall::flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) {
        Err(e) if e == SyscallError::EAGAIN => 0,
        Err(e) => {
            eprintln!("buildctl_test lockprobe: flock failed with {e}, wanted EWOULDBLOCK");
            2
        }
        Ok(()) => {
            let _ = fs_syscall::flock(file.as_raw_fd(), LOCK_UN);
            eprintln!("buildctl_test lockprobe: took a lock the parent holds");
            1
        }
    }
}

fn segv_mode() -> ! {
    // Volatile: an ordinary null store is UB the optimiser may fold away.
    unsafe { core::ptr::write_volatile(core::ptr::null_mut::<u8>(), 1) };
    eprintln!("buildctl_test segv: a null store did not fault");
    std::process::exit(97)
}

const CASES: &[(&str, fn() -> bool)] = &[
    (
        "project_tree_is_created_with_long_names_and_a_symlink",
        project_tree_is_created_with_long_names_and_a_symlink,
    ),
    (
        "a_cold_build_compiles_every_source",
        a_cold_build_compiles_every_source,
    ),
    (
        "a_second_run_skips_every_unchanged_input",
        a_second_run_skips_every_unchanged_input,
    ),
    (
        "touching_one_input_recompiles_only_that_input",
        touching_one_input_recompiles_only_that_input,
    ),
    (
        "a_failing_compile_reports_its_nonzero_code",
        a_failing_compile_reports_its_nonzero_code,
    ),
    (
        "a_child_killed_by_sigsegv_is_reported_as_signalled",
        a_child_killed_by_sigsegv_is_reported_as_signalled,
    ),
    (
        "the_fingerprint_lock_excludes_a_second_driver",
        the_fingerprint_lock_excludes_a_second_driver,
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("cc") => {
            if args.len() != 3 {
                eprintln!("buildctl_test cc: want <source> <object>");
                std::process::exit(2);
            }
            std::process::exit(compiler_mode(&args[1], &args[2]))
        }
        Some("lockprobe") => {
            if args.len() != 2 {
                eprintln!("buildctl_test lockprobe: want <path>");
                std::process::exit(2);
            }
            std::process::exit(lock_probe_mode(&args[1]))
        }
        Some("segv") => segv_mode(),
        Some(other) => {
            eprintln!("buildctl_test: unknown mode {other}");
            std::process::exit(2);
        }
        None => slopos_slibc::test_harness::run(CASES),
    }
}
