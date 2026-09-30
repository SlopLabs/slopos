//! `#!` dispatch and the `/bin/sh` fallback through `Command`, raw
//! `spawn_path`, and slibc's `execvp` and `posix_spawnp`.

use slopos_userland as _;

use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};
use slopos_userland::syscall::{UserTaskEntry, core as sys_core, process};

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const DIR: &str = "/tmp/script_exec";
const ENOEXEC: i32 = 8;
const EACCES: i32 = 13;

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

const LOCAL_BIN: &str = "/usr/local/bin";
const PROBE: &str = "slopos-path-probe";

/// Every command search with no `PATH` — slibc's `execvp`, the shell, `which`,
/// `env`, `xargs` and `find -exec` — walks the one default: it reaches
/// `/usr/local/bin`, where nothing may shadow `/bin`. The shell's
/// `command -p` walks it past any `PATH`.
fn every_search_takes_the_default_path_without_path() -> bool {
    let probe = format!("{LOCAL_BIN}/{PROBE}");
    let shadow = format!("{LOCAL_BIN}/cat");
    let installed = fs::create_dir_all(LOCAL_BIN)
        .and_then(|()| fs::write(&probe, "#!/bin/sh\necho probed \"$@\"\n"))
        .and_then(|()| fs::set_permissions(&probe, fs::Permissions::from_mode(0o755)))
        .and_then(|()| fs::write(&shadow, "#!/bin/sh\necho decoy\n"))
        .and_then(|()| fs::set_permissions(&shadow, fs::Permissions::from_mode(0o755)));
    if let Err(e) = installed {
        eprintln!("script_exec_test: installing the probes in {LOCAL_BIN}: {e}");
        return false;
    }
    let held = default_path_searches(&probe);
    let _ = fs::remove_file(&probe);
    let _ = fs::remove_file(&shadow);
    held
}

fn default_path_searches(probe: &str) -> bool {
    let pid = process::fork();
    if pid == 0 {
        // SAFETY: the forked child is single-threaded.
        unsafe { std::env::remove_var("PATH") };
        let file = format!("{PROBE}\0");
        let argv: [*const u8; 2] = [file.as_ptr(), core::ptr::null()];
        // SAFETY: NUL-terminated file, NULL-ended argv.
        unsafe { slopos_slibc::process::execvp(file.as_ptr(), argv.as_ptr()) };
        slopos_userland::syscall::core::exit_with_code(127);
    }
    let status = if pid < 0 {
        -1
    } else {
        process::wait_exit_code(pid as u32)
    };
    if status != 0 {
        eprintln!("script_exec_test: execvp of {PROBE} without PATH exited {status}");
        return false;
    }
    let default = slopos_abi::fs::DEFAULT_PATH.to_str().unwrap_or_default();
    let probed = "probed\n";
    let cases: [(&str, &[&str], Option<&str>, String); 9] = [
        (
            "/bin/shell",
            &["-c", "echo $PATH"],
            None,
            format!("{default}\n"),
        ),
        ("/bin/shell", &["-c", PROBE], None, probed.into()),
        ("/bin/which", &[PROBE], None, format!("{probe}\n")),
        ("/bin/which", &["cat"], None, "/bin/cat\n".into()),
        ("/bin/env", &[PROBE], None, probed.into()),
        ("/bin/xargs", &[PROBE], Some("x\n"), "probed x\n".into()),
        (
            "/bin/find",
            &[probe, "-exec", PROBE, "{}", ";"],
            None,
            format!("probed {probe}\n"),
        ),
        (
            "/bin/shell",
            &["-c", "cat"],
            Some("kept\n"),
            "kept\n".into(),
        ),
        ("/bin/env", &["cat"], Some("kept\n"), "kept\n".into()),
    ];
    for (program, args, stdin, want) in cases {
        let mut cmd = Command::new(program);
        cmd.args(args).env_clear();
        let got = match stdin {
            None => stdout_of(&mut cmd),
            Some(input) => stdout_with_stdin(&mut cmd, input),
        };
        if got.as_deref() != Some(want.as_str()) {
            eprintln!(
                "script_exec_test: {program} {args:?} with no PATH gave {got:?}, want {want:?}"
            );
            return false;
        }
    }
    for (script, want) in [
        (format!("command -p {PROBE}"), probed.to_owned()),
        (format!("command -p -v {PROBE}"), format!("{probe}\n")),
    ] {
        let got = stdout_of(
            Command::new("/bin/shell")
                .args(["-c", &script])
                .env_clear()
                .env("PATH", "/nowhere"),
        );
        if got.as_deref() != Some(want.as_str()) {
            eprintln!(
                "script_exec_test: {script:?} under PATH=/nowhere gave {got:?}, want {want:?}"
            );
            return false;
        }
    }
    true
}

fn stdout_with_stdin(cmd: &mut Command, input: &str) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = match cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("script_exec_test: spawn failed: {e}");
            return None;
        }
    };
    let fed = child
        .stdin
        .take()
        .is_some_and(|mut pipe| pipe.write_all(input.as_bytes()).is_ok());
    let out = child.wait_with_output().ok()?;
    (fed && out.status.success())
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
}

