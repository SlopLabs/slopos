//! The login screen and the lock screen, drawn opaque over every surface.
//! While either is up every key is its own and no pointer press reaches a
//! client, so what is typed there reaches the mechanism privd runs alone:
//! `org.slopos.login` lets a person in from the login screen, and
//! `org.slopos.session.unlock` answers whether a locked desktop's own person
//! typed their password. Ctrl+Alt+Delete, the secure attention key, is read
//! here and nowhere else: on a desktop it locks it, so the screen that asks
//! for a password after it is this one.

use std::string::String;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, PoisonError};

use slopos_abi::InputEvent;
use slopos_abi::input::keycode::{
    KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_F2, KEY_KP_ENTER, KEY_LEFTALT,
    KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_TAB, KEY_UP,
};
use slopos_abi::input::{MODIFIER_ALT, MODIFIER_CTRL};
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_priv_core::consent::Secret;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::init_control::{self, Command};
use crate::theme::*;

const CARD_W: i32 = 420;
const PAD: i32 = 16;
const GAP: i32 = 8;
const FIELD_H: i32 = 28;
const RADIUS: i32 = 12;
const BORDER: i32 = 1;
/// More keys than a hand holds down at once.
const TAKEN_MAX: usize = 16;
const NAME_MAX: usize = slopos_accounts_core::NAME_MAX;

const LOGIN: &str = "org.slopos.login";
const UNLOCK: &str = "org.slopos.session.unlock";

/// How many times the desktop has locked or unlocked: a lock still being
/// retold stops once the desktop has unlocked since.
static LOCKS: AtomicU64 = AtomicU64::new(0);
/// Held while init is told either, so a lock retold never reaches init after
/// the unlock that followed it.
static TELLING: Mutex<()> = Mutex::new(());

pub enum Role {
    /// The login screen, which is always up and lists the people.
    Greeter,
    /// A person's desktop, whose lock screen names them.
    Desktop { name: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Name,
    Password,
}

pub struct Gate {
    role: Role,
    shown: bool,
    name: String,
    /// Whom the login screen lists, by name.
    people: Vec<String>,
    secret: Secret,
    field: Field,
    status: String,
    /// What the mechanism privd ran answered, while it runs.
    asking: Option<Receiver<Result<(), String>>>,
    dirty: bool,
    /// Keys pressed into the screen, whose releases no client may see.
    taken: Vec<u16>,
    ctrl: bool,
    alt: bool,
}

impl Gate {
    pub fn new(role: Role) -> Gate {
        let shown = matches!(role, Role::Greeter);
        Gate {
            people: if shown { people() } else { Vec::new() },
            role,
            shown,
            name: String::new(),
            secret: Secret::default(),
            field: Field::Name,
            status: String::new(),
            asking: None,
            dirty: true,
            taken: Vec::new(),
            ctrl: false,
            alt: false,
        }
    }

    pub fn greeter(&self) -> bool {
        matches!(self.role, Role::Greeter)
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Lock the desktop and tell init it is locked.
    pub fn lock(&mut self) {
        if self.greeter() || self.shown {
            return;
        }
        self.shown = true;
        self.reset();
        tell_locked();
    }

    /// Unlocked by init: its person proved themselves at the login screen.
    pub fn unlocked(&mut self) {
        if !self.greeter() {
            tell_unlocked();
            self.shown = false;
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.secret.clear();
        self.name.clear();
        if self.greeter() {
            self.people = people();
        }
        self.field = match self.role {
            Role::Greeter => Field::Name,
            Role::Desktop { .. } => Field::Password,
        };
        self.status.clear();
        self.dirty = true;
    }

    /// Settle what the mechanism answered.
    pub fn poll(&mut self) {
        let Some(asking) = &self.asking else {
            return;
        };
        let answered = match asking.try_recv() {
            Ok(answered) => answered,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the request was lost".into()),
        };
        self.asking = None;
        self.secret.clear();
        self.dirty = true;
        match (answered, &self.role) {
            (Ok(()), Role::Greeter) => self.status = format!("Starting {}'s desktop", self.name),
            (Ok(()), Role::Desktop { .. }) => {
                self.shown = false;
                self.reset();
            }
            (Err(why), _) => self.status = why,
        }
    }

    /// Whether a key press is the screen's, which then takes it: the secure
    /// attention key always, any key while it is up, and a repeat of a key
    /// pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let keycode = event.key_keycode();
        self.track(keycode, true);
        let held = self.taken.contains(&keycode);
        let attention = keycode == KEY_DELETE && self.chord(event.key_modifiers());
        if attention {
            self.take(keycode);
            match self.greeter() {
                true => self.reset(),
                false => self.lock(),
            }
            return true;
        }
        if self.shown || (held && event.key_is_repeat()) {
            self.take(keycode);
            self.key(event);
            return true;
        }
        if held {
            self.taken.retain(|&k| k != keycode);
        }
        false
    }

    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.track(keycode, false);
        let Some(at) = self.taken.iter().position(|&k| k == keycode) else {
            return false;
        };
        self.taken.remove(at);
        true
    }

