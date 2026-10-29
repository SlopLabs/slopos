//! Pixels: a buffer of encoded pixels, coverage masks, and laying a mask down
//! turned and at a fractional position.
//!
//! Every pixel here is already encoded for the screen's format, four bytes of
//! it, and mixing works on those bytes whichever channel each one is: the
//! wheel never decodes what it draws.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

/// `b` over `a` at `t` out of 256, byte by byte.
#[inline(always)]
pub fn mix(a: u32, b: u32, t: u32) -> u32 {
    let s = 256 - t;
    let even = ((((a & 0x00FF_00FF) * s) + ((b & 0x00FF_00FF) * t)) >> 8) & 0x00FF_00FF;
    let odd =
        (((((a >> 8) & 0x00FF_00FF) * s) + (((b >> 8) & 0x00FF_00FF) * t)) >> 8) & 0x00FF_00FF;
    even | (odd << 8)
}

/// A coverage byte as a [`mix`] weight.
#[inline(always)]
pub const fn weight(coverage: u32) -> u32 {
    coverage + (coverage >> 7)
}

/// A rectangle of encoded pixels, rows packed.
pub struct Pixmap {
    pub w: i32,
    pub h: i32,
    pub px: Vec<u32>,
    format: PixelFormat,
}

impl Pixmap {
    pub fn new(w: i32, h: i32, format: PixelFormat) -> Self {
        Self {
            w,
            h,
            px: vec![0; (w.max(0) * h.max(0)) as usize],
            format,
        }
    }

    pub fn format(&self) -> PixelFormat {
        self.format
    }

    pub fn encode(&self, color: Color32) -> u32 {
        self.format.encode(color).0
    }

    /// Copy `rect`, in this pixmap's coordinates, from `from` (same size).
    pub fn copy_rect(&mut self, from: &Pixmap, rect: DamageRect) {
        let x0 = rect.x0.max(0);
        let x1 = rect.x1.min(self.w - 1);
        if x0 > x1 {
            return;
        }
        for y in rect.y0.max(0)..=rect.y1.min(self.h - 1) {
            let row = (y * self.w) as usize;
            let span = row + x0 as usize..=row + x1 as usize;
            self.px[span.clone()].copy_from_slice(&from.px[span]);
        }
    }
}

impl Canvas for Pixmap {
    fn width(&self) -> u32 {
        self.w as u32
    }

    fn height(&self) -> u32 {
        self.h as u32
    }

    fn pitch_bytes(&self) -> usize {
        self.w as usize * 4
    }

    fn bytes_per_pixel(&self) -> u8 {
        4
    }

    fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        if let Some(p) = self.px.get_mut(byte_offset / 4) {
            *p = pixel.0;
        }
    }

    fn read_encoded_at(&self, byte_offset: usize) -> u32 {
        self.px.get(byte_offset / 4).copied().unwrap_or(0)
    }
}

/// Coverage, 0 to 255, of something drawn once and laid down many times.
pub struct Mask {
    pub w: i32,
    pub h: i32,
    pub cov: Vec<u8>,
}

impl Mask {
    pub fn new(w: i32, h: i32) -> Self {
        Self {
            w,
            h,
            cov: vec![0; (w.max(0) * h.max(0)) as usize],
        }
    }

    /// The shape whose signed distance (negative inside) `sdf` gives at each
    /// pixel's centre, its edge a pixel wide.
    pub fn from_sdf(w: i32, h: i32, sdf: impl Fn(f32, f32) -> f32) -> Self {
        let mut mask = Self::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let d = sdf(x as f32 + 0.5, y as f32 + 0.5);
                mask.cov[(y * w + x) as usize] = ((0.5 - d).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        }
        mask
    }

    /// The text's coverage, read back from it drawn white on black.
    pub fn from_canvas_red(canvas: &Pixmap) -> Self {
        let mut mask = Self::new(canvas.w, canvas.h);
        for (c, p) in mask.cov.iter_mut().zip(&canvas.px) {
            *c = canvas.format().decode(*p).red();
        }
        mask
    }

