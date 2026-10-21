//! The sessions init keeps, as it tells privd of them over the socket pair it
//! made for that, one frame holding the whole table each time it changes:
//!
//! ```text
//! init SESSIONS{session: "<tag> <uid> <class> <front> <locked> <link|->"..}
//! ```
//!
//! A session is a person's login, or the login screen's: its login tag, which
//! every process of it carries and no other process can, its uid, its class,
//! whether its desktop is the one in front and whether it is locked, and the
//! consent link its compositor holds.

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;

use slopos_remote_core::{Fields, frame};

use crate::policy::Reach;
use crate::wire::{Malformed, known};

pub mod kind {
    pub const SESSIONS: u8 = 0x30;
}

/// The consent links init makes at boot, one per desktop or login screen
/// running at once.
pub const LINKS: usize = 4;
/// The most sessions a table holds.
pub const MAX_SESSIONS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class {
    /// The login screen.
    Greeter,
    /// A person's desktop.
    User,
    /// A person's shell at a terminal.
    Console,
    /// What runs for a person with neither: remoted.
    Background,
}

impl Class {
    pub const ALL: [Class; 4] = [
        Class::Greeter,
        Class::User,
        Class::Console,
        Class::Background,
    ];

    pub fn word(self) -> &'static str {
        match self {
            Class::Greeter => "greeter",
            Class::User => "user",
            Class::Console => "console",
            Class::Background => "background",
        }
    }

    pub fn parse(word: &str) -> Option<Class> {
        Class::ALL.into_iter().find(|class| class.word() == word)
    }

    /// Whether a person is logged in by it, as `user alone` counts them.
    pub fn logs_in(self) -> bool {
        matches!(self, Class::User | Class::Console)
    }

    pub fn graphical(self) -> bool {
        matches!(self, Class::Greeter | Class::User)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Session {
    pub tag: u32,
    pub uid: u32,
    pub class: Class,
    pub front: bool,
    pub locked: bool,
    pub link: Option<u8>,
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Table {
    pub sessions: Vec<Session>,
}

impl Table {
    pub fn frame(&self) -> Vec<u8> {
        let mut fields = Fields::new();
        for s in &self.sessions {
            let link = s.link.map_or("-".to_string(), |link| link.to_string());
            fields.push(
                "session",
                format!(
                    "{} {} {} {} {} {link}",
                    s.tag,
                    s.uid,
                    s.class.word(),
                    u8::from(s.front),
                    u8::from(s.locked)
                ),
            );
        }
        frame(kind::SESSIONS, &fields.encode())
    }

    pub fn decode(payload: &[u8]) -> Result<Table, Malformed> {
        let fields = known(payload, &["session"])?;
        let mut sessions = Vec::new();
        for value in fields.all("session") {
            let text = core::str::from_utf8(value).map_err(|_| Malformed)?;
            sessions.push(session(text).ok_or(Malformed)?);
        }
        let table = Table { sessions };
        let fronts = table.sessions.iter().filter(|s| s.front).count();
        let tags_once = table
            .sessions
            .iter()
            .enumerate()
            .all(|(i, s)| table.sessions[..i].iter().all(|t| t.tag != s.tag));
        let links_once =
            table.sessions.iter().enumerate().all(|(i, s)| {
                s.link.is_none() || table.sessions[..i].iter().all(|t| t.link != s.link)
            });
        if table.sessions.len() > MAX_SESSIONS || fronts > 1 || !tags_once || !links_once {
            return Err(Malformed);
        }
        Ok(table)
    }

    pub fn session(&self, tag: u32) -> Option<&Session> {
        self.sessions.iter().find(|s| s.tag == tag)
    }

    /// Everyone logged in at a desktop or a terminal.
    pub fn logged_in(&self) -> Vec<u32> {
        self.sessions
            .iter()
            .filter(|s| s.class.logs_in())
            .map(|s| s.uid)
            .collect()
    }

    /// Where a requester running as `uid` in the session `tag` names can be
    /// asked, and the consent link that asks it on screen: its own session's
    /// desktop while that is in front and unlocked, or, when the requester
    /// can ask at its terminal, its own console session. Nothing else is
    /// asked, so a session behind is never asked, nor one locked.
    pub fn reach(&self, tag: u32, uid: u32, terminal: bool) -> (Reach, Option<u8>) {
        match self.session(tag) {
            Some(s) if s.class.graphical() && s.front && !s.locked && s.link.is_some() => (
                Reach::Screen {
                    theirs: s.uid == uid,
                },
                s.link,
            ),
            Some(s) if s.class == Class::Console && s.uid == uid && terminal => {
                (Reach::Terminal, None)
            }
            _ => (Reach::Nowhere, None),
        }
    }
}

/// A decimal number in its one spelling.
fn number<T: core::str::FromStr>(w: &str) -> Option<T> {
    let canonical =
        !w.is_empty() && w.bytes().all(|b| b.is_ascii_digit()) && (w == "0" || !w.starts_with('0'));
    canonical.then(|| w.parse().ok()).flatten()
}

fn session(text: &str) -> Option<Session> {
    let mut words = text.split(' ');
    let mut word = || words.next();
    let flag = |w: &str| match w {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    };
    let session = Session {
        tag: number(word()?)?,
        uid: number(word()?)?,
        class: Class::parse(word()?)?,
        front: flag(word()?)?,
        locked: flag(word()?)?,
        link: match word()? {
            "-" => None,
            link => Some(number(link).filter(|&n: &u8| usize::from(n) < LINKS)?),
        },
    };
    let sound = word().is_none() && (!session.front || session.class.graphical());
    sound.then_some(session)
}

#[cfg(test)]
mod tests;
