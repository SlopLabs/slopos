//! The system menu and the network and clock panels under the menu bar. One
//! the person opened, or has since reached with the pointer, takes every key.

use std::time::{Duration, Instant};

use slopos_abi::InputEvent;
use slopos_appkit::traits::Role;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::key::Key;
use slopos_chrome_core::menu::{Choice, Menu, Picked};
use slopos_chrome_core::netstate::NetPanelModel;
use slopos_chrome_core::positioner::{Positioner, Rect, Size, position};
use slopos_chrome_core::status::{BAR_HEIGHT, StatusKind};
use slopos_theme_core::space;

use super::overlay::Overlay;
use super::views;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{core as sys_core, process};

/// Below the bar's border row, and from the screen's other edges.
const PANEL_GAP: i32 = space::SM;

/// How long the switch shows what was asked before it reverts to what the
/// kernel says, so a lost completion cannot leave it busy.
const SWITCH_SETTLE_MS: u128 = 10_000;

/// Before asking again: two presses' children run in no set order, so the
/// kernel can end in the state asked first until the last ask is repeated.
const SWITCH_RETRY_MS: u128 = 750;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    System,
    Status(StatusKind),
}

/// What the open panel was laid out showing, so a change lays it out again.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shows {
    System(Menu),
    Network(NetPanelModel, Option<bool>),
    Clock(Option<i64>),
}

pub struct Popover {
    open: Option<Panel>,
    /// The item it opened under, which a change of size keeps it under.
    anchor: Rect,
    overlay: Overlay,
    shows: Option<Shows>,
    menu: Menu,
    person: String,
    /// A new base changed the session's programs; the system menu says so
    /// until the session restarts.
    restart_wanted: bool,
    /// The system menu opened on its own; until then a press on it was
    /// meant for what was under the pointer before.
    restart_armed_at: Option<Instant>,
    /// It opened on its own and the pointer has not been over it since, so
    /// the keys still go to whoever was typing.
    unasked: bool,
    /// The press now held down landed while the menu was armed.
    pressed_armed: bool,
    /// The person may move the networking switch unasked, so it is shown.
    switch_offered: bool,
    /// What the switch was last asked for, while the kernel has yet to agree.
    pending: Option<bool>,
    pending_since: Option<Instant>,
    issued_at: Option<Instant>,
    choice: Option<Choice>,
}

impl Popover {
    pub fn new(switch_offered: bool, person: String) -> Self {
        Self {
            open: None,
            anchor: Rect::EMPTY,
            overlay: Overlay::new(),
            shows: None,
            menu: Menu::new(false),
            person,
            restart_wanted: false,
            restart_armed_at: None,
            unasked: false,
            pressed_armed: false,
            switch_offered,
            pending: None,
            pending_since: None,
            issued_at: None,
            choice: None,
        }
    }

    pub fn take_choice(&mut self) -> Option<Choice> {
        self.choice.take()
    }

    /// Whether the system menu notes that the session wants a restart;
    /// `true` when it did not before.
    pub fn want_restart(&mut self, wanted: bool) -> bool {
        let newly = wanted && !self.restart_wanted;
        if wanted != self.restart_wanted {
            self.menu = Menu::new(wanted);
        }
        self.restart_wanted = wanted;
        newly
    }

    /// While it is open, which holds the pointer.
    pub fn rect(&self) -> Option<slopos_appkit::Rect> {
        self.open.and(self.overlay.rect())
    }

    /// Open `panel` under `item`, or close it if it is the one open.
    pub fn toggle(&mut self, panel: Panel, item: DamageRect) {
        if self.open == Some(panel) {
            self.dismiss();
            return;
        }
        self.restart_armed_at = None;
        self.unasked = false;
        self.open(panel, item);
    }

    /// Open the system menu under `item` unasked, to offer the restart a
    /// new base wants, unless the person has it open already.
    pub fn offer_restart(&mut self, item: DamageRect) {
        if self.open == Some(Panel::System) {
            return;
        }
        self.open(Panel::System, item);
        self.unasked = true;
        self.restart_armed_at =
            Some(Instant::now() + Duration::from_millis(slopos_priv_core::consent::ARMING_MS));
    }

