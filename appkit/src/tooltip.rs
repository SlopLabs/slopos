//! The whole of a cut-short text, or the name of an icon, shown where the
//! pointer rests.

use slopos_theme_core::{TextRole, radius, space};

use crate::components::Wrap;
use crate::constraints::{Rect, Size};
use crate::paint::PaintContext;
use crate::traits::Widget;
use crate::widgets::card::draw_shadow;
use crate::widgets::text::break_lines;

/// How long the pointer rests before a tooltip shows.
pub(crate) const DELAY_MS: u64 = 600;

const ROLE: TextRole = TextRole::Label;
const MAX_WIDTH: i32 = 360;
const PAD: i32 = space::SM;

/// What the topmost widget under `(x, y)` that has a tooltip shows there,
/// unless a widget that covers what is beneath it lies between.
pub(crate) fn find(widget: &dyn Widget, x: i32, y: i32) -> Option<String> {
    answer(widget, x, y).flatten()
}

/// `Some` once a widget under the point has answered for it, if only to
/// cover what is beneath.
fn answer(widget: &dyn Widget, x: i32, y: i32) -> Option<Option<String>> {
    if !widget.layout_rect().contains(x, y) {
        return None;
    }
    let inner = widget
        .children()
        .iter()
        .rev()
        .find_map(|child| answer(child.as_ref(), x, y));
    inner.or_else(|| match widget.tooltip(x, y) {
        Some(tip) => Some(Some(String::from(tip))),
        None => widget.covers().then_some(None),
    })
}

/// Draws `text` beside the pointer at `at`, inside `window`.
pub(crate) fn paint(ctx: &mut PaintContext, text: &str, at: (i32, i32), window: Size) {
    let room = (window.width - PAD * 2).clamp(0, MAX_WIDTH - PAD * 2);
    let lines = break_lines(text, ROLE, Wrap::Words, Some(room));
    let lh = ROLE.style().line_height;
    let widest = lines
        .iter()
        .map(|&(s, e)| crate::text::ui::role_width(&text[s..e], ROLE))
        .max()
        .unwrap_or(0);
    let w = widest + PAD * 2;
    let h = lh * lines.len() as i32 + PAD;
    let below = at.1 + space::XL;
    let y = if below + h <= window.height {
        below
    } else {
        (at.1 - space::SM - h).max(0)
    };
    let x = (at.0 + space::MD).min(window.width - w).max(0);
    let card = Rect::new(x, y, w, h);
    let theme = ctx.theme();
    draw_shadow(ctx, card);
    ctx.fill_rounded_rect(x, y, w, h, radius::CONTROL, theme.surface_raised);
    ctx.draw_rounded_rect(x, y, w, h, radius::CONTROL, theme.border);
    let glyph = (lh - crate::text::ui::role_line_height(ROLE)) / 2;
    for (i, &(s, e)) in lines.iter().enumerate() {
        let top = y + PAD / 2 + i as i32 * lh + glyph;
        ctx.draw_text_fitted(x + PAD, top, &text[s..e], ROLE, room, theme.text);
    }
}
