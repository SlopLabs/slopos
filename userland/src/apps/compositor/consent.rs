//! privd's questions to the person at the machine, drawn over every surface.
//!
//! The channel is the socket pair init made, so an answer here is the
//! compositor's and nothing else's; the compositor owns both seats, so the
//! answer is the keyboard's or the pointer's. While a question is up, no
//! client sees input and every surface is dimmed beneath it.

use std::string::String;

use slopos_abi::draw::Color32;
use slopos_abi::input::keycode::{KEY_ENTER, KEY_ESC, KEY_KP_ENTER, KEY_LEFT, KEY_RIGHT, KEY_TAB};
use slopos_abi::syscall::{F_GETFD, MSG_NOSIGNAL};
use slopos_gfx::blend::fill_rect_blended;
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_priv_core::INIT;
use slopos_priv_core::consent::{self as protocol, Answer, Ask, Button, Dialog, Key, kind};
use slopos_remote_core::Deframer;
use slopos_slibc::pal::{Pal, Sys};

use crate::apps::privd::CONSENT_FD;
use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::{OwnedFd, RawFd, SyscallError, core as sys_core, fs, net};
use crate::theme::*;

const DIM: Color32 = Color32::new(0, 0, 0, 0xA0);
const CARD_W: i32 = 560;
const PAD: i32 = 16;
const GAP: i32 = 8;
const BUTTON_W: i32 = 96;
const BUTTON_H: i32 = 28;
const RADIUS: i32 = 12;
const BORDER: i32 = 1;
const LEADING: i32 = 2;

pub struct Consent {
    fd: Option<OwnedFd>,
    frames: Deframer,
    showing: Option<Dialog>,
    dirty: bool,
}

struct Layout {
    card: DamageRect,
    deny: DamageRect,
    allow: DamageRect,
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
        if line.chars().count() > 1 && gfx::font::string_width(line) > max_w {
            line.pop();
            lines.push(ch.to_string());
        }
    }
    lines
}