fn execvp_keeps_eacces_over_a_later_miss() -> bool {
    let _ = fs::create_dir_all(format!("{DIR}/denied"));
    if install("denied/vp_denied", "exit 0\n", 0o644).is_none() {
        return false;
    }
    let saved = std::env::var_os("PATH");
    // SAFETY: single-threaded test binary.
    unsafe { std::env::set_var("PATH", format!("{DIR}/denied:{DIR}/absent")) };
    let pid = process::fork();
    if pid == 0 {
        let argv: [*const u8; 2] = [b"vp_denied\0".as_ptr(), core::ptr::null()];
        // SAFETY: the name is NUL-terminated and the vector is NULL-ended.
        unsafe { slopos_slibc::process::execvp(b"vp_denied\0".as_ptr(), argv.as_ptr()) };
        slopos_userland::syscall::core::exit_with_code(slopos_slibc::errno_get());
    }
    match saved {
        // SAFETY: as above.
        Some(v) => unsafe { std::env::set_var("PATH", v) },
        None => unsafe { std::env::remove_var("PATH") },
    }
    let code = if pid < 0 {
        -1
    } else {
        process::wait_exit_code(pid as u32)
    };
    if code != EACCES {
        eprintln!("script_exec_test: execvp exited {code}, want EACCES");
        return false;
    }
    true
}

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

fn the_shell_runs_a_plain_file_as_a_script() -> bool {
    let Some(path) = install("plain_shell", "echo \"$0|$1\"\n", 0o755) else {
        return false;
    };
    let got = stdout_of(Command::new("/bin/sh").args(["-c", &format!("{path} z")]));
    let want = format!("{path}|z\n");
    if got.as_deref() != Some(want.as_str()) {
        eprintln!("script_exec_test: got {got:?}, want {want:?}");
        return false;
    }
    true
}

/// `read` on a pipe this test holds keeps the task alive until it is listed.
fn a_spawned_script_is_named_after_itself() -> bool {
    let Some(path) = install("named_probe", "#!/bin/sh\nread x\n", 0o755) else {
        return false;
    };
    let Ok((rd, wr)) = slopos_userland::syscall::fs::pipe() else {
        return false;
    };
    let argv = [b"named_probe\0".as_ptr()];
    let pid = process::spawn_path_with_actions(
        path.as_bytes(),
        &argv,
        TaskPriority::Normal,
        TASK_FLAG_USER_MODE,
        &[process::clone_fd(rd.raw(), 0)],
        0,
    );
    drop(rd);
    if pid <= 0 {
        eprintln!("script_exec_test: spawn of a script returned {pid}");
        return false;
    }
    let name = task_name(pid as u32);
    drop(wr);
    let _ = process::wait_exit_code(pid as u32);
    if name.as_deref() != Some("named_probe") {
        eprintln!("script_exec_test: the script's task is named {name:?}");
        return false;
    }
    true
}

fn task_name(pid: u32) -> Option<String> {
    let mut tasks = vec![UserTaskEntry::default(); 1024];
    let count = sys_core::process_list(&mut tasks).clamp(0, tasks.len() as i64) as usize;
    tasks[..count].iter().find(|t| t.task_id == pid).map(|t| {
        let end = t.name.iter().position(|&b| b == 0).unwrap_or(t.name.len());
        String::from_utf8_lossy(&t.name[..end]).into_owned()
    })
}

/// Polled: the forked child carries this test's name until its `execve` lands.
fn an_execed_script_is_named_after_itself() -> bool {
    let Some(path) = install("exec_named", "#!/bin/sh\nread x\n", 0o755) else {
        return false;
    };
    let Ok((rd, wr)) = slopos_userland::syscall::fs::pipe() else {
        return false;
    };
    let file = format!("{path}\0");
    let pid = process::fork();
    if pid == 0 {
        let _ = slopos_userland::syscall::fs::dup2(rd.raw(), 0);
        drop((rd, wr));
        let argv: [*const u8; 2] = [b"exec_named\0".as_ptr(), core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        process::execve(file.as_ptr(), argv.as_ptr(), envp.as_ptr());
        sys_core::exit_with_code(127);
    }
    drop(rd);
    if pid < 0 {
        return false;
    }
    let mut name = None;
    for _ in 0..2000 {
        name = task_name(pid as u32);
        if name.as_deref() == Some("exec_named") {
            break;
        }
        sys_core::sleep_ms(1);
    }
    drop(wr);
    let _ = process::wait_exit_code(pid as u32);
    if name.as_deref() != Some("exec_named") {
        eprintln!("script_exec_test: the exec'd script's task is named {name:?}");
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
            "every_search_takes_the_default_path_without_path",
            every_search_takes_the_default_path_without_path,
        ),
        (
            "execvp_keeps_eacces_over_a_later_miss",
            execvp_keeps_eacces_over_a_later_miss,
        ),
        (
            "posix_spawnp_runs_a_plain_file_under_sh",
            posix_spawnp_runs_a_plain_file_under_sh,
        ),
        (
            "the_shell_runs_a_plain_file_as_a_script",
            the_shell_runs_a_plain_file_as_a_script,
        ),
        (
            "a_spawned_script_is_named_after_itself",
            a_spawned_script_is_named_after_itself,
        ),
        (
            "an_execed_script_is_named_after_itself",
            an_execed_script_is_named_after_itself,
        ),
    ]);
}
