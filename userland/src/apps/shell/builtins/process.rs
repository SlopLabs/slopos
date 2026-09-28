use slopos_abi::signal::{
    NSIG, SIG_DFL, SIG_IGN, SIGCHLD, SIGCONT, SIGSTOP, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU, WNOHANG,
};
use slopos_abi::syscall::{POLLIN, UserPollFd};
use slopos_shell_core::trap::signal_by_name;

use crate::syscall::{SyscallError, fs, pidfd, process};

use super::super::display::{COLOR_ERROR_RED, shell_error_named, shell_write_idx};
use super::super::exec;
use super::super::{interrupt, jobs, traps};

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

fn parse_signal(spec: &[u8]) -> Option<u8> {
    let text = jobs::arg_as_str(spec)?;
    if let Ok(num) = text.parse::<u8>() {
        return if (num as usize) <= NSIG {
            Some(num)
        } else {
            None
        };
    }
    signal_by_name(&spec.to_ascii_uppercase())
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

/// `wait [pid|%job...]` (POSIX): every child, or each operand in turn with the
/// last one's status, 127 if unknown. A trapped signal ends it at once.
pub fn cmd_wait(argc: i32, argv: &[&[u8]]) -> i32 {
    let argc = (argc.max(0) as usize).min(argv.len());
    if argc < 2 {
        return wait_for_every_child();
    }
    let mut status = 0;
    for &operand in &argv[1..argc] {
        let pid = match parse_job_id(operand) {
            Some(job_id) => jobs::find_pid_by_job_id(job_id),
            None if operand.first() == Some(&b'%') => None,
            None => match jobs::parse_u32_arg(operand) {
                Some(pid) => Some(pid),
                None => {
                    shell_error_named(operand, b"not a pid or job");
                    status = 1;
                    continue;
                }
            },
        };
        status = match pid.map(wait_for_child) {
            Some(Waited::Status(code)) => {
                if let Some(pid) = pid {
                    jobs::remove_by_pid(pid);
                }
                code
            }
            Some(Waited::Interrupted(code)) => return code,
            Some(Waited::Unknown) | None => 127,
        };
    }
    status
}

enum Waited {
    Status(i32),
    /// Not a child of this shell, or already reaped.
    Unknown,
    /// A trapped signal or an interrupt ended the wait, with this status.
    Interrupted(i32),
}

fn interrupted() -> Option<i32> {
    if let Some(signum) = traps::pending_signal() {
        return Some(128 + signum as i32);
    }
    interrupt::take_pending().then_some(interrupt::EXIT_INTERRUPTED)
}

/// The signal handler's write to the wake pipe is what closes the window
/// between the check for a trapped signal and `poll` blocking.
fn wait_for_child(pid: u32) -> Waited {
    let wake = traps::WakePipe::arm();
    let child = pidfd::pidfd_open_owned(pid);
    let mut block = wake.is_none() || child.is_none();
    loop {
        if let Some(code) = interrupted() {
            return Waited::Interrupted(code);
        }
        let mut status = 0i32;
        let rc = process::waitpid_raw(pid as i32, &mut status, if block { 0 } else { WNOHANG });
        if rc > 0 {
            return Waited::Status(process::wait_status(status).exit_code().unwrap_or(-1));
        }
        if rc < 0 && rc != slopos_abi::Errno::EINTR.raw() as i64 {
            return Waited::Unknown;
        }
        if let (0, Some(wake), Some(child)) = (rc, &wake, &child) {
            let mut fds = [
                UserPollFd {
                    fd: child.raw(),
                    events: POLLIN,
                    revents: 0,
                },
                UserPollFd {
                    fd: wake.fd(),
                    events: POLLIN,
                    revents: 0,
                },
            ];
            match fs::poll(&mut fds, -1) {
                // An exited child is reaped at once by a blocking wait.
                Ok(_) if fds[0].revents != 0 => block = true,
                Ok(_) => wake.drain(),
                Err(e) if e == SyscallError::EINTR => {}
                Err(_) => block = true,
            }
        }
    }
}

extern "C" fn wake_on_child_exit(_signum: i32) {
    traps::wake_waiter();
}

/// While held, a child's exit makes an armed [`traps::WakePipe`] readable.
struct ChildExitWake {
    restore_ignored: bool,
}

impl ChildExitWake {
    /// `Ok(None)` when a `CHLD` trap's handler already wakes the pipe.
    fn install() -> Result<Option<Self>, ()> {
        let handler = process::signal_handler(SIGCHLD).ok_or(())?;
        if handler != SIG_DFL && handler != SIG_IGN {
            return Ok(None);
        }
        if process::set_signal_handler(SIGCHLD, wake_on_child_exit) != 0 {
            return Err(());
        }
        Ok(Some(Self {
            restore_ignored: handler == SIG_IGN,
        }))
    }
}

impl Drop for ChildExitWake {
    fn drop(&mut self) {
        if self.restore_ignored {
            process::ignore_signal(SIGCHLD);
        } else {
            process::default_signal(SIGCHLD);
        }
    }
}

/// POSIX: every child, in the job table or not; then status 0.
fn wait_for_every_child() -> i32 {
    let wake = traps::WakePipe::arm();
    let notify = ChildExitWake::install();
    let mut polls = wake.is_some() && notify.is_ok();
    loop {
        if let Some(code) = interrupted() {
            return code;
        }
        let mut status = 0i32;
        let rc = process::waitpid_raw(-1, &mut status, if polls { WNOHANG } else { 0 });
        if rc > 0 {
            jobs::remove_by_pid(rc as u32);
            continue;
        }
        if rc < 0 && rc != slopos_abi::Errno::EINTR.raw() as i64 {
            return 0;
        }
        if let (0, Some(wake)) = (rc, &wake) {
            let mut fds = [UserPollFd {
                fd: wake.fd(),
                events: POLLIN,
                revents: 0,
            }];
            match fs::poll(&mut fds, -1) {
                Ok(_) => wake.drain(),
                Err(e) if e == SyscallError::EINTR => {}
                Err(_) => polls = false,
            }
        }
    }
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
        // POSIX: in a trap action, the status from before the action.
        traps::status_before_action().unwrap_or_else(super::super::last_exit_code)
    };

    if !interrupt::in_forked_child() {
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
