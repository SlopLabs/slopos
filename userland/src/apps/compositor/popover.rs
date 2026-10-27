//! The menu bar's popovers: the system menu under the bar's mark, and the
//! network and clock panels under their status items.
//!
//! Drawn by the compositor rather than by a client: they show state the
//! compositor already holds, their light-dismiss pointer grab has to live
//! here whichever process draws the pixels, and they must stay available when
//! `ProtocolBridge::new()` returns `None`. Each is a tree of appkit's
//! components laid out through [`Embedded`] at its own size, placed by
//! [`slopos_chrome_core::positioner`]. A popover the person opened, or has
//! since reached with the pointer, takes every key, as a menu does: the
//! system menu's arrows move through it, and Escape closes any.

use std::time::{Duration, Instant};

use slopos_abi::InputEvent;
use slopos_appkit::embed::Embedded;
use slopos_chrome_core::Key;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::menu::{Choice, Menu, Picked};
use slopos_chrome_core::netstate::NetPanelModel;
use slopos_chrome_core::positioner::{Positioner, Rect, Size, position};
use slopos_chrome_core::status::StatusKind;
use slopos_theme_core::space;

use super::views;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{core as sys_core, process};
use crate::theme::SYSTEM_BAR_HEIGHT;

/// Gap between the bar's border row and the top of a panel, and between a
/// panel and the screen's other edges.
const PANEL_GAP: i32 = space::SM;

/// How long a requested switch change may stay unconfirmed. The control shows
/// what was asked for immediately and settles when the kernel's own event
/// agrees; at expiry it reverts to whatever the kernel actually says, so a lost
/// completion cannot wedge the control in a busy state forever.
const SWITCH_SETTLE_MS: u128 = 10_000;

/// How long to wait for the kernel to agree before asking again. `ip net on|off`
/// is idempotent, and re-issuing is what makes the control converge: two presses
/// spawn two children whose execution nothing orders, so the pair can land in
/// the wrong order and leave the kernel in the state asked for *first*.
const SWITCH_RETRY_MS: u128 = 750;

/// What a popover shows: the system menu under the bar's mark, or a status
/// item's panel.
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
    rect: Rect,
    /// Where it was last frame, so closing or moving repaints what it vacated.
    prev_rect: Rect,
    /// The item the panel was anchored under, kept so a change of size
    /// re-anchors to the same place.
    anchor: Rect,
    tree: Option<Embedded>,
    shows: Option<Shows>,
    /// What it shows changed without the panel moving, which the rect
    /// comparison alone would not ask a repaint for.
    dirty: bool,
    menu: Menu,
    /// Whose desktop the system menu is of.
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
    /// The networking switch is shown: the person may move it unasked.
    switch_offered: bool,
    /// What the switch was last asked for, while the kernel has yet to agree.
    pending: Option<bool>,
    /// When the target was last changed, for [`SWITCH_SETTLE_MS`].
    pending_since: Option<Instant>,
    /// When a command was last spawned, for [`SWITCH_RETRY_MS`].
    issued_at: Option<Instant>,
    /// What the person chose in the system menu, for the frame to act on.
    choice: Option<Choice>,
    taken: slopos_chrome_core::key::Taken,
    /// Where the pointer is, which a tree built afresh lights up.
    pointer: (i32, i32),
}

impl Popover {
    pub fn new(switch_offered: bool, person: String) -> Self {
        Self {
            open: None,
            rect: Rect::EMPTY,
            prev_rect: Rect::EMPTY,
            anchor: Rect::EMPTY,
            tree: None,
            shows: None,
            dirty: false,
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
            taken: slopos_chrome_core::key::Taken::default(),
            pointer: (-1, -1),
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

    /// The popover's rect while it is open; `None` when closed, which
    /// `resolve_cursor_hit` reads as "no grab".
    pub fn rect(&self) -> Option<Rect> {
        if self.open.is_some() && !self.rect.is_empty() {
            Some(self.rect)
        } else {
            None
        }
    }

    /// Open the popover for `panel` under `item`, or close it if that panel
    /// is already the open one.
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
        self.anchor = Rect::new(
            item.x0,
            item.y0,
            item.x1 - item.x0 + 1,
            item.y1 - item.y0 + 1,
        );
        self.open = Some(panel);
        self.menu = Menu::new(self.restart_wanted);
        self.shows = None;
        self.tree = None;
        self.rect = Rect::EMPTY;
    }

    /// Close the popover. Idempotent: a dismiss with nothing open is what a
    /// click on the desktop looks like, and must not manufacture damage.
    pub fn dismiss(&mut self) {
        self.open = None;
        self.tree = None;
        self.shows = None;
    }

    fn armed(&self) -> bool {
        self.restart_armed_at.is_none_or(|at| Instant::now() >= at)
    }

    /// What the open panel shows now.
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

    /// Lay the open panel out again when what it shows has changed since it
    /// was, at the size it now takes, under its item on a screen of `screen_w`
    /// by `screen_h`.
    pub fn refresh(&mut self, model: &NetPanelModel, screen_w: u32, screen_h: u32) {
        let shows = self.showing(model);
        if shows == self.shows && self.tree.is_some() == shows.is_some() {
            return;
        }
        self.shows = shows;
        self.dirty = true;
        let Some(shows) = shows else {
            self.tree = None;
            return;
        };
        let area = panel_area(screen_w, screen_h);
        let most = slopos_appkit::Size::new(views::PANEL_WIDTH.min(area.w), area.h);
        let (rect, mut tree) = match shows {
            Shows::System(menu) => placed(
                views::system_menu(&menu, &self.person),
                self.anchor,
                area,
                most,
            ),
            Shows::Network(model, switch) => placed(
                views::network_panel(&model, switch),
                self.anchor,
                area,
                most,
            ),
            Shows::Clock(minute) => {
                let now = minute.and_then(|m| WallTime::from_unix(m * 60));
                placed::<()>(views::clock_panel(now.as_ref()), self.anchor, area, most)
            }
        };
        if let Shows::System(menu) = shows
            && let Some((role, name)) = views::menu_focus_name(&menu)
        {
            tree.focus(role, name);
        }
        tree.hover(self.pointer.0, self.pointer.1);
        self.rect = rect;
        self.tree = Some(tree);
    }

    /// The primary button went down or up at `(x, y)`, over the popover.
    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        if pressed {
            self.pressed_armed = self.armed();
            self.unasked = false;
        }
        let answers = self.pressed_armed;
        let (Some(panel), Some(tree)) = (self.open, self.tree.as_mut()) else {
            return;
        };
        self.dirty = true;
        match panel {
            Panel::System => {
                let chosen: Vec<Choice> = tree.click(x, y, pressed);
                if let Some(&choice) = chosen.first().filter(|_| answers) {
                    self.choice = Some(choice);
                    self.dismiss();
                }
            }
            Panel::Status(StatusKind::Network) => {
                let toggled: Vec<views::Toggle> = tree.click(x, y, pressed);
                if !toggled.is_empty() {
                    self.toggle_switch();
                }
            }
            Panel::Status(StatusKind::Clock) => {
                let _: Vec<()> = tree.click(x, y, pressed);
            }
        }
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.pointer = (x, y);
        if self.rect().is_some_and(|r| r.contains(x, y)) {
            self.unasked = false;
        }
        if let Some(tree) = self.tree.as_mut() {
            self.dirty |= tree.hover(x, y);
        }
    }

