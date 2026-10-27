//! Title bars, the signal buttons (close / minimize / expand) and the
//! hit-tests that map a cursor onto them.

use slopos_abi::damage::DamageRect;
use slopos_appkit::text::ui;
use slopos_gfx::canvas_ops::line_aa;
use slopos_gfx::rounded::Rounded;

use crate::gfx::{self, DrawBuffer};
use crate::theme;

/// Half-length of the close/expand cross/plus glyph arms.
const GLYPH_HALF: i32 = theme::SIGNAL_GLYPH_SIZE / 2;
const MINIMIZE_DASH_HALF: i32 = 4;

/// The title bar of the window whose frame is `outline`: its background,
/// rounded with the frame's top corners, its signal buttons and its title.
///
/// The buttons show their glyphs while `signal_hovered`, the cursor being
/// in their group, and are grey on a window without focus otherwise.
pub fn draw_window_decorations(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    title: &str,
    focused: bool,
    signal_hovered: bool,
    clip: &DamageRect,
) {
    draw_title_bar_background(buf, outline, clip);
    draw_signal_buttons(buf, outline.x, outline.y, focused, signal_hovered, clip);
    draw_title_text(
        buf,
        outline.x,
        outline.y,
        outline.w as u32,
        title,
        focused,
        clip,
    );
}

/// Hit-test the signal button group: `Some(0)` = close, `Some(1)` = minimize,
/// `Some(2)` = expand, `None` = miss.
pub fn hit_test_signal_button(window_x: i32, window_y: i32, px: i32, py: i32) -> Option<u8> {
    let buttons: [(i32, u8); 3] = [
        (window_x + theme::SIGNAL_BUTTON_1_CX, 0), // close
        (window_x + theme::SIGNAL_BUTTON_2_CX, 1), // minimize
        (window_x + theme::SIGNAL_BUTTON_3_CX, 2), // expand
    ];
    let cy = window_y + theme::SIGNAL_BUTTON_CY;

    for &(cx, id) in &buttons {
        let dx = px - cx;
        let dy = py - cy;
        if dx * dx + dy * dy <= theme::SIGNAL_BUTTON_RADIUS * theme::SIGNAL_BUTTON_RADIUS {
            return Some(id);
        }
    }
    None
}

pub fn hit_test_signal_group(window_x: i32, window_y: i32, px: i32, py: i32) -> bool {
    let gx = window_x + theme::SIGNAL_GROUP_X;
    let gy = window_y + theme::SIGNAL_GROUP_Y;
    px >= gx && px < gx + theme::SIGNAL_GROUP_W && py >= gy && py < gy + theme::SIGNAL_GROUP_H
}

/// Detect if `(px, py)` is in a resize grab zone: the shadow region outside the
/// window frame, with corners taking priority over edges.
///
/// `window_y` is the content top (kernel's `window.y`); the title bar extends
/// above it.
pub fn hit_test_resize_edge(
    window_x: i32,
    window_y: i32,
    window_w: u32,
    window_h: u32,
    px: i32,
    py: i32,
) -> super::input::ResizeEdge {
    use super::input::ResizeEdge;

    let ww = window_w as i32;
    let wh = window_h as i32;

    let frame_x0 = window_x;
    let frame_y0 = window_y - theme::TITLE_BAR_HEIGHT;
    let frame_x1 = window_x + ww - 1;
    let frame_y1 = window_y + wh - 1;

    let spread = theme::RESIZE_REACH;
    let ext_x0 = frame_x0 - spread;
    let ext_y0 = frame_y0 - spread;
    let ext_x1 = frame_x1 + spread;
    let ext_y1 = frame_y1 + spread;

    if px < ext_x0 || px > ext_x1 || py < ext_y0 || py > ext_y1 {
        return ResizeEdge::NONE;
    }

    if px >= frame_x0 && px <= frame_x1 && py >= frame_y0 && py <= frame_y1 {
        return ResizeEdge::NONE;
    }

    let frame_w = ww;
    let frame_h = wh + theme::TITLE_BAR_HEIGHT;
    let corner_range = theme::RESIZE_CORNER.min(frame_w / 2).min(frame_h / 2);

    let near_left = px < frame_x0 + corner_range;
    let near_right = px > frame_x1 - corner_range;
    let near_top = py < frame_y0 + corner_range;
    let near_bottom = py > frame_y1 - corner_range;

    if near_top && near_left {
        return ResizeEdge::TOP_LEFT;
    }
    if near_top && near_right {
        return ResizeEdge::TOP_RIGHT;
    }
    if near_bottom && near_left {
        return ResizeEdge::BOTTOM_LEFT;
    }
    if near_bottom && near_right {
        return ResizeEdge::BOTTOM_RIGHT;
    }

    if py < frame_y0 {
        return ResizeEdge::TOP;
    }
    if py > frame_y1 {
        return ResizeEdge::BOTTOM;
    }
    if px < frame_x0 {
        return ResizeEdge::LEFT;
    }
    if px > frame_x1 {
        return ResizeEdge::RIGHT;
    }

    ResizeEdge::NONE
}

