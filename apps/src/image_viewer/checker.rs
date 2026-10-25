//! The checks behind an image's transparent pixels, light on any theme
//! because they are what the image's own colours are judged against.

use slopos_abi::draw::Color32;
use slopos_gfx::DrawBuffer;
use slopos_gfx::canvas_ops::fill_rect;

const SIZE: i32 = 16;
const LIGHT: Color32 = Color32::rgb(0xcf, 0xd4, 0xda);
const LIGHTER: Color32 = Color32::rgb(0xf2, 0xf4, 0xf6);

pub fn draw(fb: &mut DrawBuffer<'_>, height: i32) {
    let cols = (fb.width() as i32 + SIZE - 1) / SIZE;
    let rows = (height + SIZE - 1) / SIZE;
    for row in 0..rows {
        for col in 0..cols {
            let color = if (row + col) & 1 == 0 { LIGHT } else { LIGHTER };
            fill_rect(fb, col * SIZE, row * SIZE, SIZE, SIZE, color);
        }
    }
}
