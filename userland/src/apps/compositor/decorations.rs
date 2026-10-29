//! A window's title bar, its close, minimise and zoom buttons, and the
//! hit-tests that map the pointer onto them and onto its edges.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::Color32;
use slopos_appkit::text::ui;
use slopos_gfx::canvas_ops::line_aa;
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{TextRole, Theme, with_alpha};

use super::frame::{TITLE_BAR_HEIGHT, resize_reach};
use super::input::ResizeEdge;
use crate::gfx::{self, DrawBuffer};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowButton {
    Close,
    Minimize,
    Zoom,
}

impl WindowButton {
    const ALL: [WindowButton; 3] = [
        WindowButton::Close,
        WindowButton::Minimize,
        WindowButton::Zoom,
    ];

    fn centre_x(self) -> i32 {
        match self {
            WindowButton::Close => 18,
            WindowButton::Minimize => 38,
            WindowButton::Zoom => 58,
        }
    }

    fn color(self, theme: &Theme) -> Color32 {
        match self {
            WindowButton::Close => theme.danger,
            WindowButton::Minimize => theme.warning,
            WindowButton::Zoom => theme.success,
        }
    }
}

const BUTTON_RADIUS: i32 = 6;
const BUTTON_CY: i32 = TITLE_BAR_HEIGHT / 2;
/// Out of 255 of `shade`: a dark glyph reads on all three bright discs.
const BUTTON_EDGE: u8 = 0x40;
const GLYPH_INK: u8 = 0xa6;
/// Where the pointer shows the buttons' glyphs, from the frame's top-left.
const GROUP: DamageRect = DamageRect {
    x0: 6,
    y0: 2,
    x1: 69,
    y1: 25,
};
/// Half a glyph's stroke; the close cross's diagonals are as long.
const ARM: i32 = 4;
const DIAGONAL_ARM: i32 = 3;
/// What the buttons and their mirror on the right leave the title.
const TITLE_MARGIN: i32 = 140;
/// How far along each edge from a corner a resize takes both edges.
const RESIZE_CORNER: i32 = 24;

/// The title bar of the window whose frame is `outline`, rounded with its
/// top corners. The buttons show their glyphs while `hovered` and are grey
/// on a window without focus; a window that does not zoom shows its zoom
/// button grey and without a glyph.
pub fn draw_window_decorations(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    title: &str,
    focused: bool,
    hovered: bool,
    zooms: bool,
    clip: &DamageRect,
) {
    let theme = slopos_theme_core::current();
    draw_title_bar_background(buf, outline, theme, clip);
    draw_buttons(buf, outline, theme, focused, hovered, zooms, clip);
    draw_title_text(buf, outline, theme, title, focused, clip);
}

/// The button at `(px, py)` of the window whose frame's top-left is
/// `(frame_x, frame_y)`.
pub fn button_at(frame_x: i32, frame_y: i32, px: i32, py: i32) -> Option<WindowButton> {
    let dy = py - (frame_y + BUTTON_CY);
    WindowButton::ALL.into_iter().find(|button| {
        let dx = px - (frame_x + button.centre_x());
        dx * dx + dy * dy <= BUTTON_RADIUS * BUTTON_RADIUS
    })
}

/// The buttons' group, which shows their glyphs while the pointer is in it.
pub fn buttons_group(frame_x: i32, frame_y: i32) -> DamageRect {
    DamageRect {
        x0: frame_x + GROUP.x0,
        y0: frame_y + GROUP.y0,
        x1: frame_x + GROUP.x1,
        y1: frame_y + GROUP.y1,
    }
}

