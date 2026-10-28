use crate::ttf_parser::TtfFont;

/// `(width, height)` of one line of `text`; the width is the run's, kerning
/// included, rounded up once.
pub fn measure_text(font: &TtfFont<'_>, text: &str, size_px: u16) -> (i32, i32) {
    let height = crate::ascent(font, size_px) + crate::descent(font, size_px);
    let width = libm::ceilf(crate::run_width(font, text, size_px)) as i32;
    (width, height.max(1))
}
