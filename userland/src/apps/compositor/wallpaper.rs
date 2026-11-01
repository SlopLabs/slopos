//! The desktop's gradient at the screen's own size, as the theme shades it.

use slopos_abi::draw::Canvas;
use slopos_theme_core::Wallpaper;

use crate::gfx::DrawBuffer;

pub fn paint(buf: &mut DrawBuffer, wallpaper: &Wallpaper) {
    let (w, h) = (buf.width() as i32, buf.height() as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let format = buf.pixel_format();
    let shading = wallpaper.shading(w, h);
    for y in 0..h {
        let row = shading.row(y);
        for x in 0..w {
            buf.put_pixel(x, y, format.encode(row.at(x)));
        }
    }
}
