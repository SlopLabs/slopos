pub mod avatar;
pub mod button;
pub mod button_row;
pub mod card;
pub mod checkbox;
pub mod code_view;
pub mod dialog;
pub mod disclosure;
pub mod drag_handle;
pub mod editor_tabs;
pub mod icon;
pub mod icon_button;
pub mod image;
pub mod line_edit;
pub mod list;
pub mod menu;
pub mod menu_bar;
pub mod partition_bar;
pub mod popup;
pub mod progress_bar;
pub mod property_list;
pub mod scroll_view;
mod scrollbar;
pub mod separator;
pub mod slider;
pub mod tab_bar;
pub mod table;
pub mod tag;
pub mod text;
pub mod tree_view;

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, State, radius, stroke};

use crate::constraints::Rect;
use crate::paint::PaintContext;

/// The bar that marks the chosen row or tab, as thick as a focused edge.
const INDICATOR: i32 = stroke::FOCUS;

const CARET: i32 = 2;

/// A field's sunken fill, lit under the pointer, inside an edge of `edge`,
/// thicker while focused.
pub(crate) fn field_frame(
    ctx: &mut PaintContext,
    rect: Rect,
    focused: bool,
    lit: bool,
    edge: Color32,
) {
    let width = if focused { stroke::FOCUS } else { stroke::EDGE };
    let r = radius::CONTROL;
    let state = if lit { State::Hover } else { State::Normal };
    let sunken = ctx.theme().state(ColorRole::SurfaceSunken, state);
    ctx.fill_rounded_rect(rect, r, sunken);
    ctx.draw_rounded_rect(rect, r, width, edge);
}

/// A row of a list, a table, a tree or a menu.
#[derive(Clone, Copy, Default)]
pub(crate) struct RowState {
    pub selected: bool,
    pub hovered: bool,
    /// Its view has focus.
    pub focused: bool,
    /// The keyboard is on it.
    pub cursor: bool,
}

/// What `state` paints under the row at `rect`: the selection's wash and
/// edge, accent while its view has focus, or the hover's wash; and the focus
/// ring on the row the keyboard is on.
pub(crate) fn paint_row(ctx: &mut PaintContext, rect: Rect, state: RowState) {
    let theme = ctx.theme();
    let (x, y, w, h) = (rect.x, rect.y, rect.width, rect.height);
    if state.selected {
        ctx.fill_rect_blended(x, y, w, h, theme.selection);
        let edge = match state.focused {
            true => theme.accent,
            false => theme.text_disabled,
        };
        ctx.fill_rect(x, y, INDICATOR, h, edge);
    } else if state.hovered {
        ctx.fill_rect_blended(x, y, w, h, theme.overlay(State::Hover));
    }
    if state.focused && state.cursor {
        ctx.draw_inner_focus_ring(rect);
    }
}

/// The nearest of `len` items past `from` the arrows reach, `enabled` saying
/// which may be chosen: the first or the last with no `from`, and none past
/// either end.
pub(crate) fn next_enabled(
    len: usize,
    from: Option<usize>,
    forward: bool,
    enabled: impl Fn(usize) -> bool,
) -> Option<usize> {
    if forward {
        (from.map_or(0, |f| f + 1)..len).find(|&i| enabled(i))
    } else {
        (0..from.map_or(len, |f| f.min(len)))
            .rev()
            .find(|&i| enabled(i))
    }
}

/// The filled dot that marks unsaved changes, centred in the `size` box at
/// `(x, y)`.
pub(crate) fn modified_dot(ctx: &mut PaintContext, x: i32, y: i32, size: i32, color: Color32) {
    let r = (size / 3).max(2);
    ctx.fill_circle(x + size / 2 - r, y + size / 2 - r, r * 2, color);
}
