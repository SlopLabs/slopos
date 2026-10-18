//! What init starts, and how often a crash may start it again.

use std::collections::VecDeque;
use std::ffi::CString;

use slopos_abi::caps::{DISPLAY_SEAT, INPUT_SEAT, TEST_HARNESS};
use slopos_abi::fs::{SESSION_GID, SESSION_HOME, SESSION_UID};
use slopos_abi::syscall::O_CLOEXEC;
use slopos_abi::task::{TASK_FLAG_USER_MODE, TaskPriority};

use crate::init_control::{self, DESKTOP, REMOTE};
use crate::readiness::NOTIFIER_FD;
use crate::syscall::process::{self, Endowment};
use crate::syscall::{OwnedFd, RawFd, SyscallError, fs, pidfd};

/// A program init starts: the capabilities it delegates beside the universal
/// ones, its tier, whether it runs as the session or as uid 0, the login tag
/// its process carries, and what of the base it reads beside its program, a
/// path ending in `/` naming all beneath it.
pub struct Service {
    pub path: &'static str,
    pub reads: &'static [&'static str],
    pub args: &'static [&'static str],
    pub caps: u64,
    pub priority: TaskPriority,
    pub session: bool,
    pub login: Option<u32>,
}

/// Runs what the policy allows, holding everything an action may name.
pub const PRIVD: Service = Service {
    path: "/sbin/privd",
    reads: &["/usr/share/slopos/privd/", PAIRING],
    args: &[],
    caps: slopos_priv_core::PRIVD_CAPS,
    priority: TaskPriority::Normal,
    session: false,
    login: Some(init_control::PRIVD),
};

pub const COMPOSITOR: Service = Service {
    path: init_control::COMPOSITOR,
    reads: &[],
    args: &[],
    caps: DISPLAY_SEAT | INPUT_SEAT,
    priority: TaskPriority::High,
    session: true,
    login: Some(DESKTOP),
};

pub const TERMINAL: Service = Service {
    path: init_control::TERMINAL,
    reads: &[],
    args: &[],
    caps: 0,
    priority: TaskPriority::Normal,
    session: true,
    login: Some(DESKTOP),
};

pub const REMOTED: Service = Service {
    path: "/bin/remoted",
    reads: &[PAIRING],
    args: &[],
    caps: 0,
    priority: TaskPriority::Normal,
    session: true,
    login: Some(REMOTE),
};

pub const ROULETTE: Service = Service {
    path: "/bin/roulette",
    reads: &[],
    args: &[],
    caps: DISPLAY_SEAT,
    priority: TaskPriority::Normal,
    session: true,
    login: None,
};

pub const RUN_TESTS: &str = "--run-tests";

/// A paired base's pairing, which remoted dials with and privd's `paired`
/// principal depends on.
pub const PAIRING: &str = "/usr/share/slopos/remote/";

/// A copy of init that runs the userland tests and exits with their verdict,
/// so init goes on serving while they run.
pub const TESTS: Service = Service {
    path: "/sbin/init",
    reads: &[],
    args: &[RUN_TESTS],
    caps: TEST_HARNESS,
    priority: TaskPriority::Normal,
    session: false,
    login: None,
};

/// The session's runtime directory, its own alone.
pub fn runtime_dir() -> String {
    format!("/run/user/{SESSION_UID}")
}

/// What every program the session starts finds in its environment.
pub fn session_env() -> [String; 4] {
    [
        format!("HOME={SESSION_HOME}"),
        "USER=user".to_owned(),
        "LOGNAME=user".to_owned(),
        format!("XDG_RUNTIME_DIR={}", runtime_dir()),
    ]
}

/// A child init waits on, and a pidfd that wakes it when the child exits.
pub struct Child {
    pub pid: u32,
    pub exited: Option<OwnedFd>,
}

impl Child {
    pub fn watch(pid: u32) -> Child {
        Child {
            pid,
            exited: pidfd::pidfd_open_owned(pid),
        }
    }
}

/// Start `service` with init's stdio and each `(fd, at)`: init's `fd` at the
/// child's `at`.
pub fn start(service: &Service, fds: &[(RawFd, RawFd)]) -> Option<Child> {
    let mut actions = vec![
        process::clone_fd(0, 0),
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
    ];
    actions.extend(fds.iter().map(|&(fd, at)| process::clone_fd(fd, at)));
    let argv: Vec<CString> = std::iter::once(service.path)
        .chain(service.args.iter().copied())
        .map(|arg| CString::new(arg).expect("no NUL in a service's argv"))
        .collect();
    let argv: Vec<*const u8> = argv.iter().map(|arg| arg.as_ptr().cast()).collect();
    let env: Vec<CString> = session_env()
        .into_iter()
        .map(|var| CString::new(var).expect("no NUL in the session's environment"))
        .collect();
    let (envp, cwd, ids): (Vec<*const u8>, &str, _) = if service.session {
        let cwd = if std::path::Path::new(SESSION_HOME).is_dir() {
            SESSION_HOME
        } else {
            "/"
        };
        (
            env.iter().map(|var| var.as_ptr().cast()).collect(),
            cwd,
            Some((SESSION_UID, SESSION_GID)),
        )
    } else {
        (Vec::new(), "/", None)
    };
    let tid = process::spawn_endowed(
        service.path.as_bytes(),
        &argv,
        &envp,
        cwd.as_bytes(),
        service.priority,
        TASK_FLAG_USER_MODE,
        &actions,
        0,
        Endowment {
            caps: service.caps,
            ids,
            groups: service.session.then_some(&[][..]),
            login: service.login,
        },
    );
    if tid <= 0 {
        eprintln!(
            "init: cannot start {}: {}",
            service.path,
            SyscallError::from_errno(-tid)
        );
        return None;
    }
    Some(Child::watch(tid as u32))
}

