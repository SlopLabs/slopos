//! The system bar's status items: their cached content and their drawing.
//!
//! Each item owns the state it renders from, reports its own measured width,
//! and bumps a revision when its content changes; [`slopos_chrome_core::status`]
//! places them and the bar repaints exactly the ones whose revision moved.
//!
//! Index order is right-to-left: [`CLOCK_INDEX`] is 0 and therefore rightmost.

use std::sync::OnceLock;

use slopos_appkit::text::{icons, ui};
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::{
    NetIndicatorState, StatusItemSpec, StatusKind, StatusSlot, indicator_for,
};
use slopos_gfx::canvas_ops::rounded_rect_filled;
use slopos_theme_core::TextRole;

use crate::gfx::{DamageRect, DrawBuffer};
use crate::theme::*;

pub const CLOCK_INDEX: usize = 0;
pub const NETWORK_INDEX: usize = 1;
pub const STATUS_ITEM_COUNT: usize = 2;

const HOVER_PAD_X: i32 = 4;
const HOVER_INSET_Y: i32 = 2;
const HOVER_RADIUS: i32 = 4;

const ICON: i32 = TextRole::Label.style().icon;

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

/// How wide `HH:MM` is at its widest, so the clock and the items beside it
/// hold still as the minutes pass.
fn widest_time() -> i32 {
    static WIDEST: OnceLock<i32> = OnceLock::new();
    *WIDEST.get_or_init(|| {
        (0..24 * 60)
            .map(|minute| {
                ui::width(
                    &format!("{:02}:{:02}", minute / 60, minute % 60),
                    BAR_TEXT_PX,
                )
            })
            .max()
            .unwrap_or(0)
    })
}

pub struct StatusItems {
    specs: [StatusItemSpec; STATUS_ITEM_COUNT],
    /// The time the clock shows, `HH:MM`, or nothing while the wall clock is
    /// not known.
    clock: String,
    net: NetIndicatorState,
}

impl StatusItems {
    pub fn new() -> Self {
        Self {
            specs: [
                StatusItemSpec::hidden(StatusKind::Clock),
                StatusItemSpec::hidden(StatusKind::Network),
            ],
            clock: String::new(),
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

    /// The revision moves only if the shown time differs, so the bar
    /// repaints once a minute rather than every frame.
    pub fn set_time(&mut self, now: Option<&WallTime>) {
        let text = now.map(WallTime::time).unwrap_or_default();
        if text == self.clock {
            return;
        }
        self.clock = text;
        let spec = &mut self.specs[CLOCK_INDEX];
        spec.present = !self.clock.is_empty();
        spec.width = widest_time();
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
        spec.width = ICON;
        spec.revision = spec.revision.wrapping_add(1);
    }

    pub fn draw(&self, buf: &mut DrawBuffer, slot: &StatusSlot, hovered: bool, clip: &DamageRect) {
        if hovered {
            let r = hover_rect(slot);
            buf.with_scissor(*clip, |buf| {
                rounded_rect_filled(
                    buf,
                    r.x0,
                    r.y0,
                    r.x1 - r.x0 + 1,
                    r.y1 - r.y0 + 1,
                    HOVER_RADIUS,
                    BAR_HOVER_BG,
                );
            });
        }
        // Text is blended towards the background it is given, so hovered text
        // must blend towards the highlight or its edges punch the backdrop
        // back out.
        let text_bg = if hovered { BAR_HOVER_BG } else { BAR_INK_BG };

        match slot.kind {
            StatusKind::Clock => ui::draw_clipped(
                buf,
                slot.x + slot.w - ui::width(&self.clock, BAR_TEXT_PX),
                ui::centred_top(0, SYSTEM_BAR_HEIGHT, BAR_TEXT_PX),
                &self.clock,
                BAR_TEXT_PX,
                ui::Weight::Regular,
                TEXT_PRIMARY,
                text_bg,
                clip,
            ),
            StatusKind::Network => {
                let (icon, tone) = indicator_for(self.net);
                let ink = slopos_theme_core::current().ink(tone);
                let y = (SYSTEM_BAR_HEIGHT - ICON) / 2;
                icons::draw_clipped(buf, slot.x, y, icon, ICON, ink, clip);
            }
        }
    }
}
