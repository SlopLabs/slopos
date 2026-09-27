use slopos_userland as _;

use std::env;
use std::fs;

/// Walks every directory `/` reports and `cd`s into each through plain
/// `std::env`, proving third-party userland apps can rely on `std` for cwd ops.
fn std_cd_into_every_listed_dir() -> bool {
    if env::set_current_dir("/").is_err() {
        return false;
    }

    let rd = match fs::read_dir("/") {
        Ok(rd) => rd,
        Err(_) => return false,
    };

    let mut dirs = 0u32;
    for entry in rd.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();

        if !fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
            return false;
        }
        if env::set_current_dir(&path).is_err() {
            return false;
        }
        dirs += 1;
        if env::set_current_dir("/").is_err() {
            return false;
        }
    }

    dirs > 0
}

fn std_set_then_current_dir_roundtrip() -> bool {
    if !fs::metadata("/bin").map(|m| m.is_dir()).unwrap_or(false) {
        if env::set_current_dir("/").is_err() {
            return false;
        }
        return env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(|s| s == "/"))
            .unwrap_or(false);
    }

    if env::set_current_dir("/bin").is_err() {
        return false;
    }
    let ok = env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(|s| s == "/bin"))
        .unwrap_or(false);
    let _ = env::set_current_dir("/");
    ok
}

fn std_temp_dir_is_tmp() -> bool {
    env::temp_dir().to_str() == Some("/tmp")
}

/// `canonicalize` joined a relative path onto `/` rather than onto the working
/// directory, so it answered the canonical path of a different file — and
/// answered it successfully whenever that other file happened to exist.
///
/// `/bin/ls` is a symlink to the multicall binary and `realpath(3)` resolves
/// the final component too, so the canonical answer is `/bin/coreutils`. That
/// is what makes one case prove both halves: resolving against `/` rather
/// than the cwd could not produce it, and neither could a walk that stopped
/// short of the last symlink.
fn std_canonicalize_resolves_against_the_cwd() -> bool {
    if env::set_current_dir("/bin").is_err() {
        eprintln!("cd_test: cd /bin failed");
        return false;
    }
    let resolved = fs::canonicalize("ls");
    let _ = env::set_current_dir("/");
    match resolved {
        Ok(path) => path.to_str() == Some("/bin/coreutils"),
        Err(e) => {
            eprintln!("cd_test: canonicalize(\"ls\") from /bin failed: {e:?}");
            false
        }
    }
}

/// The working directory belongs to the process: a thread's `chdir` moves
/// every thread, as `CLONE_FS` gives a Linux thread.
fn a_threads_chdir_moves_the_whole_process() -> bool {
    if env::set_current_dir("/").is_err() {
        return false;
    }
    let moved = std::thread::spawn(|| env::set_current_dir("/tmp").is_ok()).join();
    let here = env::current_dir();
    let back = std::thread::spawn(env::current_dir).join();
    let _ = env::set_current_dir("/");
    let tmp = Some(std::path::Path::new("/tmp"));
    match (moved, here, back) {
        (Ok(true), Ok(here), Ok(Ok(back))) => {
            let ok = Some(here.as_path()) == tmp && Some(back.as_path()) == tmp;
            if !ok {
                eprintln!(
                    "cd_test: after a thread's chdir the process is at {here:?}, a new thread at {back:?}"
                );
            }
            ok
        }
        other => {
            eprintln!("cd_test: thread chdir failed: {other:?}");
            false
        }
    }
}

/// `fchdir` moves to the directory an open descriptor names.
fn fchdir_moves_to_an_open_directory() -> bool {
    use std::os::fd::AsRawFd;
    let _ = env::set_current_dir("/");
    let Ok(dir) = fs::File::open("/tmp") else {
        return false;
    };
    // SAFETY: `dir` is an open descriptor for the duration of the call.
    let rc = unsafe { slopos_slibc::ffi::syscalls::fchdir(dir.as_raw_fd()) };
    let here = env::current_dir();
    let _ = env::set_current_dir("/");
    match here {
        Ok(here) if rc == 0 && here.to_str() == Some("/tmp") => true,
        other => {
            eprintln!("cd_test: fchdir returned {rc}, cwd {other:?}");
            false
        }
    }
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("std_cd_into_every_listed_dir", std_cd_into_every_listed_dir),
        (
            "std_set_then_current_dir_roundtrip",
            std_set_then_current_dir_roundtrip,
        ),
        ("std_temp_dir_is_tmp", std_temp_dir_is_tmp),
        (
            "std_canonicalize_resolves_against_the_cwd",
            std_canonicalize_resolves_against_the_cwd,
        ),
        (
            "a_threads_chdir_moves_the_whole_process",
            a_threads_chdir_moves_the_whole_process,
        ),
        (
            "fchdir_moves_to_an_open_directory",
            fchdir_moves_to_an_open_directory,
        ),
    ]);
}