/// [`start`], handing the child a readiness notifier: the descriptor answered
/// turns readable once it is ready, or once it has exited.
pub fn start_ready(service: &Service, fds: &[(RawFd, RawFd)]) -> Option<(Child, OwnedFd)> {
    let (ready, notifier) = fs::pipe2(O_CLOEXEC as u32).ok()?;
    let mut fds = fds.to_vec();
    fds.push((notifier.raw(), NOTIFIER_FD));
    let child = start(service, &fds)?;
    Some((child, ready))
}

/// Starts of one service, at most [`STARTS`] in any [`WINDOW_MS`], as
/// systemd's start limit counts them: past that, the next waits until the
/// window allows it.
#[derive(Default)]
pub struct Budget {
    starts: VecDeque<u64>,
}

pub const STARTS: usize = 5;
pub const WINDOW_MS: u64 = 60_000;

/// How long a service told to end for a new base may take before its
/// successor starts beside it: each finishes what it serves first, and a
/// mechanism waiting on its terminal may take far longer than this.
pub const DRAIN_MS: u64 = 5_000;

impl Budget {
    /// When a start may happen, `now` at the earliest.
    pub fn next(&self, now: u64) -> u64 {
        let recent: Vec<u64> = self
            .starts
            .iter()
            .copied()
            .filter(|&at| now.saturating_sub(at) < WINDOW_MS)
            .collect();
        match recent.len().checked_sub(STARTS) {
            Some(over) => recent[over] + WINDOW_MS,
            None => now,
        }
    }

    pub fn record(&mut self, now: u64) {
        self.starts.retain(|&at| now.saturating_sub(at) < WINDOW_MS);
        self.starts.push_back(now);
    }

    pub fn clear(&mut self) {
        self.starts.clear();
    }

    pub fn starts(&self) -> impl Iterator<Item = u64> + '_ {
        self.starts.iter().copied()
    }

    pub fn of(starts: impl IntoIterator<Item = u64>) -> Budget {
        Budget {
            starts: starts.into_iter().collect(),
        }
    }
}

/// A service init starts again when it exits.
pub struct Supervised {
    pub service: &'static Service,
    pub child: Option<Child>,
    pub budget: Budget,
    /// When it is to start again, while it is not running.
    pub due: Option<u64>,
    /// When its successor starts beside it, once it was told to end.
    pub draining: Option<u64>,
}

impl Supervised {
    /// Whether `switch` changed what this service runs or reads.
    pub fn changed_by(&self, switch: &crate::base_watch::Switch) -> bool {
        switch.changed(&[self.service.path]) || switch.changed(self.service.reads)
    }

    /// Have it end so the base it runs from now starts in its place, its
    /// budget cleared: privd and remoted drain on `SIGTERM`.
    pub fn restart(&mut self, now: u64) {
        self.budget.clear();
        match &self.child {
            Some(child) => {
                let _ = process::kill(child.pid, slopos_abi::signal::SIGTERM);
                self.draining = Some(now + DRAIN_MS);
            }
            None => self.due = Some(now),
        }
    }

    /// The child, once it has drained for [`DRAIN_MS`] without exiting: it
    /// is no longer this service's, which starts again now.
    pub fn retire(&mut self, now: u64) -> Option<Child> {
        if !self.draining.is_some_and(|at| at <= now) {
            return None;
        }
        self.draining = None;
        self.due = Some(now);
        self.child.take()
    }

    pub fn new(service: &'static Service) -> Supervised {
        Supervised {
            service,
            child: None,
            budget: Budget::default(),
            due: None,
            draining: None,
        }
    }

    pub fn is(&self, pid: u32) -> bool {
        self.child.as_ref().is_some_and(|child| child.pid == pid)
    }

    pub fn due_by(&self, now: u64) -> bool {
        self.child.is_none() && self.due.is_some_and(|at| at <= now)
    }

    /// A start that fails is tried again when the budget next allows.
    pub fn start(&mut self, now: u64, fds: &[(RawFd, RawFd)]) {
        self.budget.record(now);
        self.child = start(self.service, fds);
        self.due = self.child.is_none().then(|| self.budget.next(now));
    }

    pub fn exited(&mut self, status: process::WaitStatus, now: u64) {
        self.child = None;
        self.draining = None;
        let due = self.budget.next(now);
        let path = self.service.path;
        if due > now {
            eprintln!(
                "init: {path} exited ({status:?}), its {STARTS}th in a minute; it starts again in {} s",
                (due - now).div_ceil(1000)
            );
        } else {
            eprintln!("init: {path} exited ({status:?})");
        }
        self.due = Some(due);
    }
}
