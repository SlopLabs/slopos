use slopos_abi::draw::Color32;
use slopos_gfx::canvas_font;

use crate::graphics::GraphicsContext;

pub use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};

pub fn draw_char(ctx: &mut GraphicsContext, x: i32, y: i32, c: u8, fg: Color32, bg: Color32) {
    canvas_font::draw_char(ctx, x, y, c, fg, bg);
}

pub fn draw_string(
    ctx: &mut GraphicsContext,
    x: i32,
    y: i32,
    text: &[u8],
    fg: Color32,
    bg: Color32,
) {
    canvas_font::draw_string(ctx, x, y, text, fg, bg);
}
