pub use slopos_abi::font::{
    FONT_CHAR_COUNT, FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH, FONT_DATA, FONT_FIRST_CHAR, FONT_LAST_CHAR,
    get_glyph,
};
pub use slopos_gfx::canvas_font::{
    draw_char, draw_str as draw_string, str_lines as string_lines, str_width as string_width,
};

pub fn string_height(text: &str) -> i32 {
    string_lines(text) * FONT_CHAR_HEIGHT
}