    fn open(&mut self, panel: Panel, item: DamageRect) {
        self.anchor = item.into();
        self.open = Some(panel);
        self.menu = Menu::new(self.restart_wanted);
        self.shows = None;
        self.overlay.hide();
    }

    pub fn dismiss(&mut self) {
        self.open = None;
        self.shows = None;
        self.overlay.hide();
    }

    fn armed(&self) -> bool {
        self.restart_armed_at.is_none_or(|at| Instant::now() >= at)
    }

    fn showing(&self, model: &NetPanelModel) -> Option<Shows> {
        Some(match self.open? {
            Panel::System => Shows::System(self.menu),
            Panel::Status(StatusKind::Network) => {
                Shows::Network(*model, self.switch_offered.then(|| self.switch_on(model)))
            }
            Panel::Status(StatusKind::Clock) => {
                Shows::Clock(sys_core::realtime_secs().map(|secs| secs.div_euclid(60)))
            }
        })
    }

    /// Lay the open panel out again, under its item on a screen of
    /// `screen_w` by `screen_h`, once what it shows has changed.
    pub fn refresh(
        &mut self,
        model: &NetPanelModel,
        screen_w: u32,
        screen_h: u32,
        pointer: (i32, i32),
    ) {
        let shows = self.showing(model);
        if shows == self.shows && !self.overlay.stale() {
            return;
        }
        self.shows = shows;
        let Some(shows) = shows else {
            self.overlay.hide();
            return;
        };
        let area = panel_area(screen_w, screen_h);
        let most = slopos_appkit::Size::new(views::PANEL_WIDTH.min(area.w), area.h);
        let anchor = self.anchor;
        let place = |size: slopos_appkit::Size| {
            let size = Size {
                w: size.width,
                h: size.height,
            };
            let r = position(&Positioner::below_bar_item(anchor, size, PANEL_GAP), area);
            slopos_appkit::Rect::new(r.x, r.y, r.w, r.h)
        };
        match shows {
            Shows::System(menu) => {
                let focus = views::menu_focus_name(&menu);
                let node = views::system_menu(&menu, &self.person);
                self.overlay
                    .show_natural(&node, most, place, pointer, focus);
            }
            Shows::Network(model, switch) => {
                let node = views::network_panel(&model, switch);
                self.overlay
                    .show_natural(&node, most, place, pointer, None::<(Role, &str)>);
            }
            Shows::Clock(minute) => {
                let now = minute.and_then(|m| WallTime::from_unix(m * 60));
                let node = views::clock_panel::<()>(now.as_ref());
                self.overlay
                    .show_natural(&node, most, place, pointer, None::<(Role, &str)>);
            }
        }
    }

    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        if pressed {
            self.pressed_armed = self.armed();
            self.unasked = false;
        }
        let answers = self.pressed_armed;
        match self.open {
            Some(Panel::System) => {
                let chosen: Vec<Choice> = self.overlay.click(x, y, pressed);
                if let Some(&choice) = chosen.first().filter(|_| answers) {
                    self.choice = Some(choice);
                    self.dismiss();
                }
            }
            Some(Panel::Status(StatusKind::Network)) => {
                let toggled: Vec<views::Toggle> = self.overlay.click(x, y, pressed);
                if !toggled.is_empty() {
                    self.toggle_switch();
                }
            }
            Some(Panel::Status(StatusKind::Clock)) => {
                let _: Vec<()> = self.overlay.click(x, y, pressed);
            }
            None => {}
        }
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        if self.rect().is_some_and(|r| r.contains(x, y)) {
            self.unasked = false;
        }
        self.overlay.hover(x, y);
    }

    /// Every key while it is open, once the person has opened or reached it,
    /// and a repeat of one pressed into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let Some(panel) = self.open.filter(|_| !self.unasked) else {
            return self.overlay.taken.repeats(event);
        };
        self.overlay.taken.take(event);
        match (panel, Key::read(event)) {
            (_, Some(Key::Dismiss)) => self.dismiss(),
            (Panel::System, Some(key)) => match self.menu.key(key) {
                Some(Picked::Choice(choice)) if self.armed() => {
                    self.choice = Some(choice);
                    self.dismiss();
                }
                Some(Picked::Close) => self.dismiss(),
                _ => {}
            },
            _ => {}
        }
        true
    }

    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.overlay.taken.release(keycode)
    }

    /// What was asked for while a request is outstanding, otherwise what the
    /// kernel says.
    fn switch_on(&self, model: &NetPanelModel) -> bool {
        self.pending.unwrap_or(model.enabled)
    }

    /// Settle or abandon an outstanding request: the kernel agreeing is the
    /// normal exit, the deadline the one that matters.
    pub fn settle(&mut self, model: &NetPanelModel) {
        let Some(wanted) = self.pending else {
            return;
        };
        if model.enabled == wanted {
            self.clear_pending();
            return;
        }
        if self
            .pending_since
            .is_none_or(|since| since.elapsed().as_millis() >= SWITCH_SETTLE_MS)
        {
            // Given up on: a wrong switch position is worse than a refused one.
            self.clear_pending();
            return;
        }
        if self
            .issued_at
            .is_none_or(|at| at.elapsed().as_millis() >= SWITCH_RETRY_MS)
        {
            self.issue(wanted);
        }
    }

    /// Ask for the switch the other way from what it shows: during the busy
    /// window that differs from what the kernel says, and reading the kernel
    /// would ask for what is already being asked for.
    fn toggle_switch(&mut self) {
        let Some(Shows::Network(model, Some(_))) = self.shows else {
            return;
        };
        let wanted = !self.switch_on(&model);
        if self.pending == Some(wanted) {
            return;
        }
        // A press during an outstanding request retargets it rather than being
        // dropped, which would silently lose the intent.
        self.pending = Some(wanted);
        self.pending_since = Some(Instant::now());
        self.issue(wanted);
    }

    fn issue(&mut self, wanted: bool) {
        if spawn_ip_net(wanted) {
            self.issued_at = Some(Instant::now());
        }
    }

    fn clear_pending(&mut self) {
        self.pending = None;
        self.pending_since = None;
        self.issued_at = None;
    }

    pub fn take_damage(&mut self) -> impl Iterator<Item = DamageRect> + use<> {
        self.overlay.take_damage()
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        self.overlay.draw(buf, clip);
    }
}

