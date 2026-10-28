//! The fixed-cell face the terminal and the console draw in.

use std::sync::OnceLock;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;

const FONT_SIZE_PX: u16 = 16;

static ATLAS: OnceLock<Option<GlyphAtlas>> = OnceLock::new();

pub fn atlas() -> Option<&'static GlyphAtlas> {
    ATLAS
        .get_or_init(|| GlyphAtlas::new(slopos_appkit::text::load_font("mono")?, FONT_SIZE_PX))
        .as_ref()
}

const FALLBACK_CELL_W: i32 = 8;
const FALLBACK_CELL_H: i32 = 16;

pub fn cell_width() -> i32 {
    atlas().map_or(FALLBACK_CELL_W, |a| a.cell_width())
}

pub fn cell_height() -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| a.cell_height())
}

/// A codepoint outside the atlas's glyph set draws as its replacement glyph.
pub fn draw_glyph<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    cp: u32,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas()?.draw_char(target, x, y, cp, fg, bg)
}
