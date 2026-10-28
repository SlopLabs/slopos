//! Every app's icon: a glyph of the icon font on a rounded square in a series
//! colour, which `tools/icons` renders and the dock draws for an app with none.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::DrawBuffer;
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{Icon, with_alpha};

/// The sizes an app icon ships at: 64 px, and twice that as `<id>@2x.png`.
pub const SIZES: [u32; 2] = [64, 128];

/// `icon` on a tile `size` px square in the theme's series colour `tint`,
/// as straight-alpha pixels, row by row.
pub fn render(size: u32, tint: usize, icon: Icon) -> Vec<Color32> {
    let theme = slopos_theme_core::current();
    let side = size as i32;
    let pitch = size as usize * 4;
    let mut bytes = vec![0u8; pitch * size as usize];
    let format = PixelFormat::Argb8888;
    if let Some(mut tile) = DrawBuffer::new(&mut bytes, size, size, pitch, 4) {
        tile.set_pixel_format(format);
        tile.clear_canvas(format.encode(theme.series(tint)));
        let glyph = side * 9 / 16;
        let at = (side - glyph) / 2;
        let whole = DamageRect {
            x0: 0,
            y0: 0,
            x1: side - 1,
            y1: side - 1,
        };
        crate::text::icons::draw_clipped(
            &mut tile,
            at,
            at,
            icon,
            glyph,
            theme.text_on_accent,
            &whole,
        );
    }
    let outline = Rounded::new(0, 0, side, side, side * 9 / 40);
    bytes
        .chunks_exact(4)
        .enumerate()
        .map(|(i, px)| {
            let (x, y) = ((i % size as usize) as i32, (i / size as usize) as i32);
            let ink = format.decode(u32::from_le_bytes([px[0], px[1], px[2], px[3]]));
            with_alpha(ink, outline.coverage(x, y))
        })
        .collect()
}
