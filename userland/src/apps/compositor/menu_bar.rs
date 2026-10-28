//! The menu bar: the system menu's mark and the focused app's name on the
//! left, the status items on the right; the hover registry damages a hover.

use slopos_appkit::text::ui;
use slopos_chrome_core::clock::WallTime;
use slopos_chrome_core::status::{
    BAR_HEIGHT, BAR_PADDING_X, StatusLayout, hit_status_item, layout_status_items,
};
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{TextRole, space, with_alpha};

use super::hover::HOVER_STATUS_ITEM_BASE;
use super::popover::Panel;
use super::status_item::{STATUS_ITEM_COUNT, StatusItems, hover_rect};
use crate::gfx::{self, DamageRect, DrawBuffer};

const NAME: TextRole = TextRole::Heading;
const NAME_MAX_WIDTH: i32 = 200;
const MARK_SIZE: i32 = 8;
const MARK_GAP: i32 = space::SM;
/// Over the wallpaper, so a little of it shows through.
const OPACITY: u8 = 0xcc;

/// One per item, or the one spanning rect a change of layout gives.
pub const MAX_BAR_DAMAGE: usize = STATUS_ITEM_COUNT;

/// A cursor position no bar pixel can hold, for geometry-only layouts.
const NO_CURSOR: i32 = i32::MIN;

/// The mark's press target runs from the screen's edge to halfway to the
/// app's name.
const MARK_HIT_W: i32 = BAR_PADDING_X + MARK_SIZE + MARK_GAP / 2;
const NAME_X: i32 = BAR_PADDING_X + MARK_SIZE + MARK_GAP;

pub struct MenuBar {
    items: StatusItems,
    last_signature: u64,
    last_revision: [u32; STATUS_ITEM_COUNT],
    /// So a change of layout also repaints where the items were.
    last_leftmost: i32,
    last_screen_width: i32,
}

impl MenuBar {
    pub fn new() -> Self {
        Self {
            items: StatusItems::new(),
            last_signature: 0,
            last_revision: [0; STATUS_ITEM_COUNT],
            last_leftmost: i32::MAX,
            last_screen_width: 0,
        }
    }

    pub fn set_network(
        &mut self,
        present: bool,
        state: slopos_chrome_core::netstate::NetIndicatorState,
    ) {
        self.items.set_network(present, state);
    }

    fn layout(&self, screen_width: u32, cursor_x: i32, cursor_y: i32) -> StatusLayout {
        layout_status_items(self.items.specs(), screen_width as i32, cursor_x, cursor_y)
    }

    fn name_budget(layout: &StatusLayout) -> i32 {
        (layout.app_name_limit - NAME_X).min(NAME_MAX_WIDTH)
    }

    pub fn draw(
        &self,
        buf: &mut DrawBuffer,
        screen_width: u32,
        app_name: &str,
        cursor_x: i32,
        cursor_y: i32,
        clip: &DamageRect,
    ) {
        let theme = slopos_theme_core::current();
        let sw = screen_width as i32;
        let fill = with_alpha(theme.chrome, OPACITY);
        gfx::fill_rect_blended_clipped(buf, 0, 0, sw, BAR_HEIGHT, fill, clip);
        gfx::fill_rect_clipped(buf, 0, BAR_HEIGHT, sw, 1, theme.border, clip);

        let mark = Rounded::circle(BAR_PADDING_X, (BAR_HEIGHT - MARK_SIZE) / 2, MARK_SIZE);
        mark.draw(buf, clip, theme.accent);

        let layout = self.layout(screen_width, cursor_x, cursor_y);
        let budget = Self::name_budget(&layout);
        if budget > 0 && !app_name.is_empty() {
            // Its edges are anti-aliased towards the bar's own colour rather
            // than whatever the wallpaper put behind it.
            ui::draw_elided(
                buf,
                NAME_X,
                ui::centred_top(0, BAR_HEIGHT, NAME),
                app_name,
                NAME,
                budget,
                theme.text,
                theme.chrome,
                clip,
            );
        }

        for (index, slot) in layout.slots().iter().enumerate() {
            self.items
                .draw(buf, slot, layout.hovered == Some(index), clip);
        }
    }

