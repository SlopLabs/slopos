//! `/run/init`, where init takes requests to log people in, end, restart,
//! lock and switch their sessions, to take a change of the extensions and to
//! re-execute itself; the login tags it gives each session, remoted, privd
//! and extensions' services and mechanisms, by which their processes are
//! found; and the descriptors it hands what it starts.

use slopos_abi::net::{AF_UNIX, SOCK_CLOEXEC, SOCK_STREAM};
use slopos_abi::syscall::{MSG_NOSIGNAL, POLLIN, UserPollFd, UserTaskEntry};
use slopos_abi::task::{MAX_TASKS, TaskStatus};

use crate::syscall::{SyscallError, core as sys_core, fs, net};

pub const SOCKET: &str = "/run/init";

/// The desktop session's programs, which a new base changes only once the
/// session restarts.
pub const COMPOSITOR: &str = "/bin/compositor";
pub const TERMINAL: &str = "/bin/terminal";

/// The login tag of remoted and what it runs, which a session restart spares.
pub const REMOTE: u32 = 1;
/// The login tag of privd and the mechanisms it runs, which outlive it: a
/// privd started again takes no exclusive group while its predecessor's run.
pub const PRIVD: u32 = 2;
/// The login tag of the copies of init that keep extensions' services' logs.
pub const LOGGER: u32 = 3;

/// A paired base's pairing, which remoted dials with and privd's `paired`
/// principal depends on.
pub const PAIRING: &str = "/usr/share/slopos/remote/";
const PAIRING_CONF: &str = "/usr/share/slopos/remote/remote.conf";