    #[inline(always)]
    fn at(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            0
        } else {
            u32::from(self.cov[(y * self.w + x) as usize])
        }
    }

    /// Bilinear coverage at `(x, y)` in 256ths of a pixel, pixel centres on
    /// the half.
    #[inline(always)]
    fn sample(&self, x: i32, y: i32) -> u32 {
        let (x, y) = (x - 128, y - 128);
        let (ix, iy) = (x >> 8, y >> 8);
        let (fx, fy) = ((x & 255) as u32, (y & 255) as u32);
        let top = self.at(ix, iy) * (256 - fx) + self.at(ix + 1, iy) * fx;
        let bottom = self.at(ix, iy + 1) * (256 - fx) + self.at(ix + 1, iy + 1) * fx;
        (top * (256 - fy) + bottom * fy) >> 16
    }
}

/// Where a mask goes: its `anchor` (in its own pixels) on `at` (in the
/// target's), turned clockwise by `angle` radians about it.
#[derive(Copy, Clone, Debug)]
pub struct Place {
    pub anchor: (f32, f32),
    pub at: (f32, f32),
    pub angle: f32,
}

impl Place {
    /// The target pixels the mask can reach, unclipped.
    pub fn reach(&self, mask: &Mask) -> DamageRect {
        let (c, s) = (libm::cosf(self.angle), libm::sinf(self.angle));
        let (ax, ay) = self.anchor;
        let corners = [
            (-ax, -ay),
            (mask.w as f32 - ax, -ay),
            (-ax, mask.h as f32 - ay),
            (mask.w as f32 - ax, mask.h as f32 - ay),
        ];
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (u, v) in corners {
            let x = self.at.0 + u * c - v * s;
            let y = self.at.1 + u * s + v * c;
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        DamageRect {
            x0: libm::floorf(x0) as i32 - 1,
            y0: libm::floorf(y0) as i32 - 1,
            x1: libm::ceilf(x1) as i32 + 1,
            y1: libm::ceilf(y1) as i32 + 1,
        }
    }
}

/// Lay `mask` down on `dst` as `place` says, in `color` at `opacity` out of
/// 256. Inner loop is integer only: the source position steps by a constant
/// per target pixel.
pub fn blit(dst: &mut Pixmap, mask: &Mask, place: &Place, color: u32, opacity: u32) {
    if opacity == 0 {
        return;
    }
    let reach = place.reach(mask);
    let (x0, x1) = (reach.x0.max(0), reach.x1.min(dst.w - 1));
    let (y0, y1) = (reach.y0.max(0), reach.y1.min(dst.h - 1));
    if x0 > x1 || y0 > y1 {
        return;
    }
    let c = (libm::cosf(place.angle) * 16384.0) as i64;
    let s = (libm::sinf(place.angle) * 16384.0) as i64;
    let at_x = (place.at.0 * 256.0) as i64;
    let at_y = (place.at.1 * 256.0) as i64;
    let anchor_x = (place.anchor.0 * 65536.0) as i64;
    let anchor_y = (place.anchor.1 * 65536.0) as i64;
    // Source position in 65536ths of a mask pixel; one target pixel right is
    // (c, -s) * 256 / 16384 * 256.
    let (step_u, step_v) = ((c * 4) as i32, (-s * 4) as i32);
    for y in y0..=y1 {
        let dy = (((y as i64) << 8) + 128) - at_y;
        let dx = (((x0 as i64) << 8) + 128) - at_x;
        let mut u = (((dx * c + dy * s) >> 6) + anchor_x) as i32;
        let mut v = (((dy * c - dx * s) >> 6) + anchor_y) as i32;
        let row = (y * dst.w) as usize;
        for x in x0..=x1 {
            let cov = mask.sample(u >> 8, v >> 8);
            if cov != 0 {
                let p = &mut dst.px[row + x as usize];
                *p = mix(*p, color, (weight(cov) * opacity) >> 8);
            }
            u += step_u;
            v += step_v;
        }
    }
}