/// The screen below the bar, inset on the other three sides; the gap below
/// the bar is the positioner's offset.
fn panel_area(screen_w: u32, screen_h: u32) -> Rect {
    let top = BAR_HEIGHT + 1;
    Rect::new(
        PANEL_GAP,
        top,
        screen_w as i32 - 2 * PANEL_GAP,
        screen_h as i32 - top - PANEL_GAP,
    )
}

/// Whether `/bin/ip net` started, on a privd connection of the compositor's
/// own, which the policy lets switch networking unasked; it holds no `NetAdmin`.
fn spawn_ip_net(enable: bool) -> bool {
    let Ok(privd) = crate::privd::Connection::open() else {
        return false;
    };
    let verb: &[u8] = if enable { b"on\0" } else { b"off\0" };
    let argv: [*const u8; 3] = [b"ip\0".as_ptr(), b"net\0".as_ptr(), verb.as_ptr()];
    let env = format!("{}={PRIVD_AT}\0", slopos_priv_core::ENV_FD);
    let envp = [env.as_ptr()];
    // `ip` reads nothing, and the compositor's stdin would put a child on
    // the console's input.
    let actions = [
        process::clone_fd(1, 1),
        process::clone_fd(2, 2),
        process::clone_fd(privd.raw(), PRIVD_AT),
    ];
    let tid = process::spawn_path_with_env(
        b"/bin/ip",
        &argv,
        &envp,
        slopos_abi::task::TaskPriority::Normal,
        slopos_abi::task::TASK_FLAG_USER_MODE,
        &actions,
        0,
    );
    tid > 0
}

const PRIVD_AT: i32 = 3;
