//! A window's frame: its title bar above the content, all four corners
//! rounded at the window radius unless it is maximized, and its shadow.

use slopos_abi::draw::{Canvas, Color32};
use slopos_gfx::blend::BlendInk;
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{Shadow, radius, with_alpha};

use super::region::Region;
use super::window_info::{WindowInfo, WindowState};
use crate::gfx::{self, DamageRect, DrawBuffer};

pub const TITLE_BAR_HEIGHT: i32 = 28;

fn shadow_token() -> Shadow {
    slopos_theme_core::current().shadow_window
}

/// Shorter than the shadow: a press further out belongs to what is beneath.
pub const fn resize_reach() -> i32 {
    8
}

pub fn outline(w: &WindowInfo) -> Rounded {
    let radius = match w.state == WindowState::Maximized {
        true => 0,
        false => radius::WINDOW,
    };
    Rounded::new(
        w.x,
        w.y - TITLE_BAR_HEIGHT,
        w.effective_width() as i32,
        w.effective_height() as i32 + TITLE_BAR_HEIGHT,
        radius,
    )
}

fn caster(w: &WindowInfo) -> Rounded {
    let mut caster = outline(w);
    caster.y += shadow_token().offset_y;
    caster
}

/// What casts the window's shadow and its colour at its darkest, which is
/// half as dark without focus; a maximized window casts none.
pub fn shadow(w: &WindowInfo, focused: bool) -> Option<(Rounded, Color32)> {
    if w.state == WindowState::Maximized {
        return None;
    }
    let color = shadow_token().color;
    let color = match focused {
        true => color,
        false => with_alpha(color, color.alpha() / 2),
    };
    Some((caster(w), color))
}

pub fn frame_box(w: &WindowInfo) -> DamageRect {
    outline(w).bounds()
}

/// Every pixel the frame or its shadow may touch.
pub fn bounds(w: &WindowInfo) -> DamageRect {
    frame_box(w).union(&caster(w).grown(shadow_token().spread).bounds())
}

/// What of a window, once changed, repaints all its frame and shadow touch.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct WindowBounds {
    rect: DamageRect,
    state: WindowState,
}

impl Default for WindowBounds {
    fn default() -> Self {
        Self {
            rect: DamageRect::invalid(),
            state: WindowState::Normal,
        }
    }
}

impl WindowBounds {
    pub fn from_window(w: &WindowInfo) -> Self {
        let rect = match w.state == WindowState::Minimized {
            true => DamageRect::invalid(),
            false => bounds(w),
        };
        Self {
            rect,
            state: w.state,
        }
    }

    /// An invalid rect while the window is minimized.
    pub fn rect(&self) -> DamageRect {
        self.rect
    }
}

/// The frame less its four rounded corners: what the window paints over
/// entirely, so nothing beneath it need be drawn.
pub fn opaque(w: &WindowInfo) -> [DamageRect; 3] {
    outline(w).bands()
}

pub fn uncornered(outline: &Rounded, area: &DamageRect) -> Region {
    let mut whole = Region::from_rect(*area);
    for corner in outline.corners() {
        whole.subtract(&corner);
    }
    whole
}

/// `area`, a part of the frame `outline` bounds, in `color`, the frame's
/// corners blended over what is beneath.
pub fn paint_in_outline(
    buf: &mut DrawBuffer,
    outline: &Rounded,
    area: &DamageRect,
    color: Color32,
) {
    for whole in uncornered(outline, area).rects() {
        gfx::fill_rect(
            buf,
            whole.x0,
            whole.y0,
            whole.x1 - whole.x0 + 1,
            whole.y1 - whole.y0 + 1,
            color,
        );
    }
    for corner in outline.corners() {
        outline.fill(buf, &corner, area, |_, _| color);
    }
}

/// The shadow `caster` throws over what is already in `clip`, `color` at
/// its darkest.
pub fn draw_window_shadow(
    buf: &mut DrawBuffer,
    caster: &Rounded,
    color: Color32,
    clip: &DamageRect,
) {
    let clip = clip.clip(buf.width() as i32, buf.height() as i32);
    if !clip.is_valid() {
        return;
    }
    let spread = shadow_token().spread;
    let over = buf.pixel_format().encode(with_alpha(color, 0xff));
    let darkest = u32::from(color.alpha());
    let shaded = |dark: u8, pixel: u32| match (u32::from(dark) * darkest / 255) as u8 {
        0 => None,
        alpha => Some(BlendInk::new(over, alpha).over(pixel)),
    };
    for y in clip.y0..=clip.y1 {
        caster.shadow_runs(
            spread,
            y,
            clip.x0,
            clip.x1,
            |first, last, dark| match dark {
                Some(0) => {}
                Some(dark) => buf.mix_row_span(y, first, last, |_, pixel| shaded(dark, pixel)),
                None => buf.mix_row_span(y, first, last, |x, pixel| {
                    shaded(caster.shadow(spread, x as i32, y), pixel)
                }),
            },
        );
    }
}

pub fn draw_outer_rim(buf: &mut DrawBuffer, w: &WindowInfo, clip: &DamageRect) {
    if w.state == WindowState::Maximized {
        return;
    }
    let ring = outline(w).grown(1);
    ring.stroke(buf, 1, clip, slopos_theme_core::current().rim_dark);
}

pub fn draw_inner_rim(buf: &mut DrawBuffer, w: &WindowInfo, clip: &DamageRect) {
    if w.state == WindowState::Maximized {
        return;
    }
    let light = slopos_theme_core::current().rim_light;
    outline(w).top_rim(buf, clip, light, TITLE_BAR_HEIGHT);
}
