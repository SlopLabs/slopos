use alloc::format;
use alloc::string::{String, ToString};

use super::{Answer, Ask, Kind, Secret};

/// Input this soon after the question is first drawn, or after focus moves,
/// was meant for what was there before.
pub const ARMING_MS: u64 = 750;

/// What has focus: the password field, which a question that takes a
/// password has, or a button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Field,
    Deny,
    Allow,
}

/// What a key does to the dialog: Tab moves focus, and Left and Right
/// between the buttons, Enter presses what has it, Escape denies; Backspace
/// and what is typed edit the password, Up and Down choose another
/// administrator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Move,
    Arrow,
    Press,
    Dismiss,
    Erase,
    Choose { next: bool },
    Text(char),
}

/// A question on screen. Focus starts on Deny, or on the password field of a
/// question that takes one, so a stray Enter refuses or does nothing; Allow
/// takes a click, or a move and then a press, and with a password field
/// something typed in it. Times are milliseconds on any clock that only goes
/// forward.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dialog {
    ask: Ask,
    focus: Focus,
    armed_at: Option<u64>,
    typed: Secret,
    /// Which of the administrators listed answers.
    admin: usize,
}

impl Dialog {
    pub fn new(ask: Ask) -> Self {
        let focus = match ask.kind.takes_secret() {
            true => Focus::Field,
            false => Focus::Deny,
        };
        Self {
            ask,
            focus,
            armed_at: None,
            typed: Secret::default(),
            admin: 0,
        }
    }

    pub fn ask(&self) -> &Ask {
        &self.ask
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn typed_chars(&self) -> usize {
        self.typed.chars()
    }

    /// The administrator answering a [`Kind::Admin`] question, and how many
    /// there are to choose from.
    pub fn admin(&self) -> Option<(&str, usize)> {
        let name = self.ask.admins.get(self.admin)?;
        Some((name, self.ask.admins.len()))
    }

    /// The dialog is on screen as of `now`; only the first call counts, and
    /// whether this was it.
    pub fn drawn(&mut self, now: u64) -> bool {
        let first = self.armed_at.is_none();
        self.armed_at.get_or_insert(now + ARMING_MS);
        first
    }

    fn armed(&self, now: u64) -> bool {
        self.armed_at.is_some_and(|at| now >= at)
    }

    fn rearm(&mut self, now: u64) {
        self.armed_at = Some(now + ARMING_MS);
    }

    /// The answer a key pressed at `now` gives, once it gives one.
    pub fn key(&mut self, key: Key, now: u64) -> Option<Answer> {
        if !self.armed(now) {
            return None;
        }
        let field = self.ask.kind.takes_secret();
        match key {
            Key::Move => {
                self.focus = match (self.focus, field) {
                    (Focus::Field, _) => Focus::Deny,
                    (Focus::Deny, _) => Focus::Allow,
                    (Focus::Allow, true) => Focus::Field,
                    (Focus::Allow, false) => Focus::Deny,
                };
                self.rearm(now);
                None
            }
            Key::Arrow if self.focus == Focus::Field => None,
            Key::Arrow => {
                self.focus = match self.focus {
                    Focus::Deny => Focus::Allow,
                    _ => Focus::Deny,
                };
                self.rearm(now);
                None
            }
            Key::Press => self.press(self.focus),
            Key::Dismiss => Some(Answer::deny(self.ask.id)),
            Key::Text(ch) if field && !ch.is_control() => {
                self.focus = Focus::Field;
                self.typed.push(ch);
                None
            }
            Key::Erase if field => {
                self.typed.pop();
                None
            }
            Key::Choose { next } if self.ask.admins.len() > 1 => {
                let count = self.ask.admins.len();
                self.admin = match next {
                    true => (self.admin + 1) % count,
                    false => (self.admin + count - 1) % count,
                };
                self.typed.clear();
                self.focus = Focus::Field;
                self.rearm(now);
                None
            }
            Key::Text(_) | Key::Erase | Key::Choose { .. } => None,
        }
    }

    /// The answer a click on `target` at `now` gives; one on the field
    /// gives it focus.
    pub fn click(&mut self, target: Focus, now: u64) -> Option<Answer> {
        if !self.armed(now) {
            return None;
        }
        if target == Focus::Field {
            self.focus = Focus::Field;
            return None;
        }
        self.press(target)
    }

    /// Deny denies; Allow, or Enter in the field, allows once whatever the
    /// question takes is typed.
    fn press(&mut self, on: Focus) -> Option<Answer> {
        let id = self.ask.id;
        if on == Focus::Deny {
            return Some(Answer::deny(id));
        }
        if !self.ask.kind.takes_secret() {
            return Some(Answer {
                id,
                allow: true,
                secret: None,
                admin: None,
            });
        }
        if self.typed.is_empty() {
            self.focus = Focus::Field;
            return None;
        }
        let admin = match self.ask.kind {
            Kind::Admin => Some(self.ask.admins.get(self.admin)?.clone()),
            _ => None,
        };
        Some(Answer {
            id,
            allow: true,
            secret: Some(core::mem::take(&mut self.typed)),
            admin,
        })
    }

    /// Who asks, as the kernel attested it and the accounts name them.
    pub fn requester(&self) -> String {
        let program = self
            .ask
            .program
            .as_deref()
            .unwrap_or("a program from outside the system");
        let who = match (&self.ask.person, self.ask.uid) {
            (Some(name), _) => name.clone(),
            (None, 0) => "root".to_string(),
            (None, uid) => format!("uid {uid}"),
        };
        format!("{program}, as {who}, pid {}", self.ask.pid)
    }

    /// What the person must do to allow it, when more than a click.
    pub fn prompt(&self) -> Option<String> {
        match self.ask.kind {
            Kind::Confirm => None,
            Kind::Password => Some(format!(
                "{}'s password allows it",
                self.ask.person.as_deref().unwrap_or("the requester")
            )),
            Kind::Admin => Some("An administrator's password allows it".to_string()),
        }
    }
}