    /// Ctrl and Alt as the modifiers say or as their keys were seen.
    fn chord(&self, modifiers: u8) -> bool {
        let both = MODIFIER_CTRL | MODIFIER_ALT;
        modifiers & both == both || (self.ctrl && self.alt)
    }

    fn track(&mut self, keycode: u16, pressed: bool) {
        match keycode {
            KEY_LEFTCTRL | KEY_RIGHTCTRL => self.ctrl = pressed,
            KEY_LEFTALT | KEY_RIGHTALT => self.alt = pressed,
            _ => {}
        }
    }

    fn take(&mut self, keycode: u16) {
        if !self.taken.contains(&keycode) {
            if self.taken.len() >= TAKEN_MAX {
                self.taken.remove(0);
            }
            self.taken.push(keycode);
        }
    }

    fn key(&mut self, event: &InputEvent) {
        if self.asking.is_some() {
            return;
        }
        let keycode = event.key_keycode();
        if event.key_is_repeat() && keycode != KEY_BACKSPACE {
            return;
        }
        self.dirty = true;
        match keycode {
            KEY_TAB if self.greeter() => {
                self.field = match self.field {
                    Field::Name => Field::Password,
                    Field::Password => Field::Name,
                };
            }
            KEY_ENTER | KEY_KP_ENTER if self.field == Field::Name => {
                self.field = Field::Password;
            }
            KEY_ENTER | KEY_KP_ENTER => self.submit(),
            KEY_BACKSPACE => match self.field {
                Field::Name => {
                    self.name.pop();
                }
                Field::Password => self.secret.pop(),
            },
            KEY_ESC => self.secret.clear(),
            KEY_UP | KEY_DOWN if self.greeter() && !self.people.is_empty() => {
                let at = self.people.iter().position(|p| *p == self.name);
                let n = self.people.len();
                let next = match (at, keycode == KEY_DOWN) {
                    (None, _) => 0,
                    (Some(i), true) => (i + 1) % n,
                    (Some(i), false) => (i + n - 1) % n,
                };
                self.name = self.people[next].clone();
                self.field = Field::Name;
            }
            KEY_F2 if !self.greeter() => tell_init(Command::Greeter),
            _ => {
                let Some(ch) = char::from_u32(event.key_codepoint()).filter(|ch| !ch.is_control())
                else {
                    return;
                };
                match self.field {
                    Field::Name if self.name.len() < NAME_MAX => self.name.push(ch),
                    Field::Name => {}
                    Field::Password => {
                        self.secret.push(ch);
                    }
                }
            }
        }
    }

    /// Hand the password to the mechanism on a thread of its own, which
    /// answers once privd has.
    fn submit(&mut self) {
        let (action, args) = match &self.role {
            Role::Greeter if self.name.is_empty() => {
                self.field = Field::Name;
                return;
            }
            Role::Greeter => (
                LOGIN,
                vec!["login".to_owned(), "desktop".into(), self.name.clone()],
            ),
            Role::Desktop { .. } => (UNLOCK, vec!["unlock".to_owned()]),
        };
        let mut input = self.secret.bytes().to_vec();
        input.push(b'\n');
        self.secret.clear();
        let unlock = !self.greeter();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("compositor-gate".into())
            .spawn(move || {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let answered = asked(action, &args, &input);
                slopos_tls_core::ct::wipe(&mut input);
                let answered = answered.and_then(|()| match unlock {
                    true => {
                        let _telling = TELLING.lock().unwrap_or_else(PoisonError::into_inner);
                        LOCKS.fetch_add(1, Ordering::SeqCst);
                        init_control::ask(Command::Locked(false))
                    }
                    false => Ok(()),
                });
                let _ = tx.send(answered);
            });
        match spawned {
            Ok(_) => {
                self.status = "Checking".into();
                self.asking = Some(rx);
            }
            Err(_) => self.status = "Nothing can check the password now".into(),
        }
    }

