//! The sessions init lists under `/run/sessions`, as a test waits on them.
//! Each test binary includes it and uses what it needs.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use slopos_userland::init_control::{self, Command};
use slopos_userland::sessions::{self, Class, Entry};

/// Long enough for a session to end and a compositor to start under TCG,
/// and short of the two silent minutes after which the runner gives up.
pub const SESSION_DEADLINE: Duration = Duration::from_secs(90);

pub fn until(within: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < within {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

/// The session in front once one is, within `within`.
pub fn front_within(within: Duration) -> Option<Entry> {
    let mut front = None;
    until(within, || {
        front = sessions::read().into_iter().find(|s| s.active);
        front.is_some()
    });
    front
}

/// The desktop of `uid` once it is in front, within `within`.
pub fn in_front(uid: u32, within: Duration) -> Option<Entry> {
    let mut found = None;
    until(within, || {
        found = sessions::read()
            .into_iter()
            .find(|s| s.active && s.uid == uid && s.class == Class::User);
        found.is_some()
    });
    found
}

/// The login tag of the desktop in front, if one is.
pub fn front_tag() -> Option<u32> {
    sessions::read()
        .into_iter()
        .find(|s| s.active && s.class == Class::User)
        .map(|s| s.tag)
}

/// The login tag of the desktop in front, starting the boot's own first
/// when none is.
pub fn front_desktop() -> Option<u32> {
    if let Some(front) = sessions::read()
        .into_iter()
        .find(|s| s.active && s.class == Class::User)
    {
        return Some(front.tag);
    }
    init_control::ask(Command::RestartSession).ok()?;
    let front = front_within(SESSION_DEADLINE)?;
    (front.class == Class::User).then_some(front.tag)
}

/// The session `tag` names as init lists it now.
pub fn entry(tag: u32) -> Option<Entry> {
    sessions::read().into_iter().find(|s| s.tag == tag)
}
