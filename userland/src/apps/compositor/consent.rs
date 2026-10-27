//! privd's questions to the person at the machine, drawn over every surface.
//!
//! The channel is the socket pair init made, so an answer here is the
//! compositor's and nothing else's; the compositor owns both seats, so the
//! answer is the keyboard's or the pointer's. While a question is up, no key
//! or button pressed reaches a client and every surface is dimmed beneath it,
//! so a password typed into it reaches privd alone.

use std::string::String;

use slopos_abi::InputEvent;
use slopos_abi::input::keycode::{
    KEY_BACKSPACE, KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_KP_ENTER, KEY_LEFT, KEY_RIGHT, KEY_TAB, KEY_UP,
};
use slopos_abi::syscall::{F_GETFD, MSG_NOSIGNAL};
use slopos_appkit::embed::Embedded;
use slopos_appkit::text::ui;
use slopos_appkit::traits::Role;
use slopos_appkit::{
    Button, ButtonRow, CrossAxisAlignment, Disclosure, ImageData, ImageScale, Length, List,
    ListRow, Node, PropertyList, Rect, Text,
};
use slopos_gfx::image::ImageSampling;
use slopos_image::DecodeOptions;
use slopos_priv_core::consent::{
    self as protocol, Ask, Dialog, Focus, Inbound, Key, Kind, Link, kind,
};
use slopos_priv_core::{INIT, words};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};
use slopos_theme_core::{Icon, TextRole, Tone, space};

use crate::apps::privd::CONSENT_FD;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{OwnedFd, RawFd, SyscallError, core as sys_core, fs, net};

/// More keys than a hand holds down at once.
const TAKEN_MAX: usize = 16;
/// Well past the size an icon is drawn at, so a huge one is refused before
/// it is decoded.
const ICON_PIXELS_MAX: usize = 256 * 256;
const ICON_BYTES_MAX: u64 = 1 << 20;
const PASSWORD: &str = "Password";
const ADMIN_NEEDED: &str = "This needs an administrator's password.";
/// How long after its last key a password question that went unanswered
/// keeps taking keys, so the rest of a password typed into it reaches no
/// client.
const QUIET_MS: u64 = 1500;

pub struct Consent {
    fd: Option<OwnedFd>,
    frames: Deframer,
    link: Link,
    showing: Option<Dialog>,
    /// The question on screen as the tree that draws it, built afresh once
    /// it is `stale`.
    tree: Option<Embedded>,
    stale: bool,
    icon: Option<ImageData>,
    /// The screen and anchor the question was first laid out on, kept so the
    /// window of the app that asks cannot move it under the person's pointer.
    placed: Option<(Rect, Option<Rect>)>,
    /// The compositor can draw a question.
    drawing: bool,
    /// privd may send a question: a `READY` went since the last one came.
    offered: bool,
    dirty: bool,
    /// Keys pressed into the dialog, whose releases no client may see: a
    /// release names its key, and so would spell a password typed there.
    taken: Vec<u16>,
    quiet_until: Option<u64>,
    /// The version this side names in `READY`.
    speaks: u32,
}

