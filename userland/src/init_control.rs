//! `/run/init`, where init takes requests to end and restart the desktop
//! session, to take a change of the extensions and to re-execute itself, and
//! the login tags it gives the desktop, remoted, privd and extensions'
//! services and mechanisms, by which their processes are found.

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
/// The login tag of the copies of init that keep extensions' services' logs.
pub const LOGGER: u32 = 4;

/// The login tags of extensions' services: one each, made from its
/// extension's number and its place among the services it declares, so init
/// finds a service's processes again once it has re-executed.
const SERVICE_TAGS: u32 = 0x1_0000;

pub fn service_tag(number: u32, slot: usize) -> Option<u32> {
    use slopos_extension_core::accounts::{FIRST_UID, LAST_UID};
    use slopos_extension_core::declare::MAX_SERVICES;
    if !(FIRST_UID..=LAST_UID).contains(&number) || slot >= MAX_SERVICES {
        return None;
    }
    Some(SERVICE_TAGS + (number - FIRST_UID) * MAX_SERVICES as u32 + slot as u32)
}

/// The login tags of the mechanisms privd runs for extensions' actions, one
/// per extension, by which init ends them once their extension stops.
const ACTION_TAGS: u32 = 0x2_0000;

fn action_tags() -> std::ops::Range<u32> {
    use slopos_extension_core::accounts::{FIRST_UID, LAST_UID};
    ACTION_TAGS..ACTION_TAGS + LAST_UID - FIRST_UID + 1
}

pub fn action_tag(number: u32) -> Option<u32> {
    use slopos_extension_core::accounts::FIRST_UID;
    let tag = ACTION_TAGS + number.checked_sub(FIRST_UID)?;
    action_tags().contains(&tag).then_some(tag)
}

/// One reading of every task that has not exited, for asking several
/// questions of one moment, and of the caller's children that have but are
/// not yet reaped.
pub struct Tasks(Vec<UserTaskEntry>, Vec<u32>);

impl Tasks {
    pub fn read() -> Tasks {
        let me = std::process::id();
        let (live, gone): (Vec<_>, Vec<_>) = every_task().into_iter().partition(|task| {
            !matches!(
                TaskStatus::from_u8(task.state),
                TaskStatus::Invalid | TaskStatus::Terminated | TaskStatus::Zombie
            )
        });
        let unreaped = gone
            .iter()
            .filter(|task| {
                task.parent_task_id == me && TaskStatus::from_u8(task.state) == TaskStatus::Zombie
            })
            .map(|task| task.task_id)
            .collect();
        Tasks(live, unreaped)
    }

    /// Whether `pid` is a child of the caller's that has exited and that a
    /// `wait` has yet to reap.
    pub fn unreaped(&self, pid: u32) -> bool {
        self.1.contains(&pid)
    }

    /// [`processes_of`] as of this reading.
    pub fn processes_of(&self, logins: &[u32]) -> Vec<u32> {
        leaders(self.0.iter().filter(|task| logins.contains(&task.login)))
    }

    /// [`children`] as of this reading.
    pub fn children(&self, login: u32) -> Vec<u32> {
        let me = std::process::id();
        let mine: Vec<u32> = self
            .0
            .iter()
            .filter(|task| task.login == login && task.parent_task_id == me)
            .map(|task| task.process_id)
            .collect();
        leaders(self.0.iter().filter(|task| mine.contains(&task.process_id)))
    }

    /// Every service's tag some process carries.
    pub fn service_tags(&self) -> Vec<u32> {
        use slopos_extension_core::accounts::{FIRST_UID, LAST_UID};
        use slopos_extension_core::declare::MAX_SERVICES;
        self.tags_in(SERVICE_TAGS..SERVICE_TAGS + (LAST_UID - FIRST_UID + 1) * MAX_SERVICES as u32)
    }

    /// Every extension mechanism's tag some process carries.
    pub fn action_tags(&self) -> Vec<u32> {
        self.tags_in(action_tags())
    }

    fn tags_in(&self, range: std::ops::Range<u32>) -> Vec<u32> {
        let mut tags: Vec<u32> = self
            .0
            .iter()
            .map(|task| task.login)
            .filter(|tag| range.contains(tag))
            .collect();
        tags.sort_unstable();
        tags.dedup();
        tags
    }
}

/// Where a service finds the sockets init keeps for it, in the order its
/// declaration lists them: `LISTEN_FDS` of them from this descriptor on,
/// named in `LISTEN_FDNAMES`, `:` between, `tcp-<port>`, `udp-<port>` or a
/// socket's own name.
pub const LISTEN_FDS_START: i32 = crate::readiness::NOTIFIER_FD + 1;

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
    /// Start, stop and restart extensions' services as the store and the
    /// activations now say: uid 0.
    Extensions,
}

impl Command {
    pub const ALL: [Command; 4] = [
        Command::RestartSession,
        Command::EndSession,
        Command::Reexec,
        Command::Extensions,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Command::RestartSession => "restart-session",
            Command::EndSession => "end-session",
            Command::Reexec => "reexec",
            Command::Extensions => "extensions",
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
    processes_of(&[login])
}

/// [`processes`] carrying any of `logins`.
pub fn processes_of(logins: &[u32]) -> Vec<u32> {
    Tasks::read().processes_of(logins)
}

/// The leader of each process among `tasks`, its lowest task id, by pid.
fn leaders<'a>(tasks: impl Iterator<Item = &'a UserTaskEntry>) -> Vec<u32> {
    let mut leaders: Vec<(u32, u32)> = Vec::new();
    for task in tasks {
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

/// Every live thread outside the caller's process that a privd or its
/// mechanisms started, carrying [`PRIVD`] or an extension mechanism's tag,
/// by its never-reused id.
pub fn privd_threads_elsewhere() -> Vec<u32> {
    let tasks = tasks();
    let me = std::process::id();
    let mine = tasks
        .iter()
        .find(|task| task.task_id == me)
        .map(|task| task.process_id);
    tasks
        .iter()
        .filter(|task| task.login == PRIVD || action_tags().contains(&task.login))
        .filter(|task| Some(task.process_id) != mine)
        .map(|task| task.task_id)
        .collect()
}

/// The caller's children carrying `login`, by pid.
pub fn children(login: u32) -> Vec<u32> {
    Tasks::read().children(login)
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

fn every_task() -> Vec<UserTaskEntry> {
    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = sys_core::process_list(&mut tasks).clamp(0, MAX_TASKS as i64) as usize;
    tasks.truncate(count);
    tasks
}

/// Every task that has not exited.
fn tasks() -> Vec<UserTaskEntry> {
    let mut tasks = every_task();
    tasks.retain(|task| {
        !matches!(
            TaskStatus::from_u8(task.state),
            TaskStatus::Invalid | TaskStatus::Terminated | TaskStatus::Zombie
        )
    });
    tasks
}