/// The edge `(px, py)` takes for a resize: just outside the frame, a corner
/// before an edge. `window_y` is the content's top.
pub fn hit_test_resize_edge(
    window_x: i32,
    window_y: i32,
    window_w: u32,
    window_h: u32,
    px: i32,
    py: i32,
) -> ResizeEdge {
    let ww = window_w as i32;
    let wh = window_h as i32;

    let frame_x0 = window_x;
    let frame_y0 = window_y - TITLE_BAR_HEIGHT;
    let frame_x1 = window_x + ww - 1;
    let frame_y1 = window_y + wh - 1;

    let reach = resize_reach();
    let ext_x0 = frame_x0 - reach;
    let ext_y0 = frame_y0 - reach;
    let ext_x1 = frame_x1 + reach;
    let ext_y1 = frame_y1 + reach;

    if px < ext_x0 || px > ext_x1 || py < ext_y0 || py > ext_y1 {
        return ResizeEdge::NONE;
    }

    if px >= frame_x0 && px <= frame_x1 && py >= frame_y0 && py <= frame_y1 {
        return ResizeEdge::NONE;
    }

    let frame_w = ww;
    let frame_h = wh + TITLE_BAR_HEIGHT;
    let corner_range = RESIZE_CORNER.min(frame_w / 2).min(frame_h / 2);

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

fn draw_title_bar_background(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    theme: &Theme,
    clip: &DamageRect,
) {
    let bg = theme.chrome;
    let (x, y, w) = (outline.x, outline.y, outline.w);
    let r = outline.radius.clamp(0, TITLE_BAR_HEIGHT.min(w / 2));
    gfx::fill_rect_clipped(buf, x + r, y, w - 2 * r, r, bg, clip);
    gfx::fill_rect_clipped(buf, x, y + r, w, TITLE_BAR_HEIGHT - r, bg, clip);
    for corner in &outline.corners()[..2] {
        outline.fill(buf, corner, clip, |_, _| bg);
    }
}

fn draw_buttons(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    theme: &Theme,
    focused: bool,
    hovered: bool,
    zooms: bool,
    clip: &DamageRect,
) {
    let cy = outline.y + BUTTON_CY;
    for button in WindowButton::ALL {
        let enabled = zooms || button != WindowButton::Zoom;
        let color = match enabled && (focused || hovered) {
            true => button.color(theme),
            false => theme.neutral_fill,
        };
        let (cx, r) = (outline.x + button.centre_x(), BUTTON_RADIUS);
        let disc = Rounded::circle(cx - r, cy - r, 2 * r + 1);
        disc.draw(buf, clip, color);
        disc.stroke(buf, 1, clip, with_alpha(theme.shade, BUTTON_EDGE));
        if enabled && hovered {
            draw_glyph(
                buf,
                button,
                cx,
                cy,
                with_alpha(theme.shade, GLYPH_INK),
                clip,
            );
        }
    }
}

fn draw_glyph(
    buf: &mut DrawBuffer,
    button: WindowButton,
    cx: i32,
    cy: i32,
    ink: Color32,
    clip: &DamageRect,
) {
    let reach = match button {
        WindowButton::Close => DIAGONAL_ARM,
        WindowButton::Minimize | WindowButton::Zoom => ARM,
    };
    let area = DamageRect {
        x0: cx - reach,
        y0: cy - reach,
        x1: cx + reach,
        y1: cy + reach,
    };
    if !area.intersects(clip) {
        return;
    }
    let mut line = |x0, y0, x1, y1| line_aa(&mut *buf, x0, y0, x1, y1, ink);
    match button {
        WindowButton::Close => {
            line(cx - reach, cy - reach, cx + reach, cy + reach);
            line(cx + reach, cy - reach, cx - reach, cy + reach);
        }
        WindowButton::Minimize => {
            line(cx - reach, cy, cx + reach, cy);
        }
        WindowButton::Zoom => {
            line(cx - reach, cy, cx + reach, cy);
            line(cx, cy - reach, cx, cy + reach);
        }
    }
}

fn draw_title_text(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    theme: &Theme,
    title: &str,
    focused: bool,
    clip: &DamageRect,
) {
    let max_text_w = outline.w - TITLE_MARGIN;
    if title.is_empty() || max_text_w <= 0 {
        return;
    }
    let ink = match focused {
        true => theme.text,
        false => theme.text_secondary,
    };
    let role = TextRole::Body;
    let text_w = ui::elided_width(title, role, max_text_w);
    ui::draw_elided(
        buf,
        outline.x + (outline.w - text_w) / 2,
        ui::centred_top(outline.y, TITLE_BAR_HEIGHT, role),
        title,
        role,
        max_text_w,
        ink,
        theme.chrome,
        clip,
    );
}
