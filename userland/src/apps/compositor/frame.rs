//! A window's frame: its title bar above the content, all four corners
//! rounded at the window radius unless it is maximized, and the shadow it
//! casts, which reaches as far whether or not the window has focus.

use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::surface::{WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED};
use slopos_gfx::rounded::Rounded;

use super::region::Region;
use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::{
    SHADOW_MAX_ALPHA, SHADOW_OFFSET_Y, SHADOW_SPREAD, TITLE_BAR_HEIGHT, WINDOW_CORNER_RADIUS,
};

/// The whole frame, title bar and content.
pub fn outline(w: &UserWindowInfo) -> Rounded {
    let radius = match w.state == WINDOW_STATE_MAXIMIZED {
        true => 0,
        false => WINDOW_CORNER_RADIUS,
    };
    Rounded::new(
        w.x,
        w.y - TITLE_BAR_HEIGHT,
        w.effective_width() as i32,
        w.effective_height() as i32 + TITLE_BAR_HEIGHT,
        radius,
    )
}

fn caster(w: &UserWindowInfo) -> Rounded {
    let mut caster = outline(w);
    caster.y += SHADOW_OFFSET_Y;
    caster
}

/// What casts the window's shadow and how dark it is at its darkest; a
/// maximized window casts none.
pub fn shadow(w: &UserWindowInfo, focused: bool) -> Option<(Rounded, u8)> {
    if w.state == WINDOW_STATE_MAXIMIZED {
        return None;
    }
    let darkest = match focused {
        true => SHADOW_MAX_ALPHA,
        false => SHADOW_MAX_ALPHA / 2,
    };
    Some((caster(w), darkest))
}

pub fn frame_box(w: &UserWindowInfo) -> DamageRect {
    outline(w).bounds()
}

/// Every pixel the frame or its shadow may touch.
pub fn bounds(w: &UserWindowInfo) -> DamageRect {
    let frame = frame_box(w);
    let shade = caster(w).grown(SHADOW_SPREAD).bounds();
    DamageRect {
        x0: frame.x0.min(shade.x0),
        y0: frame.y0.min(shade.y0),
        x1: frame.x1.max(shade.x1),
        y1: frame.y1.max(shade.y1),
    }
}

/// What of a window, once changed, repaints all its frame and shadow touch.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct WindowBounds {
    rect: DamageRect,
    state: u8,
}

impl Default for WindowBounds {
    fn default() -> Self {
        Self {
            rect: DamageRect::invalid(),
            state: 0,
        }
    }
}

impl WindowBounds {
    pub fn from_window(w: &UserWindowInfo) -> Self {
        let rect = match w.state == WINDOW_STATE_MINIMIZED {
            true => DamageRect::invalid(),
            false => bounds(w),
        };
        Self {
            rect,
            state: w.state,
        }
    }

    /// Every pixel the window may touch; none while it is minimized.
    pub fn rect(&self) -> DamageRect {
        self.rect
    }
}

/// The frame less its four rounded corners: what the window paints over
/// entirely, so nothing beneath it need be drawn.
pub fn opaque(w: &UserWindowInfo) -> [DamageRect; 3] {
    outline(w).bands()
}

/// `area` less `outline`'s corners.
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

/// `pixel` with every channel scaled by `(255 - alpha) / 255`, two to a
/// word.
fn darkened(pixel: u32, alpha: u32) -> u32 {
    let inv = 255 - alpha;
    let rb = (pixel & 0x00FF00FF) * inv + 0x00800080;
    let ag = ((pixel >> 8) & 0x00FF00FF) * inv + 0x00800080;
    (rb >> 8 & 0x00FF00FF) | (ag >> 8 & 0x00FF00FF) << 8
}

/// The shadow `caster` throws over what is already in `clip`, at most
/// `darkest` out of 255 of black.
pub fn draw_window_shadow(buf: &mut DrawBuffer, caster: &Rounded, darkest: u8, clip: &DamageRect) {
    let clip = clip.clip(buf.width() as i32, buf.height() as i32);
    if !clip.is_valid() {
        return;
    }
    let bpp = buf.bytes_per_pixel() as usize;
    let pitch = buf.pitch_bytes();
    for y in clip.y0..=clip.y1 {
        let base = y as usize * pitch;
        for x in clip.x0..=clip.x1 {
            let alpha = u32::from(caster.shadow(SHADOW_SPREAD, x, y)) * u32::from(darkest) / 255;
            if alpha == 0 {
                continue;
            }
            let off = base + x as usize * bpp;
            let pixel = darkened(buf.read_encoded_at(off), alpha);
            buf.write_encoded_at(off, EncodedPixel(pixel));
        }
    }
}
