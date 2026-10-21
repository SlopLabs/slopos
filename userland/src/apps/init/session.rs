//! The desktop session: the compositor and the terminal, and whatever they
//! start, all carrying the desktop's login tag, and the extensions' session
//! services, each carrying its own; by those tags the session is ended.

use slopos_abi::signal::{SIGCONT, SIGKILL, SIGTERM};

use super::service::Child;
use crate::init_control::{DESKTOP, Tasks};
use crate::syscall::{OwnedFd, process};

/// How long what is ended has on `SIGTERM` before `SIGKILL`, and how long
/// after that init stops waiting for it.
const KILL_AFTER_MS: u64 = 5000;
const GIVE_UP_AFTER_MS: u64 = 10_000;

pub enum Session {
    Down,
    /// The terminal starts once the compositor is ready, or is given up on.
    Starting {
        compositor: Child,
        ready: OwnedFd,
        since: u64,
    },
    Running {
        compositor: Child,
    },
    Ending(Ending),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Then {
    Start,
    Stay,
}

/// How an end went.
pub enum Ended {
    All,
    /// This many processes outlived the wait.
    GaveUp(usize),
}

pub struct Ending {
    termination: Termination,
    /// The compositor until init has seen it exit, which privd hears of.
    pub compositor: Option<u32>,
    pub then: Then,
    /// Requests to answer once the session has ended.
    pub waiting: Vec<OwnedFd>,
}

/// Signal passes in one step: a pass finds what a process forked during
/// the one before.
const PASSES: usize = 8;

impl Ending {
    pub fn new(now: u64, then: Then, compositor: Option<u32>) -> Ending {
        Ending {
            termination: Termination::new(now),
            compositor,
            then,
            waiting: Vec::new(),
        }
    }

    /// Signal what is left of the session, the processes carrying the
    /// desktop's tag and each of `services`; `Some` once none runs, or once
    /// init has waited long enough for them.
    pub fn step(&mut self, now: u64, services: &[u32]) -> Option<Ended> {
        let tags: Vec<u32> = std::iter::once(DESKTOP)
            .chain(services.iter().copied())
            .collect();
        let ended = self.termination.step(now, &tags);
        if let Some(Ended::GaveUp(left)) = &ended {
            eprintln!("init: {left} processes of the desktop session would not end");
        }
        ended
    }
}

/// Ending everything that carries some login tags: `SIGTERM` to each
/// process, then to each forked since, and `SIGCONT` to whichever is
/// stopped, `SIGKILL` to all after [`KILL_AFTER_MS`], given up on after
/// [`GIVE_UP_AFTER_MS`].
pub struct Termination {
    since: u64,
    signalled: Vec<u32>,
}

impl Termination {
    pub fn new(now: u64) -> Termination {
        Termination {
            since: now,
            signalled: Vec::new(),
        }
    }

    /// `Some` once nothing carrying any of `tags` runs, or once init has
    /// waited long enough.
    pub fn step(&mut self, now: u64, tags: &[u32]) -> Option<Ended> {
        for _ in 0..PASSES {
            match self.pass(now, &Tasks::read(), tags) {
                Err(ended) => return Some(ended),
                Ok(false) => break,
                Ok(true) => {}
            }
        }
        None
    }

    /// [`step`](Self::step) once, as of one reading init takes for
    /// everything it ends.
    pub fn step_seen(&mut self, now: u64, tasks: &Tasks, tags: &[u32]) -> Option<Ended> {
        self.pass(now, tasks, tags).err()
    }

    /// Signal what carries `tags`; whether any of it was new.
    fn pass(&mut self, now: u64, tasks: &Tasks, tags: &[u32]) -> Result<bool, Ended> {
        let left = tasks.processes_of(tags);
        let waited = now.saturating_sub(self.since);
        if left.is_empty() {
            return Err(Ended::All);
        }
        if waited >= GIVE_UP_AFTER_MS {
            return Err(Ended::GaveUp(left.len()));
        }
        let fresh: Vec<u32> = left
            .iter()
            .copied()
            .filter(|pid| !self.signalled.contains(pid))
            .collect();
        if waited >= KILL_AFTER_MS {
            for &pid in &left {
                let _ = process::kill(pid, SIGKILL);
            }
        } else {
            for &pid in &fresh {
                let _ = process::kill(pid, SIGTERM);
            }
            // A stopped process acts on its SIGTERM only once continued; a
            // running one is sent nothing more, since every kill walks the
            // task table.
            for pid in tasks.stopped_of(tags) {
                let _ = process::kill(pid, SIGCONT);
            }
        }
        let signalled = !fresh.is_empty();
        self.signalled.extend(fresh);
        Ok(signalled)
    }
}
