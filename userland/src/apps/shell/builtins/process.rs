use slopos_abi::signal::{
    NSIG, SIGABRT, SIGALRM, SIGBUS, SIGCHLD, SIGCONT, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGKILL,
    SIGPIPE, SIGQUIT, SIGSEGV, SIGSTOP, SIGTERM, SIGTRAP, SIGTSTP, SIGTTIN, SIGTTOU, SIGUSR1,
    SIGUSR2, SIGWINCH,
};

use crate::syscall::process;

use super::super::display::{COLOR_ERROR_RED, shell_error_named, shell_write_idx};
use super::super::exec;
use super::super::jobs;

fn parse_job_id(arg: &[u8]) -> Option<u16> {
    if arg.len() < 2 {
        return None;
    }
    if arg[0] != b'%' {
        return None;
    }
    let mut id: u16 = 0;
    for &b in &arg[1..] {
        if !b.is_ascii_digit() {
            return None;
        }
        id = id.checked_mul(10)?;
        id = id.checked_add((b - b'0') as u16)?;
    }
    if id == 0 {
        return None;
    }
    Some(id)
}

pub fn cmd_jobs(_argc: i32, _argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    jobs::render_jobs();
    0
}

const SIGNAL_NAMES: &[(&str, u8)] = &[
    ("HUP", SIGHUP),
    ("INT", SIGINT),
    ("QUIT", SIGQUIT),
    ("ILL", SIGILL),
    ("TRAP", SIGTRAP),
    ("ABRT", SIGABRT),
    ("BUS", SIGBUS),
    ("FPE", SIGFPE),
    ("KILL", SIGKILL),
    ("USR1", SIGUSR1),
    ("SEGV", SIGSEGV),
    ("USR2", SIGUSR2),
    ("PIPE", SIGPIPE),
    ("ALRM", SIGALRM),
    ("TERM", SIGTERM),
    ("CHLD", SIGCHLD),
    ("CONT", SIGCONT),
    ("STOP", SIGSTOP),
    ("TSTP", SIGTSTP),
    ("TTIN", SIGTTIN),
    ("TTOU", SIGTTOU),
    ("WINCH", SIGWINCH),
];

fn parse_signal(spec: &[u8]) -> Option<u8> {
    let text = jobs::arg_as_str(spec)?;
    if let Ok(num) = text.parse::<u8>() {
        return if (num as usize) < NSIG {
            Some(num)
        } else {
            None
        };
    }
    let name = match text.split_at_checked(3) {
        Some((head, tail)) if head.eq_ignore_ascii_case("SIG") => tail,
        _ => text,
    };
    SIGNAL_NAMES
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|&(_, num)| num)
}

/// A `%job` operand addresses the whole process group, which is what makes
/// `kill -STOP %1` suspend a pipeline rather than just its first stage.
fn signal_target(target: &[u8], signum: u8) -> i32 {
    if let Some(job_id) = parse_job_id(target) {
        let Some(pgid) = jobs::find_pgid_by_job_id(job_id) else {
            shell_write_idx(b"kill: unknown job\n", COLOR_ERROR_RED);
            return 1;
        };
        let Ok(group) = i32::try_from(pgid) else {
            shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
            return 1;
        };
        if process::kill_pid(-group, signum) < 0 {
            shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
            return 1;
        }
        if let Some(pid) = jobs::find_pid_by_job_id(job_id) {
            note_job_signal(pid, signum);
        }
        return 0;
    }

    let Some(pid) = jobs::parse_u32_arg(target) else {
        shell_write_idx(b"kill: invalid pid\n", COLOR_ERROR_RED);
        return 1;
    };
    let Ok(raw) = i32::try_from(pid) else {
        shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
        return 1;
    };
    if process::kill_pid(raw, signum) < 0 {
        shell_write_idx(b"kill: failed\n", COLOR_ERROR_RED);
        return 1;
    }
    note_job_signal(pid, signum);
    0
}

/// A job the shell just stopped or resumed has no `waitpid` report until the
/// next sweep, so record the transition now.
fn note_job_signal(pid: u32, signum: u8) {
    match signum {
        SIGCONT => {
            jobs::set_state_by_pid(pid, jobs::JobState::Running);
        }
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => {
            jobs::set_state_by_pid(pid, jobs::JobState::Stopped);
        }
        _ => {}
    }
}

/// `kill [-SIG] pid|%job...` — SIGTERM by default, as POSIX specifies.
pub fn cmd_kill(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    let argc = (argc.max(0) as usize).min(argv.len());
    let mut next = 1usize;
    let mut signum = SIGTERM;

    if next < argc && argv[next].len() > 1 && argv[next][0] == b'-' {
        let spec: &[u8] = if argv[next] == b"-s" {
            next += 1;
            if next >= argc {
                shell_write_idx(b"kill: -s needs a signal\n", COLOR_ERROR_RED);
                return 1;
            }
            argv[next]
        } else {
            &argv[next][1..]
        };
        let Some(parsed) = parse_signal(spec) else {
            shell_write_idx(b"kill: invalid signal\n", COLOR_ERROR_RED);
            return 1;
        };
        signum = parsed;
        next += 1;
    }

    if next >= argc {
        shell_write_idx(b"kill: missing pid or %job\n", COLOR_ERROR_RED);
        return 1;
    }

    let mut status = 0;
    for target in &argv[next..argc] {
        if signal_target(target, signum) != 0 {
            status = 1;
        }
    }
    status
}

