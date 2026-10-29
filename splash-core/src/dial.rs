//! The ring: twelve segments round a circle, each a round-capped stroke of
//! arc centred on an hour of a clock face, 12 at the top and on clockwise.
//! A pixel's share of a segment is measured at its centre, in 1/256 px and
//! in 1/786432 turn, with integers alone.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::over_backdrop;

pub const SEGMENTS: usize = 12;
/// Every segment, as [`Dial::paint`] selects them.
pub const ALL: u16 = (1 << SEGMENTS) - 1;

/// A pixel, in the units distances count in.
const PX: i64 = 256;
/// A turn, in the units a segment's angles count in: 65536 to a twelfth.
const TURN: i64 = SEGMENTS as i64 * 65536;

/// `atan(z) / 2π` for `z` in `0..=1`, in 65536ths of a turn, as an odd
/// polynomial in `z` whose coefficients carry 16 fractional bits: within
/// 0.0001 rad, a sixtieth of a pixel at a 160 px radius.
const ATAN: [i64; 5] = [
    683_473_678,
    -225_781_269,
    123_138_132,
    -58_193_963,
    14_242_151,
];

/// The sine of each multiple of 15° up to 90°, in 65536ths.
const SIN15: [i64; 7] = [0, 16_962, 32_768, 46_341, 56_756, 63_303, 65_536];

/// The direction of `(dx, dy)`, `y` growing down, clockwise from straight up
/// in 65536ths of a turn.
pub fn turn(dx: i64, dy: i64) -> u16 {
    let (ax, ay) = (dx.unsigned_abs(), dy.unsigned_abs());
    let (lo, hi) = if ax < ay { (ax, ay) } else { (ay, ax) };
    if hi == 0 {
        return 0;
    }
    let z = ((lo << 16) / hi) as i64;
    let z2 = (z * z) >> 16;
    let acc = ATAN[1..]
        .iter()
        .rev()
        .fold(0, |acc, &c| ((acc + c) * z2) >> 16);
    // From +x towards +y, the way a screen turns clockwise.
    let mut a = ((acc + ATAN[0]) * z) >> 32;
    if ax < ay {
        a = 16384 - a;
    }
    if dx < 0 {
        a = 32768 - a;
    }
    if dy < 0 {
        a = -a;
    }
    (a + 16384).rem_euclid(65536) as u16
}

