//! The SlopOS logo, `assets/logo.png`, pixel for pixel: drawn in whole
//! pixels scaled by a whole number, so pixel art stays crisp.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::over_backdrop;

pub const WIDTH: usize = 73;
pub const HEIGHT: usize = 18;

/// `d` the outline, `c` the screen and keyboard, `g` the glow and the
/// letters; `.` nothing.
const ART: [&[u8; WIDTH]; HEIGHT] = [
    b".........................................................................",
    b".dddddddddddddd..........................................................",
    b".dccccccccccccd.......dddddd..ddddd.....................dddddd...dddddd..",
    b".dcddddddddddcd......ddggggdd.dgggd....................ddggggdd.ddggggdd.",
    b".dcddggddggddcd......dggddggd.ddggd....................dggddggd.dggddggd.",
    b".dcddggddggddcd......dggddddd..dggd...dddddd..ddddddd..dggddggd.dggddddd.",
    b".dcddddddddddcd......dgggddd...dggd..ddggggdd.dgggggdd.dggddggd.dgggddd..",
    b".dcdgddddddgdcd......ddggggdd..dggd..dggddggd.dggddggd.dggddggd.ddggggdd.",
    b".dcddggggggddcd.......dddgggd..dggd..dggddggd.dggddggd.dggddggd..dddgggd.",
    b".dcddddddddddcd......dddddggd..dggd..dggddggd.dggddggd.dggddggd.dddddggd.",
    b".dccccccccccccd......dggddggd.ddggdd.dggddggd.dggddggd.dggddggd.dggddggd.",
    b".dddddddddddddd......ddggggdd.dggggd.ddggggdd.dgggggdd.ddggggdd.ddggggdd.",
    b"...dddddddddddddd.....dddddd..dddddd..dddddd..dggdddd...dddddd...dddddd..",
    b"...dcdcdcdcdcdccd.............................dggd.......................",
    b"...dccccccccccccd.............................dggd.......................",
    b"...dggggggggggggd.............................dddd.......................",
    b"...dddddddddddddd........................................................",
    b".........................................................................",
];

const OUTLINE: Color32 = Color32::rgb(0x13, 0x35, 0x1F);
const CREAM: Color32 = Color32::rgb(0xFA, 0xFA, 0xEB);
const GREEN: Color32 = Color32::rgb(0x8E, 0xE4, 0x51);

/// The colour of the logo's pixel at `(x, y)`, if it has one.
pub fn pixel(x: usize, y: usize) -> Option<Color32> {
    match ART.get(y)?.get(x)? {
        b'd' => Some(OUTLINE),
        b'c' => Some(CREAM),
        b'g' => Some(GREEN),
        _ => None,
    }
}

/// Where the logo goes: its top-left, and how many screen pixels square
/// each of its pixels takes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Logo {
    pub x: i32,
    pub y: i32,
    pub scale: i32,
}

impl Logo {
    /// Centred, near the foot of a `width` by `height` screen whose shorter
    /// side is `unit`.
    pub fn for_screen(width: i32, height: i32, unit: i32) -> Logo {
        let scale = (unit / 400).max(1);
        let margin = unit * 7 / 100;
        Logo {
            x: (width - WIDTH as i32 * scale) / 2,
            y: height - margin - HEIGHT as i32 * scale,
            scale,
        }
    }

    pub fn bounds(&self) -> DamageRect {
        DamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + WIDTH as i32 * self.scale - 1,
            y1: self.y + HEIGHT as i32 * self.scale - 1,
        }
    }

    /// Draw the logo over the backdrop at `level` out of 255 of its colours,
    /// leaving its empty pixels as they are.
    pub fn paint<C: Canvas>(&self, canvas: &mut C, level: u8) {
        let format = canvas.pixel_format();
        for (y, row) in ART.iter().enumerate() {
            for x in 0..row.len() {
                if let Some(color) = pixel(x, y) {
                    let s = self.scale;
                    canvas.fill_rect_encoded(
                        self.x + x as i32 * s,
                        self.y + y as i32 * s,
                        s,
                        s,
                        format.encode(over_backdrop(color, level)),
                    );
                }
            }
        }
    }
}
