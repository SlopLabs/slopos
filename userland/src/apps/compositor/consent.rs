//! privd's questions, over the socket pair init made: while one is up no key
//! or press reaches a client, so a password typed into it reaches privd alone.

use std::string::String;

use slopos_abi::InputEvent;
use slopos_abi::syscall::{F_GETFD, MSG_NOSIGNAL};
use slopos_appkit::text::ui;
use slopos_appkit::traits::Role;
use slopos_appkit::{
    Button, ButtonRow, Disclosure, ImageData, ImageScale, Length, List, ListRow, Node,
    PropertyList, Rect, Text, column,
};
use slopos_chrome_core::key::Key as Typed;
use slopos_gfx::image::ImageSampling;
use slopos_priv_core::consent::{
    self as protocol, Ask, Dialog, Focus, Inbound, Key, Kind, Link, kind,
};
use slopos_priv_core::{INIT, words};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};
use slopos_theme_core::{Icon, TextRole, Tone, space};

use super::overlay::Overlay;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::init_control::fds::COMPOSITOR_CONSENT;
use crate::syscall::{OwnedFd, RawFd, SyscallError, core as sys_core, fs, net};

/// How long after its last key a password question that went unanswered
/// keeps taking keys, so the rest of a password typed into it reaches no
/// client.
const QUIET_MS: u64 = 1500;

pub struct Consent {
    fd: Option<OwnedFd>,
    frames: Deframer,
    link: Link,
    showing: Option<Dialog>,
    overlay: Overlay,
    icon: Option<ImageData>,
    /// The screen and anchor the question was first laid out on, kept so the
    /// window of the app that asks cannot move it under the person's pointer.
    placed: Option<(Rect, Option<Rect>)>,
    drawing: bool,
    /// privd may send a question: a `READY` went since the last one came.
    offered: bool,
    quiet_until: Option<u64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Msg {
    Press(Focus),
    Pick(usize),
}

pub fn view(dialog: &Dialog, icon: Option<&ImageData>, anchor: Option<Rect>) -> Node<Msg> {
    let ask = dialog.ask();
    let mut content: Vec<Node<Msg>> = Vec::new();
    if let Some(detail) = &ask.detail {
        content.push(Text::new(detail.clone()).wrap().into());
    }
    if ask.kind == Kind::Admin {
        content.push(Text::new(words::ADMIN_NEEDED).wrap().into());
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
            placeholder: String::from(words::PASSWORD),
            caret: dialog.typed_chars(),
            focused: false,
            icon: Some(Icon::KeyRound),
            suffix: String::new(),
            invalid: false,
            tab_stop: true,
            on_input: Some(|_| Msg::Press(Focus::Field)),
        };
        content.push(column(
            space::XS,
            vec![Text::new(label).role(TextRole::Label).wrap().into(), field],
        ));
    }
    let shown = |text: String| {
        Text::new(text)
            .role(TextRole::Mono)
            .tone(Tone::Secondary)
            .wrap_anywhere()
    };
    let mut details = PropertyList::new()
        .row_with(words::ASKED_BY, shown(dialog.requester()))
        .row_with(words::RUNS, shown(ask.command_line()))
        .row_with(words::IN, shown(ask.directory()));
    if let Some(extension) = &ask.extension {
        details = details.row_with(words::EXTENSION, shown(extension.clone()));
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
    let allow = Button::new(ask.verb.clone()).on_press(Msg::Press(Focus::Allow));
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
            let (icon, tone) = match &ask.app {
                Some(_) => (Icon::AppWindow, Tone::Secondary),
                None => (Icon::TriangleAlert, Tone::Warning),
            };
            Node::Icon {
                icon,
                role: TextRole::Display,
                tone,
            }
        }
    };
    slopos_appkit::Dialog::new(ask.headline())
        .icon(mark)
        .content(column(space::MD, content))
        .footer(footer)
        .anchor(anchor)
        .into()
}

/// The icon at `path`, which privd named from a share no process writes.
fn load_icon(path: &str) -> Option<ImageData> {
    let image = crate::app_entry::read_icon(std::path::Path::new(path))?;
    ImageData::new(image.width, image.height, image.pixels)
}

