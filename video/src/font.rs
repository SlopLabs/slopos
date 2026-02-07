use core::ffi::{c_char, c_int};

use slopos_abi::draw::Color32;
use slopos_gfx::canvas_font;

use crate::framebuffer;
use crate::graphics::GraphicsContext;

pub use slopos_abi::font::{FONT_CHAR_HEIGHT, FONT_CHAR_WIDTH};

const FONT_SUCCESS: c_int = 0;
const FONT_ERROR_NO_FB: c_int = -1;
const FONT_ERROR_INVALID: c_int = -3;

fn framebuffer_ready() -> bool {
    framebuffer::framebuffer_is_initialized() != 0
}

fn c_str_to_slice(ptr: *const c_char) -> &'static [u8] {
    if ptr.is_null() {
        return &[];
    }
    let mut len = 0usize;
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(ptr as *const u8, len)
    }
}

pub fn font_draw_char_ctx(
    _ctx: &GraphicsContext,
    x: i32,
    y: i32,
    c: c_char,
    fg_color: Color32,
    bg_color: Color32,
) -> c_int {
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let mut ctx = match GraphicsContext::new() {
        Ok(c) => c,
        Err(_) => return FONT_ERROR_NO_FB,
    };

    canvas_font::draw_char(&mut ctx, x, y, c as u8, fg_color, bg_color);
    FONT_SUCCESS
}

pub fn font_draw_string_ctx(
    _ctx: &GraphicsContext,
    x: i32,
    y: i32,
    str_ptr: *const c_char,
    fg_color: Color32,
    bg_color: Color32,
) -> c_int {
    if str_ptr.is_null() {
        return FONT_ERROR_INVALID;
    }
    if !framebuffer_ready() {
        return FONT_ERROR_NO_FB;
    }

    let mut ctx = match GraphicsContext::new() {
        Ok(c) => c,
        Err(_) => return FONT_ERROR_NO_FB,
    };

    let text = c_str_to_slice(str_ptr);
    canvas_font::draw_string(&mut ctx, x, y, text, fg_color, bg_color);
    FONT_SUCCESS
}
