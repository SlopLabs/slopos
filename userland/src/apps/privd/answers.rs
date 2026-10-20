//! What privd keeps of the answers it was given: the kept ones an action's
//! `keep` asks for, and each person's wrong passwords, which slow the next
//! check of theirs.

use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use slopos_priv_core::consent::{Kind, Secret};
use slopos_priv_core::policy::MAX_KEEP;

/// Kept answers at once; past them the oldest goes.
const KEPT_MAX: usize = 64;
/// The wait after a first wrong password, which each further one doubles,
/// as PAM's own delay on a failure is about two seconds.
const FIRST_DELAY: Duration = Duration::from_secs(2);
const LONGEST_DELAY: Duration = Duration::from_secs(60);

/// What a kept answer covers: one requesting session and the live task that
/// started the requester, one action as read from one place and asked one
/// way, and the exact command line and directory, since an answer for one
/// command says nothing of another.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Asked {
    pub uid: u32,
    /// The session's login tag and its eldest live task.
    pub session: (u32, u32),
    pub parent: u32,
    pub action: String,
    /// The extension and activation an extension's action was read from.
    pub origin: Option<(String, u64)>,
    pub kind: Kind,
    pub command: Vec<Vec<u8>>,
    pub cwd: String,
}

struct Answered {
    asked: Asked,
    /// The administrator whose password answered it, if one did.
    by: Option<String>,
    at: Instant,
}

#[derive(Default)]
pub struct Kept(Mutex<Vec<Answered>>);

impl Kept {
    /// Whether `asked` was answered within the last `keep` seconds, by
    /// nobody's password or by one of the `admins` who could answer now.
    pub fn holds(&self, asked: &Asked, keep: u32, admins: &[String]) -> bool {
        let Ok(mut kept) = self.0.lock() else {
            return false;
        };
        let longest = Duration::from_secs(u64::from(MAX_KEEP));
        kept.retain(|a| a.at.elapsed() < longest);
        let keep = Duration::from_secs(u64::from(keep));
        kept.iter().any(|a| {
            a.asked == *asked
                && a.at.elapsed() < keep
                && a.by.as_ref().is_none_or(|by| admins.contains(by))
        })
    }

    pub fn keep(&self, asked: Asked, by: Option<String>) {
        let Ok(mut kept) = self.0.lock() else {
            return;
        };
        kept.retain(|a| a.asked != asked);
        if kept.len() >= KEPT_MAX {
            kept.remove(0);
        }
        kept.push(Answered {
            asked,
            by,
            at: Instant::now(),
        });
    }

    pub fn clear(&self) {
        if let Ok(mut kept) = self.0.lock() {
            kept.clear();
        }
    }
}

struct Failed {
    name: String,
    count: u32,
    until: Instant,
}

/// Password checks, one at a time so Argon2id's memory is paid once, each
/// waiting out its person's last failures.
#[derive(Default)]
pub struct Checks(Mutex<Vec<Failed>>);

impl Checks {
    pub fn check(&self, name: &str, secret: &Secret) -> Result<bool, String> {
        let mut failed = loop {
            let failed = self
                .0
                .lock()
                .map_err(|_| "privd's password checks are poisoned")?;
            let wait = failed
                .iter()
                .find(|f| f.name == name)
                .map(|f| f.until.saturating_duration_since(Instant::now()))
                .unwrap_or_default();
            if wait.is_zero() {
                break failed;
            }
            drop(failed);
            thread::sleep(wait);
        };
        let right = crate::accounts::check(name, secret.bytes())?;
        let at = failed.iter().position(|f| f.name == name);
        match (right, at) {
            (true, Some(at)) => {
                failed.swap_remove(at);
            }
            (true, None) => {}
            (false, Some(at)) => {
                let f = &mut failed[at];
                f.count = f.count.saturating_add(1);
                f.until = Instant::now() + delay(f.count);
            }
            (false, None) => failed.push(Failed {
                name: name.to_owned(),
                count: 1,
                until: Instant::now() + delay(1),
            }),
        }
        Ok(right)
    }
}

fn delay(failures: u32) -> Duration {
    FIRST_DELAY
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(LONGEST_DELAY)
}
