//! The login and lock screens, in place of every surface, and the secure
//! attention key, read here alone, so the screen that asks after it is this.

use std::string::String;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use slopos_abi::InputEvent;
use slopos_abi::input::keycode::{
    KEY_DELETE, KEY_LEFTALT, KEY_LEFTCTRL, KEY_RIGHTALT, KEY_RIGHTCTRL,
};
use slopos_abi::input::{MODIFIER_ALT, MODIFIER_CTRL};
use slopos_appkit::Rect;
use slopos_appkit::traits::Role;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::gate::{self as model, Asks, Checked, Focus, Target};
use slopos_chrome_core::key::Key;

use super::overlay::Overlay;
use super::views;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::init_control::{self, Command};
use crate::syscall::core as sys_core;

/// How long the login screen says a desktop is starting before it starts
/// over, should that desktop never come in front.
const STARTING_FOR: Duration = Duration::from_secs(30);

const LOGIN_ACTION: &str = "org.slopos.login";
const UNLOCK_ACTION: &str = "org.slopos.session.unlock";

/// How many times the desktop has locked or unlocked: a lock still being
/// retold stops once the desktop has unlocked since.
static LOCKS: AtomicU64 = AtomicU64::new(0);
/// Held while init is told either, so a lock retold never reaches init after
/// the unlock that followed it.
static TELLING: Mutex<()> = Mutex::new(());

pub enum Kind {
    /// The login screen, which is always up and lists the people.
    Greeter,
    /// A person's desktop, whose lock screen names them.
    Desktop { name: String },
}

pub struct Gate {
    model: model::Gate,
    shown: bool,
    overlay: Overlay,
    screen: Rect,
    minute: Option<i64>,
    /// What the mechanism privd ran answered, while it runs.
    asking: Option<Receiver<Checked>>,
    /// When a login passed, while that person's desktop starts.
    started: Option<Instant>,
    ctrl: bool,
    alt: bool,
}

impl Gate {
    pub fn new(kind: Kind) -> Gate {
        let (model, shown) = match kind {
            Kind::Greeter => (model::Gate::login(people()), true),
            Kind::Desktop { name } => (model::Gate::lock(name), false),
        };
        Gate {
            model,
            shown,
            overlay: Overlay::new(),
            screen: Rect::new(0, 0, 0, 0),
            minute: None,
            asking: None,
            started: None,
            ctrl: false,
            alt: false,
        }
    }

    pub fn greeter(&self) -> bool {
        self.model.greeter()
    }

    pub fn shown(&self) -> bool {
        self.shown
    }

    /// The person whose desktop this is.
    pub fn person(&self) -> Option<&str> {
        match self.greeter() {
            true => None,
            false => self.model.person(),
        }
    }

    /// What to repaint, the screen laid out again on `screen` when it
    /// changed or the minute it shows went by.
    pub fn take_damage(
        &mut self,
        screen: Rect,
        pointer: (i32, i32),
    ) -> impl Iterator<Item = DamageRect> + use<> {
        if !self.shown {
            self.overlay.hide();
            return self.overlay.take_damage();
        }
        let now = sys_core::realtime_secs().and_then(WallTime::from_unix);
        let minute = now.map(|t| t.minute());
        if self.overlay.stale() || self.screen != screen || self.minute != minute {
            let role = match self.model.focus() {
                Focus::People => Role::List,
                Focus::Field => Role::TextField,
                Focus::Switch | Focus::Submit => Role::Button,
            };
            let focus = Some((role, views::gate_focus_name(&self.model)));
            let node = views::gate(&self.model, now.as_ref());
            self.overlay.show(&node, screen, pointer, focus);
            self.screen = screen;
            self.minute = minute;
        }
        self.overlay.take_damage()
    }

    pub fn lock(&mut self) {
        if self.greeter() || self.shown {
            return;
        }
        self.shown = true;
        self.reset();
        tell_locked();
    }

    /// Brought in front by init: the login screen starts over, listing the
    /// people as they are now.
    pub fn in_front(&mut self) {
        if self.greeter() && self.asking.is_none() {
            self.reset();
        }
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
        let listed = match self.greeter() {
            true => people(),
            false => Vec::new(),
        };
        self.model.reset(listed);
        self.started = None;
        self.overlay.changed();
    }