fn layout(dialog: &Dialog, screen_w: u32, screen_h: u32) -> Layout {
    let line = gfx::cell_height();
    let w = CARD_W.min(screen_w as i32 - 2 * PAD);
    let text_w = w - 2 * PAD;
    let paragraphs = [
        (format!("Allow: {}?", dialog.ask().describe), TEXT_PRIMARY),
        (format!("Asked by {}", dialog.requester()), TEXT_SECONDARY),
        (
            format!("Runs {}", dialog.ask().command_line()),
            TEXT_SECONDARY,
        ),
        (format!("In {}", dialog.ask().directory()), TEXT_SECONDARY),
    ];
    let mut lines = Vec::new();
    let mut y = PAD;
    for (i, (text, colour)) in paragraphs.into_iter().enumerate() {
        if i > 0 {
            y += GAP - LEADING;
        }
        for piece in wrap(&text, text_w) {
            lines.push((piece, colour, y));
            y += line + LEADING;
        }
    }
    let h = y - LEADING + PAD + BUTTON_H + PAD;
    let x = (screen_w as i32 - w) / 2;
    let top = ((screen_h as i32 - h) / 2).max(0);
    for (_, _, line_y) in &mut lines {
        *line_y += top;
    }
    let buttons_y = top + h - PAD - BUTTON_H;
    let allow_x = x + w - PAD - BUTTON_W;
    Layout {
        card: rect(x, top, w, h),
        allow: rect(allow_x, buttons_y, BUTTON_W, BUTTON_H),
        deny: rect(allow_x - GAP - BUTTON_W, buttons_y, BUTTON_W, BUTTON_H),
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
        Consent {
            fd,
            frames: slopos_priv_core::wire::deframer(),
            showing: None,
            dirty: false,
        }
    }

    pub fn fd(&self) -> Option<RawFd> {
        self.fd.as_ref().map(OwnedFd::raw)
    }

    pub fn announce(&mut self) {
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
            match self.frames.next_frame() {
                Ok(None) => return true,
                Ok(Some((kind::ASK, payload))) => match Ask::decode(&payload) {
                    Ok(ask) => {
                        self.showing = Some(Dialog::new(ask));
                        self.dirty = true;
                    }
                    Err(_) => return self.close(),
                },
                Ok(Some((kind::CANCEL, payload))) => {
                    let Ok(id) = protocol::decode_cancel(&payload) else {
                        return self.close();
                    };
                    if self.showing.as_ref().is_some_and(|d| d.ask().id == id) {
                        self.showing = None;
                        self.dirty = true;
                    }
                }
                Ok(Some(_)) | Err(_) => return self.close(),
            }
        }
    }

    fn close(&mut self) -> bool {
        self.fd = None;
        if self.showing.take().is_some() {
            self.dirty = true;
        }
        false
    }

    pub fn active(&self) -> bool {
        self.showing.is_some()
    }

    /// Whether the dialog appeared, changed or went since the last frame,
    /// which the frame about to be composed shows.
    pub fn take_dirty(&mut self) -> bool {
        if let Some(dialog) = &mut self.showing {
            dialog.drawn(sys_core::get_time_ms());
        }
        std::mem::take(&mut self.dirty)
    }

    pub fn key(&mut self, keycode: u16, repeat: bool) {
        if repeat {
            return;
        }
        let key = match keycode {
            KEY_TAB | KEY_LEFT | KEY_RIGHT => Key::Move,
            KEY_ENTER | KEY_KP_ENTER => Key::Press,
            KEY_ESC => Key::Dismiss,
            _ => return,
        };
        let Some(dialog) = self.showing.as_mut() else {
            return;
        };
        let focus = dialog.focus();
        let answer = dialog.key(key, sys_core::get_time_ms());
        self.dirty |= dialog.focus() != focus;
        if let Some(allow) = answer {
            self.answer(allow);
        }
    }

    pub fn press(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) {
        let Some(dialog) = &self.showing else {
            return;
        };
        let at = layout(dialog, screen_w, screen_h);
        let button = if contains(&at.allow, x, y) {
            Button::Allow
        } else if contains(&at.deny, x, y) {
            Button::Deny
        } else {
            return;
        };
        if let Some(allow) = dialog.click(button, sys_core::get_time_ms()) {
            self.answer(allow);
        }
    }

    fn answer(&mut self, allow: bool) {
        let Some(dialog) = self.showing.take() else {
            return;
        };
        self.dirty = true;
        let answer = Answer {
            id: dialog.ask().id,
            allow,
        };
        if !self.write(&answer.frame()) {
            self.fd = None;
        }
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
            DIM,
        );
        let at = layout(dialog, screen_w, screen_h);
        let card = &at.card;
        let (w, h) = (card.x1 - card.x0 + 1, card.y1 - card.y0 + 1);
        rounded_rect_filled(buf, card.x0, card.y0, w, h, RADIUS, SHELF_SEPARATOR);
        rounded_rect_filled(
            buf,
            card.x0 + BORDER,
            card.y0 + BORDER,
            w - 2 * BORDER,
            h - 2 * BORDER,
            RADIUS - BORDER,
            PANEL_BG_OPAQUE,
        );
        for (text, colour, y) in &at.lines {
            gfx::draw_str_clipped(buf, card.x0 + PAD, *y, text, *colour, PANEL_BG_OPAQUE, clip);
        }
        for (button, label, r) in [
            (Button::Deny, "Deny", &at.deny),
            (Button::Allow, "Allow", &at.allow),
        ] {
            let focused = dialog.focus() == button;
            let fill = if focused {
                SIGNAL_EXPAND
            } else {
                SIGNAL_INACTIVE
            };
            rounded_rect_filled(
                buf,
                r.x0,
                r.y0,
                r.x1 - r.x0 + 1,
                r.y1 - r.y0 + 1,
                BUTTON_H / 2,
                fill,
            );
            let label_x = r.x0 + (BUTTON_W - gfx::font::string_width(label)) / 2;
            let label_y = r.y0 + (BUTTON_H - gfx::cell_height()) / 2;
            gfx::draw_str_clipped(buf, label_x, label_y, label, TEXT_PRIMARY, fill, clip);
        }
    }
}
