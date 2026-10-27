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
pub mod list_view;
pub mod menu;
pub mod menu_bar;
pub mod partition_bar;
pub mod popup;
pub mod progress_bar;
pub mod property_list;
pub mod radio_group;
pub mod scroll_view;
pub mod separator;
pub mod slider;
pub mod tab_bar;
pub mod table;
pub mod tag;
pub mod text;
pub mod text_field;
pub mod tree_view;

use slopos_abi::draw::Color32;
use slopos_theme_core::{radius, stroke};

use crate::constraints::Rect;
use crate::paint::PaintContext;

const SELECTED_EDGE: i32 = 2;

/// A field's sunken fill inside an edge of `edge`, thicker while focused.
pub(crate) fn field_frame(ctx: &mut PaintContext, rect: Rect, focused: bool, edge: Color32) {
    let w = if focused { stroke::FOCUS } else { stroke::EDGE };
    let r = radius::CONTROL;
    ctx.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, r, edge);
    ctx.fill_rounded_rect(
        rect.x + w,
        rect.y + w,
        rect.width - 2 * w,
        rect.height - 2 * w,
        r - w,
        ctx.theme().surface_sunken,
    );
}

/// The edge a selected row carries beside its wash, accent while its list
/// has focus.
pub(crate) fn selected_edge(ctx: &mut PaintContext, x: i32, y: i32, height: i32, focused: bool) {
    let theme = ctx.theme();
    let edge = if focused {
        theme.accent
    } else {
        theme.text_disabled
    };
    ctx.fill_rect(x, y, SELECTED_EDGE, height, edge);
}

/// The filled dot that marks unsaved changes, centred in the `size` box at
/// `(x, y)`.
pub(crate) fn modified_dot(ctx: &mut PaintContext, x: i32, y: i32, size: i32, color: Color32) {
    let r = (size / 3).max(2);
    ctx.fill_rounded_rect(x + size / 2 - r, y + size / 2 - r, r * 2, r * 2, r, color);
}