    /// Whether a key press is the popover's: every one while it is open,
    /// once the person has opened or reached it, and a repeat of one pressed
    /// into it.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let Some(panel) = self.open.filter(|_| !self.unasked) else {
            return self.taken.repeats(event);
        };
        self.taken.take(event);
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
        self.taken.release(keycode)
    }

    /// The position the switch shows: what was asked for while a request
    /// is outstanding, otherwise what the kernel says.
    fn switch_on(&self, model: &NetPanelModel) -> bool {
        self.pending.unwrap_or(model.enabled)
    }

    /// Settle or abandon an outstanding request. Called every frame; the kernel
    /// agreeing is the normal exit, the deadline the one that matters.
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

    /// Rects to repaint: whatever the popover covered last frame and whatever it
    /// covers now, emitted only when either changed.
    pub fn take_damage(&mut self, out: &mut [DamageRect]) -> usize {
        let current = self.rect().unwrap_or(Rect::EMPTY);
        if current == self.prev_rect && !self.dirty {
            return 0;
        }
        self.dirty = false;
        let mut count = 0usize;
        for rect in [self.prev_rect, current] {
            if !rect.is_empty() && count < out.len() {
                out[count] = DamageRect {
                    x0: rect.x,
                    y0: rect.y,
                    x1: rect.right() - 1,
                    y1: rect.bottom() - 1,
                };
                count += 1;
            }
        }
        self.prev_rect = current;
        count
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        let (Some(rect), Some(tree)) = (self.rect(), &self.tree) else {
            return;
        };
        let clip = slopos_appkit::Rect::new(
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
        );
        let mine = slopos_appkit::Rect::new(rect.x, rect.y, rect.w, rect.h);
        if let Some(clip) = clip.intersect(&mine) {
            tree.paint(buf, clip);
        }
    }
}

/// `node` at the size it takes within `most`, under `anchor` in `area`.
fn placed<M: Clone + 'static>(
    node: slopos_appkit::Node<M>,
    anchor: Rect,
    area: Rect,
    most: slopos_appkit::Size,
) -> (Rect, Embedded) {
    let natural = Embedded::natural_size(&node, most);
    let size = Size {
        w: natural.width,
        h: natural.height,
    };
    let rect = position(&Positioner::below_bar_item(anchor, size, PANEL_GAP), area);
    let tree = Embedded::new(
        &node,
        slopos_appkit::Rect::new(rect.x, rect.y, rect.w, rect.h),
    );
    (rect, tree)
}

/// The region a popover may occupy: the screen below the bar, inset on the other
/// three sides. No inset on top — the gap below the bar is the positioner's
/// offset, and insetting here as well would count it twice.
fn panel_area(screen_w: u32, screen_h: u32) -> Rect {
    let top = SYSTEM_BAR_HEIGHT + 1;
    Rect::new(
        PANEL_GAP,
        top,
        screen_w as i32 - 2 * PANEL_GAP,
        screen_h as i32 - top - PANEL_GAP,
    )
}

/// Ask `/bin/ip` to move the master networking switch, through a privd
/// connection this process makes: the request is attested as the
/// compositor's, which the policy lets turn networking on and off without
/// asking. The compositor itself never holds `NetAdmin`, which would widen
/// the process that already owns the framebuffer and every input event.
///
/// Returns whether the child started; the operation's own result arrives later
/// as a `net_monitor` event.
fn spawn_ip_net(enable: bool) -> bool {
    let Ok(privd) = crate::privd::Connection::open() else {
        return false;
    };
    let verb: &[u8] = if enable { b"on\0" } else { b"off\0" };
    let argv: [*const u8; 3] = [b"ip\0".as_ptr(), b"net\0".as_ptr(), verb.as_ptr()];
    let env = format!("{}={PRIVD_AT}\0", slopos_priv_core::ENV_FD);
    let envp = [env.as_ptr()];
    // stdout and stderr only: `ip` reads nothing, and handing it the
    // compositor's stdin would put a child on the console's input.
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

/// Where `ip` finds the compositor's privd connection.
const PRIVD_AT: i32 = 3;
