//! The desktop session: the compositor and the terminal, and whatever they
//! start, all carrying the desktop's login tag, by which the session is ended.

use slopos_abi::signal::{SIGCONT, SIGKILL, SIGTERM};

use super::service::Child;
use crate::init_control::{self, DESKTOP};
use crate::syscall::{OwnedFd, process};

/// How long the session's processes have to end on `SIGTERM` before
/// `SIGKILL`, and how long after that init stops waiting for them.
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
    since: u64,
    signalled: Vec<u32>,
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
            since: now,
            signalled: Vec::new(),
            compositor,
            then,
            waiting: Vec::new(),
        }
    }

    /// Signal what is left of the session; `Some` once nothing carrying its
    /// tag runs, or once init has waited long enough for it.
    pub fn step(&mut self, now: u64) -> Option<Ended> {
        let waited = now.saturating_sub(self.since);
        for _ in 0..PASSES {
            let left = init_control::processes(DESKTOP);
            if left.is_empty() {
                return Some(Ended::All);
            }
            if waited >= GIVE_UP_AFTER_MS {
                eprintln!(
                    "init: {} processes of the desktop session would not end: {left:?}",
                    left.len()
                );
                return Some(Ended::GaveUp(left.len()));
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
                    // A stopped process acts on its SIGTERM only once continued.
                    let _ = process::kill(pid, SIGTERM);
                    let _ = process::kill(pid, SIGCONT);
                }
            }
            if fresh.is_empty() {
                break;
            }
            self.signalled.extend(fresh);
        }
        None
    }
}
