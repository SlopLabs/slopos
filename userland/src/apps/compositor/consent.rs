//! privd's questions to the person at the machine, drawn over every surface.
//!
//! The channel is the socket pair init made, so an answer here is the
//! compositor's and nothing else's; the compositor owns both seats, so the
//! answer is the keyboard's or the pointer's. While a question is up, no key
//! or button pressed reaches a client and every surface is dimmed beneath it,
//! so a password typed into it reaches privd alone.

use std::string::String;

use slopos_abi::InputEvent;
use slopos_abi::draw::Color32;
use slopos_abi::input::keycode::{
    KEY_BACKSPACE, KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_KP_ENTER, KEY_LEFT, KEY_RIGHT, KEY_TAB, KEY_UP,
};
use slopos_abi::syscall::{F_GETFD, MSG_NOSIGNAL};
use slopos_appkit::text::ui;
use slopos_gfx::blend::fill_rect_blended;
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_priv_core::INIT;
use slopos_priv_core::consent::{self as protocol, Ask, Dialog, Focus, Inbound, Key, Link, kind};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use crate::apps::privd::CONSENT_FD;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::syscall::{OwnedFd, RawFd, SyscallError, core as sys_core, fs, net};
use crate::theme::*;

const CARD_W: i32 = 560;
const BUTTON_W: i32 = 96;
const LEADING: i32 = 2;
/// More keys than a hand holds down at once.
const TAKEN_MAX: usize = 16;
/// How long after its last key a password question that went unanswered
/// keeps taking keys, so the rest of a password typed into it reaches no
/// client.
const QUIET_MS: u64 = 1500;

pub struct Consent {
    fd: Option<OwnedFd>,
    frames: Deframer,
    link: Link,
    showing: Option<Dialog>,
    /// The compositor can draw a question.
    drawing: bool,
    /// privd may send a question: a `READY` went since the last one came.
    offered: bool,
    dirty: bool,
    /// Keys pressed into the dialog, whose releases no client may see: a
    /// release names its key, and so would spell a password typed there.
    taken: Vec<u16>,
    quiet_until: Option<u64>,
    /// The version privd reads, which this side names in `READY`.
    speaks: u32,
}

/// The version the running base's privd reads.
fn privd_reads() -> u32 {
    let interface = std::fs::read(slopos_boot_core::base::INTERFACE_PATH)
        .ok()
        .and_then(|text| slopos_boot_core::base::parse_interface(&text).ok());
    match interface {
        Some(n) if n >= protocol::SINCE_INTERFACE => protocol::VERSION,
        _ => 1,
    }
}

struct Layout {
    card: DamageRect,
    deny: DamageRect,
    allow: DamageRect,
    field: Option<DamageRect>,
    lines: Vec<(String, Color32, i32)>,
}

fn rect(x: i32, y: i32, w: i32, h: i32) -> DamageRect {
    DamageRect {
        x0: x,
        y0: y,
        x1: x + w - 1,
        y1: y + h - 1,
    }
}

fn contains(r: &DamageRect, x: i32, y: i32) -> bool {
    x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1
}

/// `text` in lines no wider than `max_w`, every character kept: what the
/// person is asked about is never cut short.
fn wrap(text: &str, max_w: i32) -> Vec<String> {
    let mut lines = vec![String::new()];
    for ch in text.chars() {
        let line = lines.last_mut().expect("never empty");
        line.push(ch);
        if line.chars().count() > 1 && ui::width(line, TEXT_PX) > max_w {
            line.pop();
            lines.push(ch.to_string());
        }
    }
    lines
}

