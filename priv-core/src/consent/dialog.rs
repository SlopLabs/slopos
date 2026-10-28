use alloc::format;
use alloc::string::{String, ToString};

use super::{Answer, Ask, Kind, Secret, UNIDENTIFIED, password_for};

/// Input this soon after the question is first drawn, or after focus moves
/// or the dialog changes shape, was meant for what was there before.
pub const ARMING_MS: u64 = 750;

/// What has focus: the password field, which a question that takes a
/// password has, the toggle that shows and hides the details, or a button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Field,
    Details,
    Deny,
    Allow,
}

/// What a key does to the dialog: Tab and Shift+Tab move focus, Left and
/// Right move to the button on that side without wrapping, Enter presses what
/// has it, Escape denies; Backspace and what is typed edit the password, Up
/// and Down choose another administrator.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Move { back: bool },
    Arrow { right: bool },
    Press,
    Dismiss,
    Erase,
    Choose { next: bool },
    Text(char),
}

/// A question on screen. Focus starts on Deny, or on the password field of a
/// question that takes one, so a stray Enter refuses or does nothing; Allow
/// takes a click, or a move and then a press, and with a password field
/// something typed in it. Enter in the field of a destructive question moves
/// focus to Allow rather than pressing it. The details start shown. Times
/// are milliseconds on any clock that only goes forward.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dialog {
    ask: Ask,
    focus: Focus,
    armed_at: Option<u64>,
    typed: Secret,
    admin: usize,
    expanded: bool,
    /// The primary button went down while the dialog was armed and has not
    /// waited since.
    down: bool,
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
            expanded: true,
            down: false,
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

    pub fn expanded(&self) -> bool {
        self.expanded
    }

    /// The administrator answering a [`Kind::Admin`] question, by its place
    /// among those listed.
    pub fn admin(&self) -> Option<usize> {
        (self.ask.kind == Kind::Admin).then_some(self.admin)
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
        self.down = false;
    }

    /// The primary button went down at `now`: whether the dialog takes the
    /// press. Only a press it took can [`click`](Self::click).
    pub fn pointer_down(&mut self, now: u64) -> bool {
        self.down = self.armed(now);
        self.down
    }

    /// The order Tab moves focus in, which is the order they are drawn in.
    fn stops(&self) -> &'static [Focus] {
        match self.ask.kind.takes_secret() {
            true => &[Focus::Field, Focus::Details, Focus::Deny, Focus::Allow],
            false => &[Focus::Details, Focus::Deny, Focus::Allow],
        }
    }

    pub fn key(&mut self, key: Key, now: u64) -> Option<Answer> {
        if !self.armed(now) {
            return None;
        }
        let field = self.ask.kind.takes_secret();
        match key {
            Key::Move { back } => {
                let stops = self.stops();
                let at = stops.iter().position(|&f| f == self.focus).unwrap_or(0);
                let step = if back { stops.len() - 1 } else { 1 };
                self.focus = stops[(at + step) % stops.len()];
                self.rearm(now);
                None
            }
            Key::Arrow { right } => {
                let to = match right {
                    true => Focus::Allow,
                    false => Focus::Deny,
                };
                if matches!(self.focus, Focus::Deny | Focus::Allow) && self.focus != to {
                    self.focus = to;
                    self.rearm(now);
                }
                None
            }
            Key::Press if self.focus == Focus::Details => {
                self.toggle(now);
                None
            }
            Key::Press if self.focus == Focus::Field && self.ask.destructive => {
                if !self.typed.is_empty() {
                    self.focus = Focus::Allow;
                    self.rearm(now);
                }
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
                let admin = match next {
                    true => (self.admin + 1) % count,
                    false => (self.admin + count - 1) % count,
                };
                self.choose(admin, now);
                None
            }
            Key::Text(_) | Key::Erase | Key::Choose { .. } => None,
        }
    }

    pub fn click(&mut self, target: Focus, now: u64) -> Option<Answer> {
        if !self.down || !self.armed(now) {
            return None;
        }
        match target {
            Focus::Field if self.ask.kind.takes_secret() => {
                self.focus = Focus::Field;
                None
            }
            Focus::Field => None,
            Focus::Details => {
                self.focus = Focus::Details;
                self.toggle(now);
                None
            }
            button => self.press(button),
        }
    }

    pub fn pick(&mut self, admin: usize, now: u64) {
        if self.down
            && self.armed(now)
            && self.ask.kind == Kind::Admin
            && admin < self.ask.admins.len()
        {
            self.choose(admin, now);
        }
    }

    fn choose(&mut self, admin: usize, now: u64) {
        if admin != self.admin {
            self.typed.clear();
        }
        self.admin = admin;
        self.focus = Focus::Field;
        self.rearm(now);
    }

    /// The dialog moved on the screen at `now`, so what was under the
    /// pointer is not what is there now: input waits again.
    pub fn moved(&mut self, now: u64) {
        if self.armed_at.is_some() {
            self.rearm(now);
        }
    }

    /// The details shown or hidden move the buttons, so input waits again.
    fn toggle(&mut self, now: u64) {
        self.expanded = !self.expanded;
        self.rearm(now);
    }

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

    /// Who asks, as the kernel attested them and the accounts name them.
    pub fn requester(&self) -> String {
        let program = self.ask.program.as_deref().unwrap_or(UNIDENTIFIED);
        let who = match (&self.ask.person, self.ask.uid) {
            (Some(name), _) => name.clone(),
            (None, 0) => "root".to_string(),
            (None, uid) => format!("uid {uid}"),
        };
        format!("{program} as {who} (pid {})", self.ask.pid)
    }

    pub fn field_label(&self) -> Option<String> {
        let whose = match self.ask.kind {
            Kind::Confirm => return None,
            Kind::Password => self.ask.person.as_deref()?,
            Kind::Admin => self.ask.admins.get(self.admin)?,
        };
        Some(password_for(whose))
    }
}