pub fn cmd_fg(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    if argc < 2 {
        shell_write_idx(b"fg: missing %job\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(job_id) = parse_job_id(argv[1]) else {
        shell_write_idx(b"fg: expected %job\n", COLOR_ERROR_RED);
        return 1;
    };
    let (Some(pid), Some(pgid)) = (
        jobs::find_pid_by_job_id(job_id),
        jobs::find_pgid_by_job_id(job_id),
    ) else {
        shell_write_idx(b"fg: unknown job\n", COLOR_ERROR_RED);
        return 1;
    };
    let Ok(group) = i32::try_from(pgid) else {
        shell_write_idx(b"fg: failed\n", COLOR_ERROR_RED);
        return 1;
    };

    // The terminal has to be the job's before it is resumed, or a job stopped
    // by SIGTTIN re-stops on its first read.
    exec::enter_foreground(pgid);
    if process::kill_pid(-group, SIGCONT) < 0 {
        exec::leave_foreground();
        shell_write_idx(b"fg: failed\n", COLOR_ERROR_RED);
        return 1;
    }
    jobs::set_state_by_pid(pid, jobs::JobState::Running);

    let status = exec::wait_resumed_job(pid);
    if jobs::state_of(job_id) != Some(jobs::JobState::Stopped) {
        let _ = jobs::remove_by_job_id(job_id);
    }
    status
}

pub fn cmd_bg(argc: i32, argv: &[&[u8]]) -> i32 {
    jobs::refresh_liveness();
    if argc < 2 {
        shell_write_idx(b"bg: missing %job\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(job_id) = parse_job_id(argv[1]) else {
        shell_write_idx(b"bg: expected %job\n", COLOR_ERROR_RED);
        return 1;
    };
    let (Some(pid), Some(pgid)) = (
        jobs::find_pid_by_job_id(job_id),
        jobs::find_pgid_by_job_id(job_id),
    ) else {
        shell_write_idx(b"bg: unknown job\n", COLOR_ERROR_RED);
        return 1;
    };
    let Ok(group) = i32::try_from(pgid) else {
        shell_write_idx(b"bg: failed\n", COLOR_ERROR_RED);
        return 1;
    };
    if process::kill_pid(-group, SIGCONT) < 0 {
        shell_write_idx(b"bg: failed\n", COLOR_ERROR_RED);
        return 1;
    }
    jobs::set_state_by_pid(pid, jobs::JobState::Running);
    0
}

pub fn cmd_wait(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"wait: missing pid\n", COLOR_ERROR_RED);
        return 1;
    }
    let Some(pid) = jobs::parse_u32_arg(argv[1]) else {
        shell_write_idx(b"wait: invalid pid\n", COLOR_ERROR_RED);
        return 1;
    };
    process::wait_exit_code(pid)
}

/// `exit [n]` — end the shell with status `n`, or with the status of the last
/// command when no operand is given.
///
/// In a forked pipeline stage the returned status *is* the exit, so `exit | true`
/// ends only the subshell. In the shell's own process the request is merely
/// recorded, leaving the command loop to restore redirects and hand back the
/// terminal.
pub fn cmd_exit(argc: i32, argv: &[&[u8]]) -> i32 {
    let status = if argc >= 2 {
        match jobs::parse_u32_arg(argv[1]) {
            Some(n) => (n & 0xFF) as i32,
            None => {
                shell_error_named(b"exit", b"numeric argument required");
                super::super::exec::STATUS_SYNTAX_ERROR
            }
        }
    } else {
        super::super::last_exit_code()
    };

    if !super::super::interrupt::in_forked_child() {
        super::super::request_exit(status);
    }
    status
}

pub fn cmd_exec(argc: i32, argv: &[&[u8]]) -> i32 {
    if argc < 2 {
        shell_write_idx(b"exec: missing path\n", COLOR_ERROR_RED);
        return 1;
    }

    let path = argv[1];
    if path.is_empty() {
        shell_write_idx(b"exec: invalid path\n", COLOR_ERROR_RED);
        return 1;
    }

    // `exec_ptr` takes a NUL-terminated pointer.
    let mut buf = super::super::buffers::path_scratch();
    let len = path.len().min(buf.len() - 1);
    buf[..len].copy_from_slice(&path[..len]);
    buf[len] = 0;

    let rc = process::exec_ptr(buf.as_ptr());
    if rc < 0 {
        shell_write_idx(b"exec: failed\n", COLOR_ERROR_RED);
        1
    } else {
        0
    }
}
