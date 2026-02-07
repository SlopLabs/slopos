use slopos_abi::draw::Color32;
use slopos_gfx::canvas_font;

use super::DrawBuffer;

pub use slopos_abi::font::{
    FONT_CHAR_COUNT, FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, FONT_DATA, FONT_FIRST_CHAR, FONT_LAST_CHAR,
    get_glyph,
};

pub fn draw_char(buf: &mut DrawBuffer, x: i32, y: i32, ch: u8, fg: Color32, bg: Color32) {
    let dmg = canvas_font::draw_char(buf, x, y, ch, fg, bg);
    buf.damage_mut().apply(dmg);
}

pub fn draw_string(buf: &mut DrawBuffer, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
    let dmg = canvas_font::draw_str(buf, x, y, text, fg, bg);
    buf.damage_mut().apply(dmg);
}

pub fn string_width(text: &str) -> i32 {
    canvas_font::str_width(text)
}

pub fn string_lines(text: &str) -> i32 {
    canvas_font::str_lines(text)
}

pub fn string_height(text: &str) -> i32 {
    string_lines(text) * FONT_CHAR_HEIGHT
}