    pub fn draw(&self, buf: &mut DrawBuffer, screen_w: u32, screen_h: u32, clip: &DamageRect) {
        if !self.shown {
            return;
        }
        rounded_rect_filled(
            buf,
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
            0,
            DESKTOP_BG,
        );
        let line = gfx::cell_height();
        let w = CARD_W.min(screen_w as i32 - 2 * PAD);
        let (title, hint) = match &self.role {
            Role::Greeter => (
                format!("Log in: {}", self.people.join(", ")),
                "Up, Down: choose  Tab: next field  Enter: log in".to_owned(),
            ),
            Role::Desktop { name } => (
                format!("{name}'s desktop is locked"),
                "Enter: unlock  F2: switch person".to_owned(),
            ),
        };
        let fields: &[Field] = match self.role {
            Role::Greeter => &[Field::Name, Field::Password],
            Role::Desktop { .. } => &[Field::Password],
        };
        let h = PAD
            + line
            + GAP
            + fields.len() as i32 * (line + FIELD_H + GAP)
            + 2 * (line + GAP)
            + PAD;
        let x = (screen_w as i32 - w) / 2;
        let mut y = ((screen_h as i32 - h) / 2).max(0);
        rounded_rect_filled(buf, x, y, w, h, RADIUS, SHELF_SEPARATOR);
        rounded_rect_filled(
            buf,
            x + BORDER,
            y + BORDER,
            w - 2 * BORDER,
            h - 2 * BORDER,
            RADIUS - BORDER,
            PANEL_BG_OPAQUE,
        );
        let text = |buf: &mut DrawBuffer, y: i32, s: &str, colour| {
            gfx::draw_str_clipped(buf, x + PAD, y, s, colour, PANEL_BG_OPAQUE, clip);
        };
        y += PAD;
        text(buf, y, &title, TEXT_PRIMARY);
        y += line + GAP;
        for &field in fields {
            let label = match field {
                Field::Name => "Name",
                Field::Password => "Password",
            };
            text(buf, y, label, TEXT_SECONDARY);
            y += line;
            let edge = match self.field == field {
                true => SIGNAL_EXPAND,
                false => SHELF_SEPARATOR,
            };
            let fw = w - 2 * PAD;
            rounded_rect_filled(buf, x + PAD, y, fw, FIELD_H, RADIUS / 2, edge);
            rounded_rect_filled(
                buf,
                x + PAD + BORDER,
                y + BORDER,
                fw - 2 * BORDER,
                FIELD_H - 2 * BORDER,
                RADIUS / 2 - BORDER,
                PANEL_BG_OPAQUE,
            );
            let shown = match field {
                Field::Name => self.name.clone(),
                Field::Password => {
                    let room = (fw - 2 * PAD) / gfx::font::string_width("*").max(1);
                    "*".repeat(self.secret.chars().min(room.max(0) as usize))
                }
            };
            gfx::draw_str_clipped(
                buf,
                x + 2 * PAD,
                y + (FIELD_H - line) / 2,
                &shown,
                TEXT_PRIMARY,
                PANEL_BG_OPAQUE,
                clip,
            );
            y += FIELD_H + GAP;
        }
        text(buf, y, &self.status, TEXT_PRIMARY);
        y += line + GAP;
        text(buf, y, &hint, TEXT_SECONDARY);
    }
}

/// What the mechanism answered: run, or why not.
fn asked(action: &str, args: &[&str], input: &[u8]) -> Result<(), String> {
    let null = crate::syscall::fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR)
        .map_err(|e| format!("/dev/null: {e}"))?;
    match crate::privd::run_feeding(action, args, input, null.raw()) {
        Ok(0) => Ok(()),
        Ok(_) => Err("The password is wrong".into()),
        Err(why) => Err(why),
    }
}

/// Tell init on a thread of its own, which init answers once it has acted.
fn tell_init(command: Command) {
    let _ = std::thread::Builder::new()
        .name("compositor-init".into())
        .spawn(move || {
            if let Err(why) = init_control::ask(command) {
                eprintln!("compositor: init: {why}");
            }
        });
}

/// Stop telling init of a lock and tell it the desktop is unlocked, after a
/// lock already on its way: init unlocked it when it brought it in front.
fn tell_unlocked() {
    LOCKS.fetch_add(1, Ordering::SeqCst);
    let _ = std::thread::Builder::new()
        .name("compositor-init".into())
        .spawn(|| {
            let _telling = TELLING.lock().unwrap_or_else(PoisonError::into_inner);
            if let Err(why) = init_control::ask(Command::Locked(false)) {
                eprintln!("compositor: init: {why}");
            }
        });
}

/// Tell init the desktop locked, again a second later while init could not
/// hear it and the desktop has not unlocked since: a restart brings the
/// desktop back as init last heard it was.
fn tell_locked() {
    const TRIES: u32 = 60;
    let lock = LOCKS.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = std::thread::Builder::new()
        .name("compositor-init".into())
        .spawn(move || {
            for _ in 0..TRIES {
                let telling = TELLING.lock().unwrap_or_else(PoisonError::into_inner);
                if LOCKS.load(Ordering::SeqCst) != lock {
                    return;
                }
                match init_control::ask(Command::Locked(true)) {
                    Ok(()) => return,
                    Err(why) if why.starts_with("refused:") => {
                        eprintln!("compositor: init: {why}");
                        return;
                    }
                    Err(_) => {}
                }
                drop(telling);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            eprintln!("compositor: init did not hear that the desktop locked");
        });
}

/// The people the user database lists, as anyone reads it.
fn people() -> Vec<String> {
    use slopos_slibc_core::passwd::{BASE_PASSWD, Passwd, lines};
    let text = std::fs::read("/etc/passwd").unwrap_or_else(|_| BASE_PASSWD.as_bytes().to_vec());
    lines(&text)
        .filter_map(Passwd::parse)
        .filter(|p| slopos_abi::fs::PERSON_UIDS.contains(&p.uid))
        .map(|p| String::from_utf8_lossy(p.name).into_owned())
        .collect()
}
