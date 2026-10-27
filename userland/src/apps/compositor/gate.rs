//! The login screen and the lock screen, drawn over the dimmed wallpaper in
//! place of every surface. While either is up every key is its own and no
//! pointer press reaches a client, so what is typed there reaches the
//! mechanism privd runs alone: `org.slopos.login` lets a person in from the
//! login screen, and `org.slopos.session.unlock` answers whether a locked
//! desktop's own person typed their password. Ctrl+Alt+Delete, the secure
//! attention key, is read here and nowhere else: on a desktop it locks it,
//! so the screen that asks for a password after it is this one.

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
use slopos_appkit::embed::Embedded;
use slopos_appkit::traits::Role as Shows;
use slopos_chrome_core::Key;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::gate::{self as model, Asks, Checked, Focus, Target};

use super::views;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::init_control::{self, Command};
use crate::syscall::core as sys_core;

/// How long the login screen says a desktop is starting before it starts
/// over, should that desktop never come in front.
const STARTING_FOR: Duration = Duration::from_secs(30);

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

pub struct Gate {
    model: model::Gate,
    shown: bool,
    /// What the screen draws, laid out on `screen` at `minute`; built afresh
    /// once `stale`.
    tree: Option<Embedded>,
    screen: Rect,
    minute: Option<i64>,
    stale: bool,
    /// What the mechanism privd ran answered, while it runs.
    asking: Option<Receiver<Checked>>,
    dirty: bool,
    /// When a login passed, while that person's desktop starts.
    started: Option<Instant>,
    /// Where the pointer is, which a tree built afresh lights up.
    pointer: (i32, i32),
    taken: slopos_chrome_core::key::Taken,
    ctrl: bool,
    alt: bool,
}

impl Gate {
    pub fn new(role: Role) -> Gate {
        let (model, shown) = match role {
            Role::Greeter => (model::Gate::login(people()), true),
            Role::Desktop { name } => (model::Gate::lock(name), false),
        };
        Gate {
            model,
            shown,
            tree: None,
            screen: Rect::new(0, 0, 0, 0),
            minute: None,
            stale: true,
            asking: None,
            dirty: true,
            started: None,
            pointer: (-1, -1),
            taken: slopos_chrome_core::key::Taken::default(),
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

    /// Whether the screen changed since the last frame, laid out again on
    /// `screen` when it did, or when the minute it shows went by.
    pub fn take_dirty(&mut self, screen: Rect) -> bool {
        if !self.shown {
            self.tree = None;
            return std::mem::take(&mut self.dirty);
        }
        let now = sys_core::realtime_secs().and_then(WallTime::from_unix);
        let minute = now.map(|t| t.minute());
        if self.stale || self.tree.is_none() || self.screen != screen || self.minute != minute {
            let node = views::gate(&self.model, now.as_ref());
            let mut tree = Embedded::new(&node, screen);
            tree.hover(self.pointer.0, self.pointer.1);
            self.tree = Some(tree);
            let role = match self.model.focus() {
                Focus::People => Shows::List,
                Focus::Field => Shows::TextField,
                Focus::Switch | Focus::Submit => Shows::Button,
            };
            if let Some(tree) = &mut self.tree {
                tree.focus(role, views::gate_focus_name(&self.model));
            }
            self.screen = screen;
            self.minute = minute;
            self.stale = false;
            self.dirty = true;
        }
        std::mem::take(&mut self.dirty)
    }

    fn changed(&mut self) {
        self.stale = true;
        self.dirty = true;
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
        self.changed();
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
        self.changed();
    }

    /// Whether a key press is the screen's, which then takes it: the secure
    /// attention key always, any key while it is up, and a repeat of a key
    /// pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let keycode = event.key_keycode();
        self.track(keycode, true);
        if keycode == KEY_DELETE && self.chord(event.key_modifiers()) {
            self.taken.take(event);
            match self.greeter() {
                true if self.asking.is_none() && self.started.is_none() => self.reset(),
                true => {}
                false => self.lock(),
            }
            return true;
        }
        if !self.shown {
            return self.taken.repeats(event);
        }
        self.taken.take(event);
        if let Some(key) = Key::read(event) {
            let asks = self.model.key(key);
            self.changed();
            self.act(asks);
        }
        true
    }

    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.track(keycode, false);
        self.taken.release(keycode)
    }

    /// The primary button went down or up at `(x, y)` over the screen.
    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        let sent: Vec<Target> = tree.click(x, y, pressed);
        self.dirty = true;
        for target in sent {
            let asks = self.model.click(target);
            self.changed();
            self.act(asks);
        }
    }

    /// The wheel turned by `value_v120` over the screen: a person chosen
    /// for each notch.
    pub fn wheel(&mut self, value_v120: i32) {
        if value_v120 == 0 {
            return;
        }
        for _ in 0..(value_v120.unsigned_abs() / 120).max(1) {
            self.model.scroll(value_v120 > 0);
        }
        self.changed();
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.pointer = (x, y);
        if let Some(tree) = self.tree.as_mut() {
            self.dirty |= tree.hover(x, y);
        }
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
            true => (LOGIN, vec!["login".to_owned(), "desktop".into(), person]),
            false => (UNLOCK, vec!["unlock".to_owned()]),
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
                                eprintln!("compositor: init: {why}");
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
        let (true, Some(tree)) = (self.shown, &self.tree) else {
            return;
        };
        let clip = Rect::new(
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
        );
        tree.paint(buf, clip);
    }
}

/// How the mechanism answered: run, refused or not reached; why it was not
/// reached goes to the log.
fn asked(action: &str, args: &[&str], input: &[u8]) -> Checked {
    let answered = crate::syscall::fs::open_cstr(c"/dev/null", slopos_abi::fs::O_RDWR)
        .map_err(|e| crate::privd::NotRun::from(format!("/dev/null: {e}")))
        .and_then(|null| crate::privd::run_feeding(action, args, input, null.raw(), 2));
    match answered {
        Ok(0) => Checked::Passed,
        Ok(_) => Checked::Wrong,
        Err(why) => {
            eprintln!("compositor: {action}: {why}");
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
