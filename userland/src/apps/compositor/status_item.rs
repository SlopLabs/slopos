//! The menu bar's status items, each moving its revision when what it shows
//! changes so the bar repaints only those; index 0, the clock, is rightmost.

use std::sync::OnceLock;

use slopos_appkit::text::{icons, ui};
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::indicator::indicator_for;
use slopos_chrome_core::netstate::NetIndicatorState;
use slopos_chrome_core::status::{BAR_HEIGHT, StatusItemSpec, StatusKind, StatusSlot};
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{ColorRole, State, TextRole, radius, space};

use crate::gfx::{DamageRect, DrawBuffer};

const CLOCK_INDEX: usize = 0;
const NETWORK_INDEX: usize = 1;
pub const STATUS_ITEM_COUNT: usize = 2;

const ROLE: TextRole = TextRole::Label;
const ICON: i32 = ROLE.style().icon;

/// Wider than the slot, so the highlight reads as the item lighting up
/// rather than as a box drawn round its text.
pub fn hover_rect(slot: &StatusSlot) -> DamageRect {
    const INSET_Y: i32 = 2;
    DamageRect {
        x0: slot.x - space::XS,
        y0: INSET_Y,
        x1: slot.x + slot.w - 1 + space::XS,
        y1: BAR_HEIGHT - 1 - INSET_Y,
    }
}

/// `HH:MM` at its widest, so the clock and the items beside it hold still
/// as the minutes pass.
fn widest_time() -> i32 {
    static WIDEST: OnceLock<i32> = OnceLock::new();
    *WIDEST.get_or_init(|| {
        (0..24 * 60)
            .map(|minute| ui::width(&format!("{:02}:{:02}", minute / 60, minute % 60), ROLE))
            .max()
            .unwrap_or(0)
    })
}

pub struct StatusItems {
    specs: [StatusItemSpec; STATUS_ITEM_COUNT],
    /// Empty while the wall clock is not known.
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
        self.specs.map(|spec| spec.revision)
    }

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
        let theme = slopos_theme_core::current();
        let hover = theme.state(ColorRole::Chrome, State::Hover);
        if hovered {
            let r = hover_rect(slot);
            let (w, h) = (r.x1 - r.x0 + 1, r.y1 - r.y0 + 1);
            Rounded::new(r.x0, r.y0, w, h, radius::CONTROL).draw(buf, clip, hover);
        }
        // Text is anti-aliased towards the colour it is given, which a
        // highlight must be or the text's edges punch it back out.
        let text_bg = if hovered { hover } else { theme.chrome };

        match slot.kind {
            StatusKind::Clock => ui::draw(
                buf,
                slot.x + slot.w - ui::width(&self.clock, ROLE),
                ui::centred_top(0, BAR_HEIGHT, ROLE),
                &self.clock,
                ROLE,
                theme.text,
                text_bg,
                clip,
            ),
            StatusKind::Network => {
                let (icon, tone) = indicator_for(self.net);
                let y = (BAR_HEIGHT - ICON) / 2;
                icons::draw_clipped(buf, slot.x, y, icon, ICON, theme.ink(tone), clip);
            }
        }
    }
}
