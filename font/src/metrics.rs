//! Text measurement utilities.

use crate::ttf_parser::TtfFont;

/// Measure the width and height of a text string at a given pixel size.
///
/// Returns `(width, height)` in pixels; multi-line text is not handled. The
/// width is the run's, kerning included, rounded up once.
pub fn measure_text(font: &TtfFont<'_>, text: &str, size_px: u16) -> (i32, i32) {
    let scale = crate::scale(font, size_px);
    let hhea = font.hhea();
    let height = libm::ceilf((hhea.ascender as f32 - hhea.descender as f32) * scale) as i32;
    let width = libm::ceilf(crate::run_width(font, text, size_px)) as i32;
    (width, height.max(1))
}