fn draw_title_bar_background(buf: &mut DrawBuffer, outline: &Rounded, clip: &DamageRect) {
    let bg = theme::TITLE_BAR;
    let (x, y, w) = (outline.x, outline.y, outline.w);
    let r = outline.radius.clamp(0, theme::TITLE_BAR_HEIGHT.min(w / 2));
    gfx::fill_rect_clipped(buf, x + r, y, w - 2 * r, r, bg, clip);
    gfx::fill_rect_clipped(buf, x, y + r, w, theme::TITLE_BAR_HEIGHT - r, bg, clip);
    for corner in &outline.corners()[..2] {
        outline.fill(buf, corner, clip, |_, _| bg);
    }
}

fn draw_signal_buttons(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    focused: bool,
    signal_hovered: bool,
    clip: &DamageRect,
) {
    let cy = window_y + theme::SIGNAL_BUTTON_CY;
    let lit = focused || signal_hovered;
    let buttons = [
        (theme::SIGNAL_BUTTON_1_CX, theme::SIGNAL_CLOSE),
        (theme::SIGNAL_BUTTON_2_CX, theme::SIGNAL_MINIMIZE),
        (theme::SIGNAL_BUTTON_3_CX, theme::SIGNAL_EXPAND),
    ];
    let r = theme::SIGNAL_BUTTON_RADIUS;
    for (cx, color) in buttons {
        let color = if lit { color } else { theme::SIGNAL_INACTIVE };
        let circle = Rounded::circle(window_x + cx - r, cy - r, 2 * r + 1);
        circle.fill(buf, &circle.bounds(), clip, |_, _| color);
    }
    if signal_hovered {
        draw_signal_glyphs(buf, window_x, window_y, clip);
    }
}

fn draw_signal_glyphs(buf: &mut DrawBuffer, window_x: i32, window_y: i32, clip: &DamageRect) {
    let glyph_color = theme::SIGNAL_GLYPH;
    let cy = window_y + theme::SIGNAL_BUTTON_CY;

    let close_cx = window_x + theme::SIGNAL_BUTTON_1_CX;
    let close_rect = DamageRect {
        x0: close_cx - GLYPH_HALF,
        y0: cy - GLYPH_HALF,
        x1: close_cx + GLYPH_HALF,
        y1: cy + GLYPH_HALF,
    };
    if rects_intersect(&close_rect, clip) {
        line_aa(
            buf,
            close_cx - GLYPH_HALF,
            cy - GLYPH_HALF,
            close_cx + GLYPH_HALF,
            cy + GLYPH_HALF,
            glyph_color,
        );
        line_aa(
            buf,
            close_cx + GLYPH_HALF,
            cy - GLYPH_HALF,
            close_cx - GLYPH_HALF,
            cy + GLYPH_HALF,
            glyph_color,
        );
    }

    let min_cx = window_x + theme::SIGNAL_BUTTON_2_CX;
    let min_rect = DamageRect {
        x0: min_cx - MINIMIZE_DASH_HALF,
        y0: cy,
        x1: min_cx + MINIMIZE_DASH_HALF,
        y1: cy,
    };
    if rects_intersect(&min_rect, clip) {
        line_aa(
            buf,
            min_cx - MINIMIZE_DASH_HALF,
            cy,
            min_cx + MINIMIZE_DASH_HALF,
            cy,
            glyph_color,
        );
    }

    let exp_cx = window_x + theme::SIGNAL_BUTTON_3_CX;
    let exp_rect = DamageRect {
        x0: exp_cx - MINIMIZE_DASH_HALF,
        y0: cy - MINIMIZE_DASH_HALF,
        x1: exp_cx + MINIMIZE_DASH_HALF,
        y1: cy + MINIMIZE_DASH_HALF,
    };
    if rects_intersect(&exp_rect, clip) {
        line_aa(
            buf,
            exp_cx - MINIMIZE_DASH_HALF,
            cy,
            exp_cx + MINIMIZE_DASH_HALF,
            cy,
            glyph_color,
        );
        line_aa(
            buf,
            exp_cx,
            cy - MINIMIZE_DASH_HALF,
            exp_cx,
            cy + MINIMIZE_DASH_HALF,
            glyph_color,
        );
    }
}

fn draw_title_text(
    buf: &mut DrawBuffer,
    window_x: i32,
    window_y: i32,
    window_w: u32,
    title: &str,
    focused: bool,
    clip: &DamageRect,
) {
    if title.is_empty() {
        return;
    }

    let text_color = if focused {
        theme::TEXT_PRIMARY
    } else {
        theme::TEXT_SECONDARY
    };
    let bg_color = theme::TITLE_BAR;

    let max_text_w = (window_w as i32 - theme::TITLE_MAX_TEXT_WIDTH_MARGIN).max(0);
    if max_text_w == 0 {
        return;
    }

    let size = theme::TITLE_TEXT_PX;
    let weight = ui::Weight::Regular;
    let text_w = ui::elided_width(title, size, weight, max_text_w);
    ui::draw_elided(
        buf,
        window_x + (window_w as i32 - text_w) / 2,
        ui::centred_top(window_y, theme::TITLE_BAR_HEIGHT, size),
        title,
        size,
        weight,
        max_text_w,
        text_color,
        bg_color,
        clip,
    );
}

fn rects_intersect(a: &DamageRect, b: &DamageRect) -> bool {
    a.x0 <= b.x1 && a.x1 >= b.x0 && a.y0 <= b.y1 && a.y1 >= b.y0
}