impl Consent {
    /// The end init handed over at [`COMPOSITOR_CONSENT`]: either end of a
    /// socket pair names its maker, so no other process can hand over one
    /// that passes.
    pub fn acquire() -> Consent {
        let fd = Sys::fcntl(COMPOSITOR_CONSENT, F_GETFD as i32, 0)
            .ok()
            .map(|_| {
                let _ = fs::set_fd_cloexec(COMPOSITOR_CONSENT);
                // SAFETY: init handed this descriptor over and nothing else here
                // owns it.
                unsafe { OwnedFd::from_raw(COMPOSITOR_CONSENT) }
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
            overlay: Overlay::new(),
            icon: None,
            placed: None,
            drawing: false,
            offered: false,
            quiet_until: None,
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

    fn offer(&mut self) {
        if !self.drawing || self.offered || self.showing.is_some() {
            return;
        }
        self.offered = true;
        if !self.write(&protocol::ready_frame()) {
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

    /// `false` once the channel is gone.
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
                        self.overlay.changed();
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
        self.overlay.hide();
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

    /// Whether a press is the dialog's: while it is
    /// [`capturing`](Self::capturing), or a repeat of a key pressed into it. A
    /// fresh press of a key it took is a client's, release and all.
    pub fn takes_press(&mut self, event: &InputEvent) -> bool {
        if self.capturing() || self.overlay.taken.repeats(event) {
            self.key(event);
            return true;
        }
        false
    }

    pub fn active(&self) -> bool {
        self.showing.is_some()
    }

    pub fn asker(&self) -> Option<u32> {
        self.showing.as_ref().map(|dialog| dialog.ask().pid)
    }

    /// What to repaint since the last frame, the dialog laid out over
    /// `anchor` the first time. A screen that changed moves it and input
    /// waits again; privd hears when a question first goes on screen.
    pub fn take_damage(
        &mut self,
        screen: Rect,
        anchor: Option<Rect>,
        pointer: (i32, i32),
    ) -> impl Iterator<Item = DamageRect> + use<> {
        let Some(dialog) = self.showing.as_mut() else {
            return self.overlay.take_damage();
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
            log!("shown a question from {}", dialog.ask().app_name());
            let id = dialog.ask().id;
            if !self.write(&protocol::shown_frame(id)) {
                self.close();
                return self.overlay.take_damage();
            }
        }
        if self.overlay.stale() || self.placed != Some(placed) {
            self.lay_out(placed, pointer);
        }
        self.overlay.take_damage()
    }

    fn lay_out(&mut self, (screen, anchor): (Rect, Option<Rect>), pointer: (i32, i32)) {
        let Some(dialog) = &self.showing else {
            return;
        };
        let node = view(dialog, self.icon.as_ref(), anchor);
        let focus = match dialog.focus() {
            Focus::Field => (Role::TextField, words::PASSWORD),
            Focus::Details if dialog.expanded() => (Role::Button, words::SHOW_LESS),
            Focus::Details => (Role::Button, words::SHOW_DETAILS),
            Focus::Deny => (Role::Button, words::CANCEL),
            Focus::Allow => (Role::Button, dialog.ask().verb.as_str()),
        };
        self.overlay.show(&node, screen, pointer, Some(focus));
        self.placed = Some((screen, anchor));
    }

    /// Any release but of a key it took reaches the client that saw the
    /// press, question or not, so nothing stays held down behind one.
    pub fn takes_release(&mut self, keycode: u16) -> bool {
        self.overlay.taken.release(keycode)
    }

    fn key(&mut self, event: &InputEvent) {
        self.overlay.taken.take(event);
        let now = sys_core::get_time_ms();
        let Some(dialog) = self.showing.as_mut() else {
            if self.quiet(now) {
                self.quiet_until = Some(now + QUIET_MS);
            }
            return;
        };
        let key = match Typed::read(event) {
            Some(Typed::Move { back }) => Key::Move { back },
            Some(Typed::Arrow { right }) => Key::Arrow { right },
            Some(Typed::Choose { next }) => Key::Choose { next },
            Some(Typed::Press) => Key::Press,
            Some(Typed::Dismiss) => Key::Dismiss,
            Some(Typed::Erase) => Key::Erase,
            Some(Typed::Text(ch)) => Key::Text(ch),
            None => return,
        };
        let answer = dialog.key(key, now);
        self.overlay.changed();
        if let Some(answer) = answer {
            self.answer(answer);
        }
    }

    /// What the control at `(x, y)` answers, for a press made while the
    /// dialog was armed.
    pub fn click(&mut self, x: i32, y: i32, pressed: bool) {
        let Some(dialog) = self.showing.as_mut() else {
            return;
        };
        let now = sys_core::get_time_ms();
        if pressed && !dialog.pointer_down(now) {
            return;
        }
        let sent: Vec<Msg> = self.overlay.click(x, y, pressed);
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
            self.overlay.changed();
            if let Some(answer) = answer {
                self.answer(answer);
            }
        }
    }

    /// The primary button went up over the lock or login screen, which hides
    /// the question: whatever it pressed answers nothing.
    pub fn release_unseen(&mut self) {
        self.overlay.release_unseen();
    }

    pub fn wheel(&mut self, x: i32, y: i32, value_v120: i32) {
        let _: Vec<Msg> = self.overlay.wheel(x, y, value_v120);
    }

    pub fn hover(&mut self, x: i32, y: i32) {
        self.overlay.hover(x, y);
    }

    fn answer(&mut self, answer: protocol::Answer) {
        if self.showing.take().is_none() {
            return;
        }
        self.overlay.hide();
        let mut frame = answer.frame();
        drop(answer);
        if !self.write(&frame) {
            self.fd = None;
        }
        protocol::wipe(&mut frame);
        self.offer();
    }

    pub fn draw(&self, buf: &mut DrawBuffer, clip: &DamageRect) {
        self.overlay.draw(buf, clip);
    }
}