    /// Settle what the mechanism answered.
    pub fn poll(&mut self) {
        if self.started.is_some_and(|at| at.elapsed() >= STARTING_FOR) {
            self.reset();
        }
        let Some(asking) = &self.asking else {
            return;
        };
        let checked = match asking.try_recv() {
            Ok(checked) => checked,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Checked::Unchecked,
        };
        self.asking = None;
        self.model.checked(checked);
        match (checked, self.greeter()) {
            (Checked::Passed, true) => self.started = Some(Instant::now()),
            (Checked::Passed, false) => {
                self.shown = false;
                self.reset();
            }
            _ => {}
        }
        self.overlay.changed();
    }

    /// The secure attention key always, any key while it is up, and a repeat
    /// of a key pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let keycode = event.key_keycode();
        self.track(keycode, true);
        if keycode == KEY_DELETE && self.chord(event.key_modifiers()) {
            self.overlay.taken.take(event);
            match self.greeter() {
                true if self.asking.is_none() && self.started.is_none() => self.reset(),
                true => {}
                false => self.lock(),
            }
            return true;
        }
        if !self.shown {
            return self.overlay.taken.repeats(event);
        }
        self.overlay.taken.take(event);
        if let Some(key) = Key::read(event) {
            let asks = self.model.key(key);
            self.overlay.changed();
            self.act(asks);
        }
        true
    }

    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.track(keycode, false);
        self.overlay.taken.release(keycode)
    }

    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let sent: Vec<Target> = self.overlay.click(x, y, pressed);
        for target in sent {
            let asks = self.model.click(target);
            self.overlay.changed();
            self.act(asks);
        }
    }

    /// A person chosen for each notch.
    pub fn wheel(&mut self, value_v120: i32) {
        if value_v120 == 0 {
            return;
        }
        for _ in 0..(value_v120.unsigned_abs() / 120).max(1) {
            self.model.scroll(value_v120 > 0);
        }
        self.overlay.changed();
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.overlay.hover(x, y);
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

    fn act(&mut self, asks: Option<Asks>) {
        match asks {
            Some(Asks::Check { person, secret }) => self.check(person, secret),
            Some(Asks::SwitchUser) => super::ask_init(Command::Greeter),
            None => {}
        }
    }

    /// Hand the password to the mechanism on a thread of its own, which
    /// answers once privd has.
    fn check(&mut self, person: String, secret: slopos_priv_core::consent::Secret) {
        let (action, args) = match self.greeter() {
            true => (
                LOGIN_ACTION,
                vec!["login".to_owned(), "desktop".into(), person],
            ),
            false => (UNLOCK_ACTION, vec!["unlock".to_owned()]),
        };
        let mut input = secret.bytes().to_vec();
        input.push(b'\n');
        drop(secret);
        let unlock = !self.greeter();
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("compositor-gate".into())
            .spawn(move || {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let checked = asked(action, &args, &input);
                slopos_tls_core::ct::wipe(&mut input);
                let checked = match (checked, unlock) {
                    (Checked::Passed, true) => {
                        let _telling = TELLING.lock().unwrap_or_else(PoisonError::into_inner);
                        LOCKS.fetch_add(1, Ordering::SeqCst);
                        match init_control::ask(Command::Locked(false)) {
                            Ok(()) => Checked::Passed,
                            Err(why) => {
                                log!("init: {why}");
                                Checked::Unchecked
                            }
                        }
                    }
                    (checked, _) => checked,
                };
                let _ = tx.send(checked);
            });
        match spawned {
            Ok(_) => self.asking = Some(rx),
            Err(_) => self.model.checked(Checked::Unchecked),
        }
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        if self.shown {
            self.overlay.draw(buf, clip);
        }
    }
}

/// Why the mechanism was not reached goes to the log.
fn asked(action: &str, args: &[&str], input: &[u8]) -> Checked {
    match super::privd(action, args, input) {
        Ok(0) => Checked::Passed,
        Ok(_) => Checked::Wrong,
        Err(why) => {
            log!("{action}: {why}");
            Checked::Unchecked
        }
    }
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
                log!("init: {why}");
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
                        log!("init: {why}");
                        return;
                    }
                    Err(_) => {}
                }
                drop(telling);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            log!("init did not hear that the desktop locked");
        });
}

fn people() -> Vec<String> {
    crate::accounts::people()
        .into_iter()
        .map(|p| p.name)
        .collect()
}
