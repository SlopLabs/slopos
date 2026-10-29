//! App icons: one white glyph on a tile in an [`AppTile`] hue, with no
//! effects of its own; the dock casts the shadow.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_abi::pixel::PixelFormat;
use slopos_gfx::DrawBuffer;
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{AppTile, Icon, mix, with_alpha};

/// 64 px, and 128 px as `<id>@2x.png`.
pub const SIZES: [u32; 2] = [64, 128];

pub const fn outline(side: i32) -> Rounded {
    Rounded::new(0, 0, side, side, side * 9 / 40)
}

fn hue_at(tile: AppTile, side: i32, x: i32, y: i32) -> Color32 {
    let (base, lit) = slopos_theme_core::current().app_tile(tile);
    let span = (2 * (side - 1)).max(1);
    mix(lit, base, ((x + y) * 255 / span) as u8)
}

fn within(shape: &Rounded, x: i32, y: i32, color: Color32) -> Color32 {
    match shape.coverage(x, y) {
        0 => Color32::TRANSPARENT,
        covered => with_alpha(color, covered),
    }
}

pub fn tile(size: u32, tile: AppTile) -> Vec<Color32> {
    let side = size as i32;
    let shape = outline(side);
    (0..side * side)
        .map(|i| {
            let (x, y) = (i % side, i / side);
            within(&shape, x, y, hue_at(tile, side, x, y))
        })
        .collect()
}

pub fn render(size: u32, tile: AppTile, icon: Icon) -> Vec<Color32> {
    let theme = slopos_theme_core::current();
    let side = size as i32;
    let pitch = size as usize * 4;
    let mut bytes = vec![0u8; pitch * size as usize];
    let format = PixelFormat::Argb8888;
    if let Some(mut canvas) = DrawBuffer::new(&mut bytes, size, size, pitch, 4) {
        canvas.set_pixel_format(format);
        for y in 0..side {
            for x in 0..side {
                canvas.put_pixel(x, y, format.encode(hue_at(tile, side, x, y)));
            }
        }
        let glyph = side * 9 / 16;
        let at = (side - glyph) / 2;
        let whole = DamageRect {
            x0: 0,
            y0: 0,
            x1: side - 1,
            y1: side - 1,
        };
        crate::text::icons::draw_clipped(
            &mut canvas,
            at,
            at,
            icon,
            glyph,
            theme.text_on_accent,
            &whole,
        );
    }
    let shape = outline(side);
    bytes
        .chunks_exact(4)
        .enumerate()
        .map(|(i, px)| {
            let (x, y) = ((i % size as usize) as i32, (i / size as usize) as i32);
            let ink = format.decode(u32::from_le_bytes([px[0], px[1], px[2], px[3]]));
            within(&shape, x, y, ink)
        })
        .collect()
}
