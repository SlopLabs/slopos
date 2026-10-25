//! The system bar's status items: their cached content and their drawing.
//!
//! Each item owns the state it renders from, reports its own measured width,
//! and bumps a revision when its content changes; [`slopos_chrome_core::status`]
//! places them and the bar repaints exactly the ones whose revision moved.
//!
//! Index order is right-to-left: [`CLOCK_INDEX`] is 0 and therefore rightmost.

use slopos_appkit::text::ui;
use slopos_chrome_core::{NetIndicatorState, StatusItemSpec, StatusKind, StatusSlot, glyph_for};
use slopos_gfx::canvas_ops::rounded_rect_filled;

use super::net_glyph;
use crate::gfx::{DamageRect, DrawBuffer};
use crate::theme::*;

pub const CLOCK_INDEX: usize = 0;
pub const NETWORK_INDEX: usize = 1;
pub const STATUS_ITEM_COUNT: usize = 2;

const HOVER_PAD_X: i32 = 4;
const HOVER_INSET_Y: i32 = 2;
const HOVER_RADIUS: i32 = 4;

const GLYPH_Y: i32 = (SYSTEM_BAR_HEIGHT - net_glyph::glyph_height(STATUS_GLYPH_SCALE)) / 2;

/// Wider than the slot on purpose: a highlight flush against the text reads as
/// a box drawn round it rather than as the item lighting up.
pub fn hover_rect(slot: &StatusSlot) -> DamageRect {
    DamageRect {
        x0: slot.x - HOVER_PAD_X,
        y0: HOVER_INSET_Y,
        x1: slot.x + slot.w - 1 + HOVER_PAD_X,
        y1: SYSTEM_BAR_HEIGHT - 1 - HOVER_INSET_Y,
    }
}

pub struct StatusItems {
    specs: [StatusItemSpec; STATUS_ITEM_COUNT],
    /// Rendered clock text, `HH:MM:SS`.
    clock: [u8; 8],
    net: NetIndicatorState,
}

impl StatusItems {
    pub fn new() -> Self {
        Self {
            specs: [
                StatusItemSpec::hidden(StatusKind::Clock),
                StatusItemSpec::hidden(StatusKind::Network),
            ],
            clock: [0u8; 8],
            net: NetIndicatorState::Disconnected,
        }
    }

    pub fn specs(&self) -> &[StatusItemSpec] {
        &self.specs
    }

    pub fn revisions(&self) -> [u32; STATUS_ITEM_COUNT] {
        let mut out = [0u32; STATUS_ITEM_COUNT];
        for (slot, spec) in out.iter_mut().zip(self.specs.iter()) {
            *slot = spec.revision;
        }
        out
    }

    /// The revision moves only if the rendered text differs, so the bar
    /// repaints once a second rather than every frame.
    pub fn set_uptime(&mut self, secs: u64) {
        let text = format_clock(secs);
        if text == self.clock {
            return;
        }
        self.clock = text;
        let width = ui::width(clock_str(&self.clock), BAR_TEXT_PX);
        let spec = &mut self.specs[CLOCK_INDEX];
        spec.present = true;
        spec.width = width;
        spec.revision = spec.revision.wrapping_add(1);
    }

    /// The revision moves only when the *drawn* state changes, so a poll that
    /// finds nothing new costs nothing.
    pub fn set_network(&mut self, present: bool, state: NetIndicatorState) {
        if self.specs[NETWORK_INDEX].present == present && self.net == state {
            return;
        }
        self.net = state;
        let spec = &mut self.specs[NETWORK_INDEX];
        spec.present = present;
        spec.width = net_glyph::glyph_width(STATUS_GLYPH_SCALE);
        spec.revision = spec.revision.wrapping_add(1);
    }

    pub fn draw(&self, buf: &mut DrawBuffer, slot: &StatusSlot, hovered: bool, clip: &DamageRect) {
        if hovered {
            let r = hover_rect(slot);
            rounded_rect_filled(
                buf,
                r.x0,
                r.y0,
                r.x1 - r.x0 + 1,
                r.y1 - r.y0 + 1,
                HOVER_RADIUS,
                BAR_HOVER_BG,
            );
        }
        // Text is blended towards the background it is given, so hovered text
        // must blend towards the highlight or its edges punch the backdrop
        // back out.
        let text_bg = if hovered { BAR_HOVER_BG } else { BAR_INK_BG };

        match slot.kind {
            StatusKind::Clock => ui::draw_clipped(
                buf,
                slot.x,
                ui::centred_top(0, SYSTEM_BAR_HEIGHT, BAR_TEXT_PX),
                clock_str(&self.clock),
                BAR_TEXT_PX,
                ui::Weight::Regular,
                TEXT_PRIMARY,
                text_bg,
                clip,
            ),
            StatusKind::Network => net_glyph::draw(
                buf,
                &glyph_for(self.net),
                slot.x,
                GLYPH_Y,
                STATUS_GLYPH_SCALE,
                clip,
            ),
        }
    }
}

/// Hours wrap at 100 so the field never widens and never moves the clock.
fn format_clock(secs: u64) -> [u8; 8] {
    let h = (secs / 3600) % 100;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    let mut buf = [0u8; 8];
    buf[0] = b'0' + (h / 10) as u8;
    buf[1] = b'0' + (h % 10) as u8;
    buf[2] = b':';
    buf[3] = b'0' + (m / 10) as u8;
    buf[4] = b'0' + (m % 10) as u8;
    buf[5] = b':';
    buf[6] = b'0' + (s / 10) as u8;
    buf[7] = b'0' + (s % 10) as u8;
    buf
}

/// Every byte [`format_clock`] writes is ASCII, so the fallback is unreachable.
fn clock_str(clock: &[u8; 8]) -> &str {
    core::str::from_utf8(clock).unwrap_or("00:00:00")
}