/// Whether the running base carries a pairing.
pub fn paired() -> bool {
    std::path::Path::new(PAIRING_CONF).is_file()
}

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
        Self::of(every_task())
    }

    /// [`read`](Self::read), `None` when the kernel would not list the tasks.
    pub fn try_read() -> Option<Tasks> {
        Some(Self::of(listed()?))
    }

    fn of(every: Vec<UserTaskEntry>) -> Tasks {
        let me = std::process::id();
        let (live, gone): (Vec<_>, Vec<_>) = every.into_iter().partition(|task| {
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

    /// Those of [`processes_of`](Self::processes_of) parked on a stop
    /// signal.
    pub fn stopped_of(&self, logins: &[u32]) -> Vec<u32> {
        leaders(self.0.iter().filter(|task| {
            logins.contains(&task.login) && TaskStatus::from_u8(task.state) == TaskStatus::Stopped
        }))
    }

    /// The login tag of the live process `pid` names by its leader.
    pub fn login_of(&self, pid: u32) -> Option<u32> {
        self.0
            .iter()
            .find(|task| task.task_id == pid)
            .map(|task| task.login)
    }

    /// The live task that started the live process `pid`.
    pub fn parent_of(&self, pid: u32) -> Option<u32> {
        let parent = self
            .0
            .iter()
            .find(|task| task.task_id == pid)?
            .parent_task_id;
        self.0
            .iter()
            .any(|task| task.task_id == parent)
            .then_some(parent)
    }

    /// The first task still alive that carries `login`: task ids are never
    /// reused, so a session started again has another.
    pub fn eldest(&self, login: u32) -> Option<u32> {
        self.0
            .iter()
            .filter(|task| task.login == login)
            .map(|task| task.task_id)
            .min()
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

    /// Every tag in `range` some process carries.
    pub fn tags_in_range(&self, range: std::ops::Range<u32>) -> Vec<u32> {
        self.tags_in(range)
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

/// Where what init starts finds the descriptors init hands it, past the
/// readiness notifier at [`NOTIFIER_FD`](crate::readiness::NOTIFIER_FD).
pub mod fds {
    use crate::readiness::NOTIFIER_FD;
    use crate::syscall::RawFd;
    use slopos_priv_core::sessions::LINKS;

    /// Where an extension's service finds the sockets init keeps for it, in
    /// the order its declaration lists them: `LISTEN_FDS` of them from this
    /// descriptor on, named in `LISTEN_FDNAMES`, `:` between, `tcp-<port>`,
    /// `udp-<port>` or a socket's own name.
    pub const LISTEN_START: RawFd = NOTIFIER_FD + 1;
    /// A compositor's consent link to privd.
    pub const COMPOSITOR_CONSENT: RawFd = NOTIFIER_FD + 1;
    /// The pipe init tells a compositor on whether its session is in front.
    pub const COMPOSITOR_SESSION: RawFd = COMPOSITOR_CONSENT + 1;
    /// privd's consent link `n` is at `PRIVD_LINKS + n`, one per desktop or
    /// login screen.
    pub const PRIVD_LINKS: RawFd = NOTIFIER_FD + 1;
    /// The pair privd hears the session table on.
    pub const PRIVD_TABLE: RawFd = PRIVD_LINKS + LINKS as RawFd;
}

/// The longest request line init reads.
pub const MAX_REQUEST: usize = 64;
/// Init answers an end of the session once its processes are gone.
const REPLY_MS: i64 = 30_000;

/// A person's name as a request carries it, at most
/// [`slopos_accounts_core::NAME_MAX`] bytes the record's grammar takes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Name {
    bytes: [u8; slopos_accounts_core::NAME_MAX],
    len: u8,
}

impl Name {
    pub fn new(name: &str) -> Option<Name> {
        if !slopos_accounts_core::valid_name(name) {
            return None;
        }
        let mut bytes = [0u8; slopos_accounts_core::NAME_MAX];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Some(Name {
            bytes,
            len: name.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

impl core::fmt::Debug for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a person logs in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum At {
    /// Their desktop, from the login screen.
    Desktop,
    /// A shell on the terminal the request hands over.
    Console,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// End the requester's session and start it again; from uid 0, every
    /// session but the background's, starting the boot's own again.
    RestartSession,
    /// End the requester's session, which logs its person out; from uid 0,
    /// every session but the background's, leaving them ended.
    EndSession,
    /// End the session the login tag names and start it again: uid 0.
    Restart(u32),
    /// End the session the login tag names: uid 0.
    End(u32),
    /// Re-execute `/sbin/init`, keeping its services: uid 0.
    Reexec,
    /// Start, stop and restart extensions' services as the store and the
    /// activations now say: uid 0.
    Extensions,
    /// Bring the login screen in front, starting it: uid 0, or a person from
    /// their desktop in front.
    Greeter,
    /// Bring the desktop the login tag names in front: uid 0.
    Switch(u32),
    /// Log a person in, whose password the requester checked: uid 0.
    Login(At, Name),
    /// Lock the requester's desktop.
    Lock,
    /// The desktop's compositor says it locked or unlocked.
    Locked(bool),
}

impl Command {
    pub fn word(self) -> &'static str {
        match self {
            Command::RestartSession | Command::Restart(_) => "restart-session",
            Command::EndSession | Command::End(_) => "end-session",
            Command::Reexec => "reexec",
            Command::Extensions => "extensions",
            Command::Greeter => "greeter",
            Command::Switch(_) => "switch",
            Command::Login(..) => "login",
            Command::Lock => "lock",
            Command::Locked(true) => "locked",
            Command::Locked(false) => "unlocked",
        }
    }

    /// The request line, without its newline.
    pub fn line(self) -> String {
        match self {
            Command::Restart(tag) | Command::End(tag) | Command::Switch(tag) => {
                format!("{} {tag}", self.word())
            }
            Command::Login(At::Desktop, name) => format!("login desktop {}", name.as_str()),
            Command::Login(At::Console, name) => format!("login console {}", name.as_str()),
            _ => self.word().to_owned(),
        }
    }

    pub fn parse(line: &[u8]) -> Option<Command> {
        let line = core::str::from_utf8(line).ok()?;
        let words: Vec<&str> = line.split(' ').collect();
        let tag = |word: &str| -> Option<u32> {
            let canonical = !word.is_empty()
                && word.bytes().all(|b| b.is_ascii_digit())
                && !word.starts_with('0');
            canonical.then(|| word.parse().ok()).flatten()
        };
        Some(match words[..] {
            ["restart-session"] => Command::RestartSession,
            ["end-session"] => Command::EndSession,
            ["restart-session", t] => Command::Restart(tag(t)?),
            ["end-session", t] => Command::End(tag(t)?),
            ["reexec"] => Command::Reexec,
            ["extensions"] => Command::Extensions,
            ["greeter"] => Command::Greeter,
            ["switch", t] => Command::Switch(tag(t)?),
            ["login", "desktop", name] => Command::Login(At::Desktop, Name::new(name)?),
            ["login", "console", name] => Command::Login(At::Console, Name::new(name)?),
            ["lock"] => Command::Lock,
            ["locked"] => Command::Locked(true),
            ["unlocked"] => Command::Locked(false),
            _ => return None,
        })
    }
}

/// Ask init for `command`: `Err` is init's refusal, or why init was not
/// reached.
pub fn ask(command: Command) -> Result<(), String> {
    ask_handing(command, None)
}

/// [`ask`], handing init `fd` beside the request: the terminal a console
/// login starts its shell on.
pub fn ask_handing(command: Command, fd: Option<crate::syscall::RawFd>) -> Result<(), String> {
    let fd_held = fd;
    let fd = net::socket(AF_UNIX, u32::from(SOCK_STREAM) | SOCK_CLOEXEC, 0)
        .map_err(|e| format!("socket: {e}"))?;
    net::connect_unix(fd.raw(), SOCKET.as_bytes()).map_err(|e| format!("{SOCKET}: {e}"))?;
    let line = format!("{}\n", command.line());
    match fd_held {
        Some(held) => net::send_with_fds(fd.raw(), line.as_bytes(), &[held])
            .map(|_| ())
            .map_err(|e| format!("{SOCKET}: {e}"))?,
        None => net::send(fd.raw(), line.as_bytes(), MSG_NOSIGNAL)
            .map(|_| ())
            .map_err(|e| format!("{SOCKET}: {e}"))?,
    }
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

/// Every live thread of a mechanism a privd started, carrying [`PRIVD`] or
/// an extension mechanism's tag, by its never-reused id: an earlier privd,
/// which init started as it did the caller and which starts nothing once it
/// drains, is none.
pub fn privd_threads_elsewhere() -> Vec<u32> {
    let tasks = tasks();
    let me = std::process::id();
    let mine = tasks
        .iter()
        .find(|task| task.task_id == me)
        .map(|task| task.process_id);
    let started_by = |process: u32| {
        tasks
            .iter()
            .filter(|task| task.process_id == process)
            .min_by_key(|task| task.task_id)
            .map(|task| task.parent_task_id)
    };
    let init = mine.and_then(started_by);
    tasks
        .iter()
        .filter(|task| task.login == PRIVD || action_tags().contains(&task.login))
        .filter(|task| Some(task.process_id) != mine)
        .filter(|task| init.is_none() || started_by(task.process_id) != init)
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
    Tasks::read().login_of(pid)
}

fn every_task() -> Vec<UserTaskEntry> {
    listed().unwrap_or_default()
}

fn listed() -> Option<Vec<UserTaskEntry>> {
    let mut tasks = vec![UserTaskEntry::default(); MAX_TASKS];
    let count = usize::try_from(sys_core::process_list(&mut tasks)).ok()?;
    tasks.truncate(count.min(MAX_TASKS));
    Some(tasks)
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