    pub fn covers(py: i32) -> bool {
        (0..BAR_HEIGHT).contains(&py)
    }

    /// Which panel the item at `(px, py)` opens, laid out at that position.
    pub fn hit_test(&self, screen_width: u32, px: i32, py: i32) -> Option<Panel> {
        if Self::covers(py) && (0..MARK_HIT_W).contains(&px) {
            return Some(Panel::System);
        }
        let layout = self.layout(screen_width, px, py);
        hit_status_item(&layout, px, py).map(Panel::Status)
    }

    /// What `panel`'s popover hangs from.
    pub fn item_rect(&self, screen_width: u32, panel: Panel) -> Option<DamageRect> {
        let (x, w) = match panel {
            Panel::System => (BAR_PADDING_X, MARK_SIZE),
            Panel::Status(kind) => {
                let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
                let slot = layout.slot_for(kind)?;
                (slot.x, slot.w)
            }
        };
        Some(strip(x, x + w - 1))
    }

    /// One `(id, rect, hovered)` per placed item.
    pub fn hover_regions(
        &self,
        screen_width: u32,
        cursor_x: i32,
        cursor_y: i32,
        out: &mut [(u32, DamageRect, bool)],
    ) -> usize {
        let layout = self.layout(screen_width, cursor_x, cursor_y);
        let mut count = 0usize;
        for (index, slot) in layout.slots().iter().enumerate() {
            if count >= out.len() {
                break;
            }
            out[count] = (
                HOVER_STATUS_ITEM_BASE | slot.kind as u32,
                hover_rect(slot),
                layout.hovered == Some(index),
            );
            count += 1;
        }
        count
    }

    /// Where the mark and the app's name are drawn, which nothing else in a
    /// frame repaints when focus moves.
    pub fn app_name_damage(&self, screen_width: u32) -> DamageRect {
        let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
        strip(BAR_PADDING_X, NAME_X + Self::name_budget(&layout).max(0))
    }

    /// Show `now` on the clock and say what changed since the last call, in
    /// `out`, which holds at least [`MAX_BAR_DAMAGE`]. A change of layout
    /// moves everything left of it, so it repaints from the leftmost item,
    /// before or after, to the right edge; a change of content repaints the
    /// item alone.
    pub fn take_damage(
        &mut self,
        screen_width: u32,
        now: Option<&WallTime>,
        out: &mut [DamageRect],
    ) -> usize {
        self.items.set_time(now);

        let sw = screen_width as i32;
        let layout = self.layout(screen_width, NO_CURSOR, NO_CURSOR);
        let leftmost = layout
            .slots()
            .iter()
            .map(|slot| slot.x)
            .min()
            .unwrap_or(sw - BAR_PADDING_X);

        let mut count = 0usize;
        let mut push = |rect: DamageRect| {
            if count < out.len() {
                out[count] = rect;
                count += 1;
            }
        };
        if layout.signature != self.last_signature || sw != self.last_screen_width {
            push(strip(leftmost.min(self.last_leftmost).max(0), sw - 1));
        } else {
            let revisions = self.items.revisions();
            for slot in layout.slots() {
                if revisions[slot.idx] != self.last_revision[slot.idx] {
                    push(strip(slot.x, slot.x + slot.w - 1));
                }
            }
        }

        self.last_signature = layout.signature;
        self.last_screen_width = sw;
        self.last_leftmost = leftmost;
        self.last_revision = self.items.revisions();
        count
    }
}

/// The bar from `x0` to `x1`, its border row included.
fn strip(x0: i32, x1: i32) -> DamageRect {
    DamageRect {
        x0,
        y0: 0,
        x1,
        y1: BAR_HEIGHT,
    }
}