/// The sine of `i` times 15°, in 65536ths.
fn sin15(i: i64) -> i64 {
    let i = i.rem_euclid(24) as usize;
    match i {
        0..=6 => SIN15[i],
        7..=12 => SIN15[12 - i],
        13..=18 => -SIN15[i - 12],
        _ => -SIN15[24 - i],
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Dial {
    /// The centre, on the corner four pixels share, so every quadrant of the
    /// ring is the same pixels mirrored.
    pub cx: i32,
    pub cy: i32,
    /// Of the segments' centre line, in pixels.
    pub radius: i32,
    /// Across a segment, in pixels.
    pub width: i32,
    /// How far each segment's straight run reaches either side of its hour,
    /// in 1/786432 turn; its caps round off past that.
    half_span: i64,
}

impl Dial {
    /// The gap between two segments' caps is as wide as a segment is, and
    /// never under two pixels.
    pub fn new(cx: i32, cy: i32, radius: i32, width: i32) -> Dial {
        let (r, w) = (i64::from(radius.max(1)), i64::from(width.max(1)));
        let gap = w.max(2);
        // A twelfth of the circumference, then less a cap at each end and the
        // gap.
        let pitch = (r * PX * 34_315) >> 16;
        let run = (pitch - (w + gap) * PX).max(0);
        Dial {
            cx,
            cy,
            radius: r as i32,
            width: w as i32,
            half_span: run * 125_165 / (r * PX * 2),
        }
    }

    /// Past this many pixels from the centre, nothing is drawn.
    fn reach(&self) -> i32 {
        self.radius + self.width / 2 + 2
    }

    pub fn bounds(&self) -> DamageRect {
        let reach = self.reach();
        DamageRect {
            x0: self.cx - reach,
            y0: self.cy - reach,
            x1: self.cx + reach - 1,
            y1: self.cy + reach - 1,
        }
    }

    /// The pixels segment `k` may cover: those of the twelfth of the ring
    /// centred on its hour.
    pub fn segment_bounds(&self, k: usize) -> DamageRect {
        let reach = i64::from(self.reach());
        let inner = i64::from((self.radius - self.width / 2 - 2).max(0));
        let mut b = DamageRect {
            x0: i32::MAX,
            y0: i32::MAX,
            x1: i32::MIN,
            y1: i32::MIN,
        };
        let hour = 2 * k as i64;
        for i in [hour - 1, hour, hour + 1] {
            for rho in [reach, inner] {
                let x = self.cx + ((rho * sin15(i)) >> 16) as i32;
                let y = self.cy - ((rho * sin15(i + 6)) >> 16) as i32;
                b.x0 = b.x0.min(x - 1);
                b.y0 = b.y0.min(y - 1);
                b.x1 = b.x1.max(x + 1);
                b.y1 = b.y1.max(y + 1);
            }
        }
        b
    }

    /// The bounds of the segments `which` selects, a bit a segment.
    pub fn bounds_of(&self, which: u16) -> Option<DamageRect> {
        (0..SEGMENTS)
            .filter(|k| which & (1 << k) != 0)
            .map(|k| self.segment_bounds(k))
            .reduce(|a, b| a.union(&b))
    }

    /// The segment pixel `(x, y)` shows, and how much of the pixel it
    /// covers out of 255; `None` for a pixel no segment touches.
    pub fn coverage(&self, x: i32, y: i32) -> Option<(usize, u8)> {
        let dx = i64::from(x - self.cx) * PX + PX / 2;
        let dy = i64::from(y - self.cy) * PX + PX / 2;
        let r = (dx * dx + dy * dy).isqrt();
        let centre = i64::from(self.radius) * PX;
        let half = i64::from(self.width) * PX / 2;
        let across = (r - centre).abs();
        if across >= half + PX / 2 {
            return None;
        }
        let pos = i64::from(turn(dx, dy)) * SEGMENTS as i64;
        let k = ((pos + 32_768) >> 16) as usize % SEGMENTS;
        let mut off = pos - ((k as i64) << 16);
        if off > TURN / 2 {
            off -= TURN;
        }
        let beyond = off.abs() - self.half_span;
        let d = if beyond <= 0 {
            across
        } else {
            // Past the straight run, the distance to the cap's centre: the
            // law of cosines with 1 - cos δ taken as δ²/2, δ being under a
            // twelfth of a turn, in radians with 16 fractional bits.
            let delta = (beyond * 137_258) >> 18;
            let bend = ((((r * centre) >> 16) * delta) >> 16) * delta;
            (across * across + bend).isqrt()
        };
        let covered = (half + PX / 2 - d).clamp(0, PX);
        (covered > 0).then(|| (k, (covered * 255 / PX) as u8))
    }

    /// Call `f(x, y, segment, coverage)` for each pixel of `area` some
    /// segment covers, row by row, never twice.
    pub fn for_each_pixel(&self, area: DamageRect, mut f: impl FnMut(i32, i32, usize, u8)) {
        let b = self.bounds();
        let (y0, y1) = (area.y0.max(b.y0), area.y1.min(b.y1));
        let outer = i64::from(self.radius) * PX + i64::from(self.width) * PX / 2 + PX;
        let inner = i64::from(self.radius) * PX - i64::from(self.width) * PX / 2 - PX;
        for y in y0..=y1 {
            let dy = i64::from(y - self.cy) * PX + PX / 2;
            if dy.abs() > outer {
                continue;
            }
            let xo = ((outer * outer - dy * dy).isqrt() / PX) as i32 + 1;
            let xi = if inner > dy.abs() {
                ((inner * inner - dy * dy).isqrt() / PX) as i32 - 1
            } else {
                0
            };
            let spans = if xi > 0 {
                [(-xo - 1, -xi - 1), (xi, xo)]
            } else {
                [(-xo - 1, xo), (1, 0)]
            };
            for (u0, u1) in spans {
                let x0 = (self.cx + u0).max(area.x0);
                let x1 = (self.cx + u1).min(area.x1);
                for x in x0..=x1 {
                    if let Some((k, cover)) = self.coverage(x, y) {
                        f(x, y, k, cover);
                    }
                }
            }
        }
    }

    /// Draw the segments `which` selects, segment `k` in `inks[k]`, over the
    /// backdrop: each pixel is written whole, so what was under it goes.
    pub fn paint<C: Canvas>(&self, canvas: &mut C, inks: &[Color32; SEGMENTS], which: u16) {
        let Some(area) = self.bounds_of(which) else {
            return;
        };
        let format = canvas.pixel_format();
        let area = area.clip(canvas.width() as i32, canvas.height() as i32);
        self.for_each_pixel(area, |x, y, k, cover| {
            if which & (1 << k) != 0 {
                canvas.put_pixel(x, y, format.encode(over_backdrop(inks[k], cover)));
            }
        });
    }
}