/// The version to name in `READY`: this side's to a privd that reads one
/// there, which asks in the older of its own and that.
fn privd_reads() -> u32 {
    let interface = std::fs::read(slopos_boot_core::base::INTERFACE_PATH)
        .ok()
        .and_then(|text| slopos_boot_core::base::parse_interface(&text).ok());
    match interface {
        Some(n) if n >= protocol::SINCE_INTERFACE => protocol::VERSION,
        _ => 1,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    Press(Focus),
    Pick(usize),
}

/// The question as the person sees it: a dialog of appkit's components,
/// centred over `anchor` where that fits.
pub fn view(dialog: &Dialog, icon: Option<&ImageData>, anchor: Option<Rect>) -> Node<Msg> {
    let ask = dialog.ask();
    let mut content: Vec<Node<Msg>> = Vec::new();
    if let Some(detail) = &ask.detail {
        content.push(Text::new(detail.clone()).wrap().into());
    }
    if ask.kind == Kind::Admin {
        content.push(Text::new(ADMIN_NEEDED).wrap().into());
    }
    if ask.admins.len() > 1 {
        let rows = ask
            .admins
            .iter()
            .map(|name| ListRow::new(name.clone()).icon(Icon::User))
            .collect();
        content.push(
            List::new(rows)
                .selected(dialog.admin())
                .on_select(Msg::Pick)
                .into(),
        );
    }
    if let Some(label) = dialog.field_label() {
        let field = Node::LineEdit {
            text: ui::BULLET.repeat(dialog.typed_chars()),
            placeholder: String::from(PASSWORD),
            caret: dialog.typed_chars(),
            focused: false,
            icon: Some(Icon::KeyRound),
            suffix: String::new(),
            invalid: false,
            tab_stop: true,
            on_input: Some(|_| Msg::Press(Focus::Field)),
        };
        content.push(Node::VStack {
            children: vec![Text::new(label).role(TextRole::Label).wrap().into(), field],
            spacing: space::XS,
            align: CrossAxisAlignment::Stretch,
        });
    }
    let shown = |text: String| {
        Text::new(text)
            .role(TextRole::Mono)
            .tone(Tone::Secondary)
            .wrap_anywhere()
    };
    let mut details = PropertyList::new()
        .row_with("Asked by", shown(dialog.requester()))
        .row_with("Runs", shown(ask.command_line()))
        .row_with("In", shown(ask.directory()));
    if let Some(extension) = &ask.extension {
        details = details.row_with("Extension", shown(extension.clone()));
    }
    let toggle = match dialog.expanded() {
        true => words::SHOW_LESS,
        false => words::SHOW_DETAILS,
    };
    content.push(
        Disclosure::new(toggle, details)
            .expanded(dialog.expanded())
            .on_toggle(Msg::Press(Focus::Details))
            .into(),
    );
    let cancel = Button::new(words::CANCEL).on_press(Msg::Press(Focus::Deny));
    let allow = Button::new(dialog.verb()).on_press(Msg::Press(Focus::Allow));
    let footer = match ask.destructive {
        true => ButtonRow::new().cancel(cancel).destructive(allow),
        false => ButtonRow::new().cancel(cancel).confirm(allow),
    };
    let size = TextRole::Display.style().icon;
    let mark = match icon {
        Some(image) => Node::SizedBox {
            width: Some(Length::Px(size)),
            height: Some(Length::Px(size)),
            child: Box::new(Node::Image {
                image: image.clone(),
                scale: ImageScale::Fit,
                sampling: ImageSampling::for_resize(image.width, image.height, size, size),
            }),
        },
        None => {
            let (icon, tone) = match (dialog.names_app(), &ask.app) {
                (false, _) => (Icon::Shield, Tone::Secondary),
                (true, Some(_)) => (Icon::AppWindow, Tone::Secondary),
                (true, None) => (Icon::TriangleAlert, Tone::Warning),
            };
            Node::Icon {
                icon,
                role: TextRole::Display,
                tone,
            }
        }
    };
    Node::Dialog {
        title: dialog.headline(),
        icon: Some(Box::new(mark)),
        content: Box::new(Node::VStack {
            children: content,
            spacing: space::MD,
            align: CrossAxisAlignment::Stretch,
        }),
        footer,
        on_dismiss: None,
        anchor,
    }
}

/// The icon at `path`, which privd named from a share no process writes.
fn load_icon(path: &str) -> Option<ImageData> {
    if !protocol::icon_path(path) {
        return None;
    }
    let bytes = crate::app_entry::read_bounded(std::path::Path::new(path), ICON_BYTES_MAX)?;
    let options = DecodeOptions {
        max_pixels: ICON_PIXELS_MAX,
        ..DecodeOptions::default()
    };
    let image = slopos_image::decode(&bytes, options).ok()?;
    ImageData::new(image.width, image.height, image.pixels)
}

impl Consent {
    /// The end init handed over at [`CONSENT_FD`]: either end of a socket
    /// pair names its maker, so no other process can hand over one that
    /// passes.
    pub fn acquire() -> Consent {
        let fd = Sys::fcntl(CONSENT_FD, F_GETFD as i32, 0).ok().map(|_| {
            let _ = fs::set_fd_cloexec(CONSENT_FD);
            // SAFETY: init handed this descriptor over and nothing else here
            // owns it.
            unsafe { OwnedFd::from_raw(CONSENT_FD) }
        });
        let fd = fd.filter(|fd| {
            net::peer_cred(fd.raw()).is_ok_and(|cred| cred.uid == 0)
                && net::peer_program(fd.raw()).ok().flatten().as_deref() == Some(INIT.as_bytes())
                && net::set_nonblocking(fd.raw()).is_ok()
        });
        let mut consent = Consent {
            fd,
            frames: slopos_priv_core::wire::deframer(),
            link: Link::new(crate::privd::request_id()),
            showing: None,
            tree: None,
            stale: false,
            icon: None,
            placed: None,
            drawing: false,
            offered: false,
            dirty: false,
            taken: Vec::new(),
            quiet_until: None,
            speaks: privd_reads(),
        };
        if !consent.write(&consent.link.hello()) {
            consent.fd = None;
        }
        consent
    }

    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(OwnedFd::raw)
    }

    pub fn announce(&mut self) {
        self.drawing = true;
        self.offer();
    }

    /// Tell privd a question can be drawn, once nothing is on screen.
    fn offer(&mut self) {
        if !self.drawing || self.offered || self.showing.is_some() {
            return;
        }
        self.offered = true;
        if !self.write(&protocol::ready_frame(self.speaks)) {
            self.fd = None;
        }
    }

    fn write(&self, frame: &[u8]) -> bool {
        let Some(fd) = &self.fd else {
            return false;
        };
        let mut sent = 0;
        while sent < frame.len() {
            match net::send(fd.raw(), &frame[sent..], MSG_NOSIGNAL) {
                Ok(n) if n > 0 => sent += n,
                _ => return false,
            }
        }
        true
    }

    /// Take what privd sent; `false` once the channel is gone.
    pub fn read(&mut self) -> bool {
        let Some(fd) = &self.fd else {
            return false;
        };
        let mut buf = [0u8; 4096];
        loop {
            match fs::read_slice(fd.raw(), &mut buf) {
                Ok(n) if n > 0 => self.frames.push(&buf[..n]),
                Err(SyscallError::EAGAIN) => break,
                Err(SyscallError::EINTR) => {}
                _ => return self.close(),
            }
        }
        loop {
            let inbound = match self.frames.next_frame() {
                Ok(None) => return true,
                Ok(Some((kind, payload))) => self.link.take(kind, payload),
                Err(_) => return self.close(),
            };
            match inbound {
                Ok(Inbound::Stale) => {}
                Ok(Inbound::Reset(seen)) => {
                    self.withdraw();
                    self.speaks = privd_reads();
                    self.offered = false;
                    if !self.write(&seen) {
                        return self.close();
                    }
                    self.offer();
                }
                Ok(Inbound::Again(seen)) => {
                    if !self.write(&seen) {
                        return self.close();
                    }
                }
                Ok(Inbound::Frame(kind::ASK, payload)) => match Ask::decode(&payload) {
                    Ok(ask) => {
                        self.offered = false;
                        self.icon = ask.icon.as_deref().and_then(load_icon);
                        self.showing = Some(Dialog::new(ask));
                        self.placed = None;
                        self.stale = true;
                        self.dirty = true;
                    }
                    Err(_) => return self.close(),
                },
                Ok(Inbound::Frame(kind::CANCEL, payload)) => {
                    let Ok(id) = protocol::decode_cancel(&payload) else {
                        return self.close();
                    };
                    if self.showing.as_ref().is_some_and(|d| d.ask().id == id) {
                        self.withdraw();
                        self.offer();
                    }
                }
                Ok(Inbound::Frame(..)) | Err(_) => return self.close(),
            }
        }
    }

    fn close(&mut self) -> bool {
        self.fd = None;
        self.withdraw();
        false
    }

    fn withdraw(&mut self) {
        let Some(dialog) = self.showing.take() else {
            return;
        };
        self.tree = None;
        self.dirty = true;
        if dialog.ask().kind.takes_secret() {
            self.quiet_until = Some(sys_core::get_time_ms() + QUIET_MS);
        }
    }

    fn quiet(&self, now: u64) -> bool {
        self.quiet_until.is_some_and(|until| now < until)
    }

    /// Whether every key pressed now is the dialog's: one is up, or one that
    /// took a password went from under someone typing into it.
    pub fn capturing(&self) -> bool {
        self.showing.is_some() || self.quiet(sys_core::get_time_ms())
    }

    /// Whether a key press is the dialog's, which then takes it: while it
    /// is [`capturing`](Self::capturing), or when the press repeats a key
    /// pressed into the dialog. A fresh press of a key the dialog took is a
    /// client's, release and all, since the dialog's release was lost.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        let keycode = event.key_keycode();
        let held = self.taken.contains(&keycode);
        if self.capturing() || (held && event.key_is_repeat()) {
            self.key(event);
            return true;
        }
        if held {
            self.taken.retain(|&k| k != keycode);
        }
        false
    }

    pub fn active(&self) -> bool {
        self.showing.is_some()
    }

    /// The pid of who asks the question on screen, whose window it centres
    /// over.
    pub fn asker(&self) -> Option<u32> {
        self.showing.as_ref().map(|dialog| dialog.ask().pid)
    }

    /// Whether the dialog appeared, changed or went since the last frame,
    /// which shows it on `screen` over the `anchor` it was first laid out
    /// on. A screen that changed moves it, and input waits again; privd
    /// hears when a question first goes on screen.
    pub fn take_dirty(&mut self, screen: Rect, anchor: Option<Rect>) -> bool {
        let Some(dialog) = self.showing.as_mut() else {
            return std::mem::take(&mut self.dirty);
        };
        let now = sys_core::get_time_ms();
        let placed = match self.placed {
            None => (screen, anchor),
            Some((was, kept)) if was == screen => (was, kept),
            Some((_, kept)) => {
                dialog.moved(now);
                (screen, kept)
            }
        };
        if dialog.drawn(now) {
            eprintln!(
                "COMPOSITOR: shown a question from {}",
                printable(dialog.app())
            );
            let id = dialog.ask().id;
            if protocol::says_shown(self.speaks) && !self.write(&protocol::shown_frame(id)) {
                self.close();
                return std::mem::take(&mut self.dirty);
            }
        }
        if self.stale || self.tree.is_none() || self.placed != Some(placed) {
            self.lay_out(placed);
        }
        std::mem::take(&mut self.dirty)
    }

    fn lay_out(&mut self, (screen, anchor): (Rect, Option<Rect>)) {
        let Some(dialog) = &self.showing else {
            return;
        };
        let node = view(dialog, self.icon.as_ref(), anchor);
        match &mut self.tree {
            Some(tree) => tree.rebuild(&node, screen),
            None => self.tree = Some(Embedded::new(&node, screen)),
        }
        let (role, name) = match dialog.focus() {
            Focus::Field => (Role::TextField, PASSWORD),
            Focus::Details if dialog.expanded() => (Role::Button, words::SHOW_LESS),
            Focus::Details => (Role::Button, words::SHOW_DETAILS),
            Focus::Deny => (Role::Button, words::CANCEL),
            Focus::Allow => (Role::Button, dialog.verb()),
        };
        if let Some(tree) = &mut self.tree {
            tree.focus(role, name);
        }
        self.placed = Some((screen, anchor));
        self.stale = false;
        self.dirty = true;
    }

    /// Whether the release of `keycode` is the dialog's, which took its press.
    /// Any other release reaches the client that saw the press, question or
    /// not, so nothing stays held down behind one.
    pub fn takes_release(&mut self, keycode: u16) -> bool {
        let Some(at) = self.taken.iter().position(|&k| k == keycode) else {
            return false;
        };
        self.taken.remove(at);
        true
    }

    fn key(&mut self, event: &InputEvent) {
        if !event.key_is_repeat() && !self.taken.contains(&event.key_keycode()) {
            if self.taken.len() >= TAKEN_MAX {
                self.taken.remove(0);
            }
            self.taken.push(event.key_keycode());
        }
        let now = sys_core::get_time_ms();
        let Some(dialog) = self.showing.as_mut() else {
            if self.quiet(now) {
                self.quiet_until = Some(now + QUIET_MS);
            }
            return;
        };
        if event.key_is_repeat() && event.key_keycode() != KEY_BACKSPACE {
            return;
        }
        let key = match event.key_keycode() {
            KEY_TAB => Key::Move,
            KEY_LEFT | KEY_RIGHT => Key::Arrow,
            KEY_ENTER | KEY_KP_ENTER => Key::Press,
            KEY_ESC => Key::Dismiss,
            KEY_BACKSPACE => Key::Erase,
            KEY_UP => Key::Choose { next: false },
            KEY_DOWN => Key::Choose { next: true },
            _ => match char::from_u32(event.key_codepoint()) {
                Some(ch) if ch != '\0' => Key::Text(ch),
                _ => return,
            },
        };
        let answer = dialog.key(key, now);
        self.stale = true;
        self.dirty = true;
        if let Some(answer) = answer {
            self.answer(answer);
        }
    }

    /// The primary button went down or up at `(x, y)`: what the control
    /// there answers, for a press made while the dialog was armed.
    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let (Some(tree), Some(dialog)) = (self.tree.as_mut(), self.showing.as_mut()) else {
            return;
        };
        let now = sys_core::get_time_ms();
        if pressed && !dialog.pointer_down(now) {
            return;
        }
        let sent: Vec<Msg> = tree.click(x, y, pressed);
        self.dirty = true;
        for msg in sent {
            let Some(dialog) = self.showing.as_mut() else {
                return;
            };
            let answer = match msg {
                Msg::Press(target) => dialog.click(target, now),
                Msg::Pick(admin) => {
                    dialog.pick(admin, now);
                    None
                }
            };
            self.stale = true;
            if let Some(answer) = answer {
                self.answer(answer);
            }
        }
    }

    /// The primary button went up over the lock or login screen, which hides
    /// the question: whatever it pressed answers nothing.
    pub fn release_unseen(&mut self) {
        if let Some(tree) = self.tree.as_mut() {
            tree.release_unseen();
        }
    }

    /// A turn of the wheel at `(x, y)`, which scrolls what the question
    /// holds when it is taller than the screen.
    pub fn wheel(&mut self, x: i32, y: i32, value_v120: i32) {
        if let Some(tree) = self.tree.as_mut() {
            let _: Vec<Msg> = tree.wheel(x, y, value_v120);
            self.dirty = true;
        }
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        if let Some(tree) = self.tree.as_mut() {
            self.dirty |= tree.hover(x, y);
        }
    }

    fn answer(&mut self, answer: protocol::Answer) {
        if self.showing.take().is_none() {
            return;
        }
        self.tree = None;
        self.dirty = true;
        let mut frame = answer.frame();
        drop(answer);
        if !self.write(&frame) {
            self.fd = None;
        }
        protocol::wipe(&mut frame);
        self.offer();
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        let (Some(_), Some(tree)) = (&self.showing, &self.tree) else {
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

/// `text` with every control character shown as `?`, for a log line.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}
