use alloc::format;
use alloc::string::{String, ToString};

use slopos_abi::fs::SESSION_UID;

use super::Ask;

/// Input this soon after the question is first drawn, or after focus moves,
/// was meant for what was there before.
pub const ARMING_MS: u64 = 750;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Deny,
    Allow,
}

/// What a key does to the dialog: Tab, Left and Right move focus, Enter
/// presses the focused button, Escape denies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Move,
    Press,
    Dismiss,
}

/// A question on screen. Focus starts on Deny, so a stray Enter refuses;
/// Allow takes a click, or a move and then a press. Times are milliseconds
/// on any clock that only goes forward.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dialog {
    ask: Ask,
    focus: Button,
    armed_at: Option<u64>,
}

impl Dialog {
    pub fn new(ask: Ask) -> Self {
        Self {
            ask,
            focus: Button::Deny,
            armed_at: None,
        }
    }

    pub fn ask(&self) -> &Ask {
        &self.ask
    }

    pub fn focus(&self) -> Button {
        self.focus
    }

    /// The dialog is on screen as of `now`; only the first call counts.
    pub fn drawn(&mut self, now: u64) {
        self.armed_at.get_or_insert(now + ARMING_MS);
    }

    fn armed(&self, now: u64) -> bool {
        self.armed_at.is_some_and(|at| now >= at)
    }

    /// The answer a key pressed at `now` gives, once it gives one.
    pub fn key(&mut self, key: Key, now: u64) -> Option<bool> {
        if !self.armed(now) {
            return None;
        }
        match key {
            Key::Move => {
                self.focus = match self.focus {
                    Button::Deny => Button::Allow,
                    Button::Allow => Button::Deny,
                };
                self.armed_at = Some(now + ARMING_MS);
                None
            }
            Key::Press => Some(self.focus == Button::Allow),
            Key::Dismiss => Some(false),
        }
    }

    /// The answer a click on `button` at `now` gives.
    pub fn click(&self, button: Button, now: u64) -> Option<bool> {
        self.armed(now).then_some(button == Button::Allow)
    }

    /// Who asks, as the kernel attested it.
    pub fn requester(&self) -> String {
        let program = self
            .ask
            .program
            .as_deref()
            .unwrap_or("a program from outside the system");
        let who = match self.ask.uid {
            0 => "root".to_string(),
            SESSION_UID => "user".to_string(),
            uid => format!("uid {uid}"),
        };
        format!("{program}, as {who}, pid {}", self.ask.pid)
    }
}
