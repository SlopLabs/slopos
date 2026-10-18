//! `/run/init`, where init takes requests to end and restart the desktop
//! session and to re-execute itself, and the login tags it gives the desktop
//! and remoted, by which a session's processes are found.

use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{MSG_NOSIGNAL, POLLIN, UserPollFd, UserTaskEntry};
use slopos_abi::task::{MAX_TASKS, TaskStatus};

use crate::syscall::{SyscallError, core as sys_core, fs, net};

pub const SOCKET: &str = "/run/init";

/// The desktop session's programs, which a new base changes only once the
/// session restarts.
pub const COMPOSITOR: &str = "/bin/compositor";
pub const TERMINAL: &str = "/bin/terminal";

/// The login tag of every process the desktop session runs.
pub const DESKTOP: u32 = 1;
/// The login tag of remoted and what it runs, which a session restart spares.
pub const REMOTE: u32 = 2;
/// The login tag of privd and the mechanisms it runs, which outlive it: a
/// privd started again takes no exclusive group while its predecessor's run.
pub const PRIVD: u32 = 3;

/// The longest request line init reads.
pub const MAX_REQUEST: usize = 64;
/// Init answers an end of the session once its processes are gone.
const REPLY_MS: i64 = 30_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// End the desktop session and start it again: the session's uid or uid 0.
    RestartSession,
    /// End the desktop session and leave it ended: uid 0.
    EndSession,
    /// Re-execute `/sbin/init`, keeping its services: uid 0.
    Reexec,
}

impl Command {
    pub const ALL: [Command; 3] = [
        Command::RestartSession,
        Command::EndSession,
        Command::Reexec,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Command::RestartSession => "restart-session",
            Command::EndSession => "end-session",
            Command::Reexec => "reexec",
        }
    }

    pub fn parse(word: &[u8]) -> Option<Command> {
        Command::ALL
            .into_iter()
            .find(|command| command.word().as_bytes() == word)
    }
}

/// Ask init for `command`: `Err` is init's refusal, or why init was not
/// reached.
pub fn ask(command: Command) -> Result<(), String> {
    let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0)
        .map_err(|e| format!("socket: {e}"))?;
    net::connect_unix(fd.raw(), SOCKET.as_bytes()).map_err(|e| format!("{SOCKET}: {e}"))?;
    let line = format!("{}\n", command.word());
    net::send(fd.raw(), line.as_bytes(), MSG_NOSIGNAL).map_err(|e| format!("{SOCKET}: {e}"))?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let mut fds = [UserPollFd {
            fd: fd.raw(),
            events: POLLIN,
            revents: 0,
        }];
        match fs::poll(&mut fds, REPLY_MS) {
            Ok(0) => return Err("init did not answer".into()),
            Err(SyscallError::EINTR) => continue,
            Err(e) => return Err(format!("{SOCKET}: {e}")),
            Ok(_) => {}
        }
        match fs::read_slice(fd.raw(), &mut buf) {
            Ok(0) => break,
            Ok(n) if reply.len() + n <= buf.len() => reply.extend_from_slice(&buf[..n]),
            Ok(_) => return Err("init answered too much".into()),
            Err(SyscallError::EINTR) => {}
            Err(e) => return Err(format!("{SOCKET}: {e}")),
        }
    }
    match String::from_utf8_lossy(&reply).trim_end() {
        "ok" => Ok(()),
        "" => Err("init closed the connection without answering".into()),
        refusal => Err(refusal.to_owned()),
    }
}

/// Every live process carrying `login`, by the lowest of its live threads'
/// never-reused ids, its first thread's while that one lives.
pub fn processes(login: u32) -> Vec<u32> {
    let mut leaders: Vec<(u32, u32)> = Vec::new();
    for task in tasks().iter().filter(|task| task.login == login) {
        match leaders
            .iter_mut()
            .find(|(process, _)| *process == task.process_id)
        {
            Some((_, leader)) => *leader = (*leader).min(task.task_id),
            None => leaders.push((task.process_id, task.task_id)),
        }
    }
    let mut pids: Vec<u32> = leaders.into_iter().map(|(_, leader)| leader).collect();
    pids.sort_unstable();
    pids
}

/// Every live thread carrying `login` outside the caller's process, by its
/// never-reused id.
pub fn threads_elsewhere(login: u32) -> Vec<u32> {
    let tasks = tasks();
    let me = std::process::id();
    let mine = tasks
        .iter()
        .find(|task| task.task_id == me)
        .map(|task| task.process_id);
    tasks
        .iter()
        .filter(|task| task.login == login && Some(task.process_id) != mine)
        .map(|task| task.task_id)
        .collect()
}

/// The caller's children carrying `login`, by pid.
pub fn children(login: u32) -> Vec<u32> {
    let me = std::process::id();
    tasks()
        .iter()
        .filter(|task| task.login == login && task.parent_task_id == me)
        .map(|task| task.task_id)
        .collect()
}

/// Who listens at the `AF_UNIX` name `path`, as the kernel recorded it at
/// `listen`.
pub fn listener(path: &str) -> Option<u32> {
    let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0).ok()?;
    net::connect_unix(fd.raw(), path.as_bytes()).ok()?;
    net::peer_cred(fd.raw()).ok().map(|cred| cred.pid as u32)
}

/// The login tag of process `pid` while it is alive.
pub fn login_of(pid: u32) -> Option<u32> {
    tasks()
        .into_iter()
        .find(|task| task.task_id == pid)
        .map(|task| task.login)
}

/// Every task that has not exited.
fn tasks() -> Vec<UserTaskEntry> {
    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = sys_core::process_list(&mut tasks).clamp(0, MAX_TASKS as i64) as usize;
    tasks.truncate(count);
    tasks.retain(|task| {
        !matches!(
            TaskStatus::from_u8(task.state),
            TaskStatus::Invalid | TaskStatus::Terminated | TaskStatus::Zombie
        )
    });
    tasks
}
