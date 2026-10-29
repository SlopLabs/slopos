//! The desktop: the theme's [`Wallpaper`] at the screen's own size, dithered
//! so the gradient shows no bands.

use slopos_abi::draw::{Canvas, Color32};
use slopos_theme_core::Wallpaper;

use crate::gfx::DrawBuffer;

/// Shares of the screen; the centre sits above the top edge, so only the
/// fall-off shows.
const LIGHT_X: f32 = 0.38;
const LIGHT_Y: f32 = -0.25;
const REACH_X: f32 = 0.85;
const REACH_Y: f32 = 0.95;

const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

pub fn paint(buf: &mut DrawBuffer, wallpaper: &Wallpaper) {
    let (w, h) = (buf.width() as i32, buf.height() as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let format = buf.pixel_format();
    let channels = |c: Color32| [c.red(), c.green(), c.blue()].map(f32::from);
    let (top, bottom, light) = (
        channels(wallpaper.top),
        channels(wallpaper.bottom),
        channels(wallpaper.light),
    );
    let strength = f32::from(wallpaper.light.alpha()) / 255.0;
    let (cx, cy) = (w as f32 * LIGHT_X, h as f32 * LIGHT_Y);
    let (rx, ry) = (w as f32 * REACH_X, h as f32 * REACH_Y);
    let span = (h - 1).max(1) as f32;
    for y in 0..h {
        let t = y as f32 / span;
        let base = [0, 1, 2].map(|i| top[i] + (bottom[i] - top[i]) * t);
        let dy = (y as f32 - cy) / ry;
        for x in 0..w {
            let dx = (x as f32 - cx) / rx;
            let d2 = dx * dx + dy * dy;
            let lit = strength / (1.0 + 4.0 * d2 * d2);
            let dither = (f32::from(BAYER[(y & 3) as usize][(x & 3) as usize]) + 0.5) / 16.0 - 0.5;
            let [r, g, b] = [0, 1, 2].map(|i| {
                (base[i] + (light[i] - base[i]) * lit + dither + 0.5).clamp(0.0, 255.0) as u8
            });
            buf.put_pixel(x, y, format.encode(Color32::rgb(r, g, b)));
        }
    }
}