fn layout(dialog: &Dialog, screen_w: u32, screen_h: u32) -> Layout {
    let line = ui::line_height(TEXT_PX);
    let w = CARD_W.min(screen_w as i32 - 2 * CARD_PAD);
    let text_w = w - 2 * CARD_PAD;
    let mut paragraphs = vec![
        (format!("Allow: {}?", dialog.ask().describe), TEXT_PRIMARY),
        (format!("Asked by {}", dialog.requester()), TEXT_SECONDARY),
        (
            format!("Runs {}", dialog.ask().command_line()),
            TEXT_SECONDARY,
        ),
        (format!("In {}", dialog.ask().directory()), TEXT_SECONDARY),
    ];
    paragraphs.extend(dialog.prompt().map(|prompt| (prompt, TEXT_PRIMARY)));
    if let Some((admin, count)) = dialog.admin() {
        let choose = match count {
            1 => String::new(),
            n => format!(", one of {n}: Up and Down choose"),
        };
        paragraphs.push((format!("Administrator: {admin}{choose}"), TEXT_PRIMARY));
    }
    let mut lines = Vec::new();
    let mut y = CARD_PAD;
    for (i, (text, colour)) in paragraphs.into_iter().enumerate() {
        if i > 0 {
            y += CARD_GAP - LEADING;
        }
        for piece in wrap(&text, text_w) {
            lines.push((piece, colour, y));
            y += line + LEADING;
        }
    }
    let field_y = y - LEADING + CARD_GAP;
    if dialog.ask().kind.takes_secret() {
        y = field_y + CONTROL_H + LEADING;
    }
    let h = y - LEADING + CARD_PAD + CONTROL_H + CARD_PAD;
    let x = (screen_w as i32 - w) / 2;
    let top = ((screen_h as i32 - h) / 2).max(0);
    for (_, _, line_y) in &mut lines {
        *line_y += top;
    }
    let buttons_y = top + h - CARD_PAD - CONTROL_H;
    let allow_x = x + w - CARD_PAD - BUTTON_W;
    Layout {
        card: rect(x, top, w, h),
        allow: rect(allow_x, buttons_y, BUTTON_W, CONTROL_H),
        deny: rect(
            allow_x - CARD_GAP - BUTTON_W,
            buttons_y,
            BUTTON_W,
            CONTROL_H,
        ),
        field: dialog
            .ask()
            .kind
            .takes_secret()
            .then(|| rect(x + CARD_PAD, top + field_y, text_w, CONTROL_H)),
        lines,
    }
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
                        self.showing = Some(Dialog::new(ask));
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

    /// Whether the dialog appeared, changed or went since the last frame,
    /// which the frame about to be composed shows. privd hears when a
    /// question first goes on screen.
    pub fn take_dirty(&mut self) -> bool {
        if let Some(dialog) = self.showing.as_mut()
            && dialog.drawn(sys_core::get_time_ms())
            && protocol::says_shown(self.speaks)
        {
            let id = dialog.ask().id;
            if !self.write(&protocol::shown_frame(id)) {
                self.close();
            }
        }
        std::mem::take(&mut self.dirty)
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
        self.dirty = true;
        if let Some(answer) = answer {
            self.answer(answer);
        }
    }

    pub fn press(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) {
        let Some(dialog) = self.showing.as_mut() else {
            return;
        };
        let at = layout(dialog, screen_w, screen_h);
        let target = if contains(&at.allow, x, y) {
            Focus::Allow
        } else if contains(&at.deny, x, y) {
            Focus::Deny
        } else if at.field.as_ref().is_some_and(|f| contains(f, x, y)) {
            Focus::Field
        } else {
            return;
        };
        let focus = dialog.focus();
        let answer = dialog.click(target, sys_core::get_time_ms());
        self.dirty |= dialog.focus() != focus;
        if let Some(answer) = answer {
            self.answer(answer);
        }
    }

    fn answer(&mut self, answer: protocol::Answer) {
        if self.showing.take().is_none() {
            return;
        }
        self.dirty = true;
        let mut frame = answer.frame();
        drop(answer);
        if !self.write(&frame) {
            self.fd = None;
        }
        protocol::wipe(&mut frame);
        self.offer();
    }

    /// Dim `clip` and draw the question over it.
    pub fn draw(&self, buf: &mut DrawBuffer, screen_w: u32, screen_h: u32, clip: &DamageRect) {
        let Some(dialog) = &self.showing else {
            return;
        };
        fill_rect_blended(
            buf,
            clip.x0,
            clip.y0,
            clip.x1 - clip.x0 + 1,
            clip.y1 - clip.y0 + 1,
            SCRIM,
        );
        let at = layout(dialog, screen_w, screen_h);
        let card = &at.card;
        let (w, h) = (card.x1 - card.x0 + 1, card.y1 - card.y0 + 1);
        rounded_rect_filled(buf, card.x0, card.y0, w, h, CARD_RADIUS, CARD_EDGE);
        rounded_rect_filled(
            buf,
            card.x0 + EDGE_W,
            card.y0 + EDGE_W,
            w - 2 * EDGE_W,
            h - 2 * EDGE_W,
            CARD_RADIUS - EDGE_W,
            CARD_BG,
        );
        for (text, colour, y) in &at.lines {
            ui::draw_clipped(
                buf,
                card.x0 + CARD_PAD,
                *y,
                text,
                TEXT_PX,
                ui::Weight::Regular,
                *colour,
                CARD_BG,
                clip,
            );
        }
        if let Some(field) = &at.field {
            let (w, h) = (field.x1 - field.x0 + 1, field.y1 - field.y0 + 1);
            let edge = match dialog.focus() {
                Focus::Field => FOCUS,
                _ => FIELD_EDGE,
            };
            rounded_rect_filled(buf, field.x0, field.y0, w, h, FIELD_RADIUS, edge);
            rounded_rect_filled(
                buf,
                field.x0 + EDGE_W,
                field.y0 + EDGE_W,
                w - 2 * EDGE_W,
                h - 2 * EDGE_W,
                FIELD_RADIUS - EDGE_W,
                FIELD_BG,
            );
            let room = (w - 2 * CARD_PAD) / ui::width(ui::BULLET, TEXT_PX).max(1);
            let masked = ui::BULLET.repeat(dialog.typed_chars().min(room.max(0) as usize));
            let text_y = ui::centred_top(field.y0, h, TEXT_PX);
            ui::draw_clipped(
                buf,
                field.x0 + CARD_PAD,
                text_y,
                &masked,
                TEXT_PX,
                ui::Weight::Regular,
                TEXT_PRIMARY,
                FIELD_BG,
                clip,
            );
        }
        for (button, label, r) in [
            (Focus::Deny, "Deny", &at.deny),
            (Focus::Allow, "Allow", &at.allow),
        ] {
            let focused = dialog.focus() == button;
            let (edge, fill, ink) = if focused {
                (ACCENT, ACCENT, TEXT_ON_ACCENT)
            } else {
                (FIELD_EDGE, CARD_BG, TEXT_PRIMARY)
            };
            let (w, h) = (r.x1 - r.x0 + 1, r.y1 - r.y0 + 1);
            rounded_rect_filled(buf, r.x0, r.y0, w, h, CONTROL_H / 2, edge);
            rounded_rect_filled(
                buf,
                r.x0 + EDGE_W,
                r.y0 + EDGE_W,
                w - 2 * EDGE_W,
                h - 2 * EDGE_W,
                CONTROL_H / 2 - EDGE_W,
                fill,
            );
            let label_x = r.x0 + (BUTTON_W - ui::width(label, TEXT_PX)) / 2;
            let label_y = ui::centred_top(r.y0, CONTROL_H, TEXT_PX);
            ui::draw_clipped(
                buf,
                label_x,
                label_y,
                label,
                TEXT_PX,
                ui::Weight::Regular,
                ink,
                fill,
                clip,
            );
        }
    }
}
