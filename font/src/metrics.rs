//! Text measurement utilities.

use crate::ttf_parser::TtfFont;

/// Measure the width and height of a text string at a given pixel size.
///
/// Returns `(width, height)` in pixels; multi-line text is not handled.
pub fn measure_text(font: &TtfFont<'_>, text: &str, size_px: u16) -> (i32, i32) {
    let upem = font.units_per_em() as f32;
    if upem == 0.0 {
        return (0, 0);
    }
    let scale = size_px as f32 / upem;

    let hhea = font.hhea();
    let height = libm::ceilf((hhea.ascender as f32 - hhea.descender as f32) * scale) as i32;

    // Accumulate width in float to avoid per-character truncation error.
    let mut width_f = 0.0f32;

    for ch in text.chars() {
        width_f += crate::advance_px(font, ch as u32, scale, size_px);
    }

    (libm::ceilf(width_f) as i32, height.max(1))
}
