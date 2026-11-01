//! The backgrounds the theme draws itself: a gradient from `top` to
//! `bottom` with a soft light from above, dithered so it shows no bands.

use slopos_abi::draw::Color32;

use crate::Wallpaper;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Gradient {
    /// What a person's preferences name it by.
    pub id: &'static str,
    pub name: &'static str,
    pub wallpaper: Wallpaper,
}

pub(crate) const GRAPHITE: Wallpaper = Wallpaper {
    top: Color32::rgb(0x1c, 0x23, 0x33),
    bottom: Color32::rgb(0x0c, 0x0e, 0x12),
    light: Color32::new(0x5c, 0x86, 0xd6, 0x48),
};

const fn gradient(
    id: &'static str,
    name: &'static str,
    top: u32,
    bottom: u32,
    light: u32,
) -> Gradient {
    Gradient {
        id,
        name,
        wallpaper: Wallpaper {
            top: Color32::rgb((top >> 16) as u8, (top >> 8) as u8, top as u8),
            bottom: Color32::rgb((bottom >> 16) as u8, (bottom >> 8) as u8, bottom as u8),
            light: Color32::new(
                (light >> 24) as u8,
                (light >> 16) as u8,
                (light >> 8) as u8,
                light as u8,
            ),
        },
    }
}

/// The theme's own first, which a person who chose none sees.
pub const GRADIENTS: [Gradient; 6] = [
    Gradient {
        id: "graphite",
        name: "Graphite",
        wallpaper: GRAPHITE,
    },
    gradient("dusk", "Dusk", 0x2b2140, 0x0e0b14, 0xa27ae048),
    gradient("ocean", "Ocean", 0x10303b, 0x060f12, 0x3fb2c944),
    gradient("forest", "Forest", 0x15301f, 0x070e09, 0x63c27e3c),
    gradient("ember", "Ember", 0x3b2016, 0x110906, 0xe2884e44),
    gradient("slate", "Slate", 0x2a2c31, 0x0f1012, 0xb6bfcf2c),
];

pub const GRADIENT_IDS: [&str; GRADIENTS.len()] = {
    let mut ids = [""; GRADIENTS.len()];
    let mut at = 0;
    while at < GRADIENTS.len() {
        ids[at] = GRADIENTS[at].id;
        at += 1;
    }
    ids
};

/// Shares of the screen; the centre sits above the top edge, so only the
/// fall-off shows.
const LIGHT_X: f32 = 0.38;
const LIGHT_Y: f32 = -0.25;
const REACH_X: f32 = 0.85;
const REACH_Y: f32 = 0.95;

const BAYER: [[u8; 4]; 4] = [[0, 8, 2, 10], [12, 4, 14, 6], [3, 11, 1, 9], [15, 7, 13, 5]];

/// A wallpaper laid over a screen of one size, which says the colour of each
/// of its pixels.
pub struct Shading {
    top: [f32; 3],
    bottom: [f32; 3],
    light: [f32; 3],
    strength: f32,
    centre: (f32, f32),
    reach: (f32, f32),
    span: f32,
}

impl Wallpaper {
    pub fn shading(&self, width: i32, height: i32) -> Shading {
        let channels = |c: Color32| [c.red(), c.green(), c.blue()].map(f32::from);
        let (w, h) = (width.max(1) as f32, height.max(1) as f32);
        Shading {
            top: channels(self.top),
            bottom: channels(self.bottom),
            light: channels(self.light),
            strength: f32::from(self.light.alpha()) / 255.0,
            centre: (w * LIGHT_X, h * LIGHT_Y),
            reach: (w * REACH_X, h * REACH_Y),
            span: (height - 1).max(1) as f32,
        }
    }
}

impl Shading {
    pub fn at(&self, x: i32, y: i32) -> Color32 {
        self.row(y).at(x)
    }

    /// Row `y`, whose part of the gradient each of its pixels shares.
    pub fn row(&self, y: i32) -> Row<'_> {
        let t = y as f32 / self.span;
        let dy = (y as f32 - self.centre.1) / self.reach.1;
        Row {
            shading: self,
            y,
            base: [0, 1, 2].map(|i| self.top[i] + (self.bottom[i] - self.top[i]) * t),
            dy2: dy * dy,
        }
    }
}

pub struct Row<'a> {
    shading: &'a Shading,
    y: i32,
    base: [f32; 3],
    dy2: f32,
}

impl Row<'_> {
    pub fn at(&self, x: i32) -> Color32 {
        let shading = self.shading;
        let dx = (x as f32 - shading.centre.0) / shading.reach.0;
        let d2 = dx * dx + self.dy2;
        let lit = shading.strength / (1.0 + 4.0 * d2 * d2);
        let dither = (f32::from(BAYER[(self.y & 3) as usize][(x & 3) as usize]) + 0.5) / 16.0 - 0.5;
        let [r, g, b] = [0, 1, 2].map(|i| {
            let base = self.base[i];
            (base + (shading.light[i] - base) * lit + dither + 0.5).clamp(0.0, 255.0) as u8
        });
        Color32::rgb(r, g, b)
    }
}
