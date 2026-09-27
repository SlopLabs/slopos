//! `#!` dispatch and the `/bin/sh` fallback, through the entry points a port
//! reaches: `std::process::Command`, the raw `spawn_path` syscall, and
//! slibc's `execvp` and `posix_spawnp`.

use slopos_userland as _;

use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::syscall::process;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const DIR: &str = "/tmp/script_exec";
const ENOEXEC: i32 = 8;

fn install(name: &str, body: &str, mode: u32) -> Option<String> {
    let _ = fs::create_dir_all(DIR);
    let path = format!("{DIR}/{name}");
    let _ = fs::remove_file(&path);
    fs::write(&path, body).ok()?;
    fs::set_permissions(&path, fs::Permissions::from_mode(mode)).ok()?;
    Some(path)
}

fn stdout_of(cmd: &mut Command) -> Option<String> {
    match cmd.output() {
        Ok(out) if out.status.success() => String::from_utf8(out.stdout).ok(),
        Ok(out) => {
            eprintln!(
                "script_exec_test: exited {:?}, stderr {:?}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(e) => {
            eprintln!("script_exec_test: spawn failed: {e}");
            None
        }
    }
}

/// The interpreter gets the script's path as passed and then `argv[1..]`;
/// the script's own `argv[0]` is dropped.
fn a_script_runs_under_its_interpreter() -> bool {
    let Some(path) = install("hello", "#!/bin/sh\necho \"$0|$1|$2\"\n", 0o755) else {
        return false;
    };
    let got = stdout_of(Command::new(&path).args(["a b", "c"]));
    let want = format!("{path}|a b|c\n");
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

/// The rest of the `#!` line is one argument, not split on blanks: split, the
/// format would be `[%s]` and the second half would print as an operand.
fn the_interpreter_argument_is_one_word() -> bool {
    let Some(path) = install("oneword", "#!/bin/printf  [%s] [%s]  \n", 0o755) else {
        return false;
    };
    let got = stdout_of(Command::new(&path).arg("x"));
    let want = format!("[{path}] [x]");
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

/// A script's interpreter may itself be a script, and the chain unwinds into
/// one argument vector.
fn a_nested_script_chain_unwinds() -> bool {
    let Some(inner) = install("inner", "#!/bin/sh\necho \"$0|$1|$2\"\n", 0o755) else {
        return false;
    };
    let Some(outer) = install("outer", &format!("#!{inner}\n"), 0o755) else {
        return false;
    };
    let got = stdout_of(Command::new(&outer).arg("z"));
    let want = format!("{inner}|{outer}|z\n");
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

/// Neither ELF nor `#!`: the kernel answers `ENOEXEC` and runs nothing.
fn a_plain_text_file_is_enoexec_to_the_kernel() -> bool {
    let Some(path) = install("plain_raw", "echo should not run\n", 0o755) else {
        return false;
    };
    let rc = process::spawn_path_with_actions(
        path.as_bytes(),
        &[],
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[],
        0,
    );
    if rc != -ENOEXEC {
        eprintln!("script_exec_test: spawn of a text file returned {rc}");
        return false;
    }
    true
}

fn marker_after(out: &str) -> Option<String> {
    fs::read_to_string(out).ok()
}

/// POSIX `execvp`: a file the system cannot execute runs as `/bin/sh file`.
fn execvp_runs_a_plain_file_under_sh() -> bool {
    let out = format!("{DIR}/execvp.out");
    let _ = fs::remove_file(&out);
    let Some(path) = install("plain_vp", &format!("echo \"$0|$1\" > {out}\n"), 0o755) else {
        return false;
    };
    let file = format!("{path}\0");
    let pid = process::fork();
    if pid == 0 {
        let argv: [*const u8; 3] = [b"plain_vp\0".as_ptr(), b"y\0".as_ptr(), core::ptr::null()];
        // SAFETY: both strings are NUL-terminated and the vector is NULL-ended.
        unsafe { slopos_slibc::process::execvp(file.as_ptr(), argv.as_ptr()) };
        slopos_userland::syscall::core::exit_with_code(127);
    }
    if pid < 0 || process::wait_exit_code(pid as u32) != 0 {
        eprintln!("script_exec_test: execvp child failed");
        return false;
    }
    let want = format!("{path}|y\n");
    let got = marker_after(&out);
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

/// `posix_spawnp` searches `PATH` as `execvp` does, fallback included.
fn posix_spawnp_runs_a_plain_file_under_sh() -> bool {
    let out = format!("{DIR}/spawnp.out");
    let _ = fs::remove_file(&out);
    let Some(path) = install("plain_sp", &format!("echo \"$0|$1\" > {out}\n"), 0o755) else {
        return false;
    };
    let saved = std::env::var_os("PATH");
    // SAFETY: single-threaded test binary.
    unsafe { std::env::set_var("PATH", format!("{DIR}:/bin")) };
    let argv: [*const u8; 3] = [b"plain_sp\0".as_ptr(), b"w\0".as_ptr(), core::ptr::null()];
    let mut pid = 0;
    // SAFETY: NUL-terminated file, NULL-ended argv, default actions and
    // attributes, the caller's environment.
    let rc = unsafe {
        slopos_slibc::process::spawn::posix_spawnp(
            &mut pid,
            b"plain_sp\0".as_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            argv.as_ptr(),
            core::ptr::null(),
        )
    };
    match saved {
        // SAFETY: as above.
        Some(v) => unsafe { std::env::set_var("PATH", v) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    if rc != 0 || pid <= 0 || process::wait_exit_code(pid as u32) != 0 {
        eprintln!("script_exec_test: posix_spawnp returned {rc}, pid {pid}");
        return false;
    }
    let want = format!("{path}|w\n");
    let got = marker_after(&out);
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "a_script_runs_under_its_interpreter",
            a_script_runs_under_its_interpreter,
        ),
        (
            "the_interpreter_argument_is_one_word",
            the_interpreter_argument_is_one_word,
        ),
        (
            "a_nested_script_chain_unwinds",
            a_nested_script_chain_unwinds,
        ),
        (
            "a_plain_text_file_is_enoexec_to_the_kernel",
            a_plain_text_file_is_enoexec_to_the_kernel,
        ),
        (
            "execvp_runs_a_plain_file_under_sh",
            execvp_runs_a_plain_file_under_sh,
        ),
        (
            "posix_spawnp_runs_a_plain_file_under_sh",
            posix_spawnp_runs_a_plain_file_under_sh,
        ),
    ]);
}
