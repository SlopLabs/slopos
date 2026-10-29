//! A rounded rectangle measured per pixel at the pixel's centre, so an edge on
//! a pixel boundary covers whole pixels and a shadow follows the corners.

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::blend::{fill_rect_blended_clipped, put_pixel_coverage};

/// The `w` by `h` rectangle at `(x, y)` whose corners are quarter circles
/// of `radius`, as round as its shorter side allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rounded {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub radius: i32,
}

/// One pixel, in the 1/256 px [`Rounded::distance`] counts in.
const PX: i32 = 256;
const HALF_PX: i32 = PX / 2;

impl Rounded {
    pub const fn new(x: i32, y: i32, w: i32, h: i32, radius: i32) -> Rounded {
        Rounded { x, y, w, h, radius }
    }

    /// A circle of `diameter` with its box's top-left at `(x, y)`.
    pub const fn circle(x: i32, y: i32, diameter: i32) -> Rounded {
        Rounded::new(x, y, diameter, diameter, diameter)
    }

    /// The radius in half pixels, so an odd side rounds whole.
    fn half_radius(&self) -> i32 {
        (2 * self.radius).min(self.w.min(self.h)).max(0)
    }

    /// How far the centre of pixel `(x, y)` lies outside the outline, in
    /// 1/256 px; 0 or less for a centre inside it.
    pub fn distance(&self, x: i32, y: i32) -> i32 {
        let r = self.half_radius();
        let (cx, cy) = (2 * x + 1, 2 * y + 1);
        let off = |c: i32, lo: i32, len: i32| {
            let (inner_lo, inner_hi) = (2 * lo + r, 2 * (lo + len) - r);
            i64::from((inner_lo - c).max(c - inner_hi).max(0))
        };
        let (dx, dy) = (off(cx, self.x, self.w), off(cy, self.y, self.h));
        if dx == 0 && dy == 0 {
            return -r * HALF_PX - HALF_PX;
        }
        let half_px = match (dx, dy) {
            (0, d) | (d, 0) => d * i64::from(HALF_PX),
            _ => ((dx * dx + dy * dy) as u64 * 16_384).isqrt() as i64,
        };
        (half_px - i64::from(r * HALF_PX)).clamp(i32::MIN.into(), i32::MAX.into()) as i32
    }

    /// Out of 255, how much of pixel `(x, y)` the shape covers.
    pub fn coverage(&self, x: i32, y: i32) -> u8 {
        let covered = (HALF_PX - self.distance(x, y)).clamp(0, PX);
        (covered * 255 / PX) as u8
    }

    /// Out of 255 of its darkest, how dark a shadow reaching `spread` px
    /// past the outline is at pixel `(x, y)`: darkest within the outline,
    /// fading with the square of the distance to nothing at `spread`.
    pub fn shadow(&self, spread: i32, x: i32, y: i32) -> u8 {
        if spread <= 0 {
            return 0;
        }
        let reach = i64::from(spread) * PX as i64;
        let left = (reach - i64::from(self.distance(x, y).max(0))).max(0);
        (left * left * 255 / (reach * reach)) as u8
    }

    /// The rectangle holding every pixel a shadow reaching `spread` px past
    /// the outline darkens.
    pub const fn grown(&self, spread: i32) -> Rounded {
        Rounded::new(
            self.x - spread,
            self.y - spread,
            self.w + 2 * spread,
            self.h + 2 * spread,
            self.radius + spread,
        )
    }

    pub const fn bounds(&self) -> DamageRect {
        DamageRect {
            x0: self.x,
            y0: self.y,
            x1: self.x + self.w - 1,
            y1: self.y + self.h - 1,
        }
    }

    /// How far each corner's square reaches along the sides.
    fn corner_side(&self) -> i32 {
        (self.half_radius() + 1) / 2
    }

    /// The square at each corner: the only pixels the shape may cover in
    /// part.
    pub fn corners(&self) -> [DamageRect; 4] {
        let r = self.corner_side();
        let b = self.bounds();
        let square = |x: i32, y: i32| DamageRect {
            x0: x,
            y0: y,
            x1: x + r - 1,
            y1: y + r - 1,
        };
        [
            square(b.x0, b.y0),
            square(b.x1 - r + 1, b.y0),
            square(b.x0, b.y1 - r + 1),
            square(b.x1 - r + 1, b.y1 - r + 1),
        ]
    }

    /// The rectangle less [`corners`](Rounded::corners), as three bands:
    /// pixels the shape covers whole.
    pub fn bands(&self) -> [DamageRect; 3] {
        let r = self.corner_side();
        let b = self.bounds();
        [
            DamageRect {
                x0: b.x0 + r,
                y0: b.y0,
                x1: b.x1 - r,
                y1: b.y0 + r - 1,
            },
            DamageRect {
                x0: b.x0,
                y0: b.y0 + r,
                x1: b.x1,
                y1: b.y1 - r,
            },
            DamageRect {
                x0: b.x0 + r,
                y0: b.y1 - r + 1,
                x1: b.x1 - r,
                y1: b.y1,
            },
        ]
    }

    /// Blend each pixel of `area` within `clip` towards `ink` there, by
    /// how much of it the shape covers.
    pub fn fill<C: Canvas>(
        &self,
        canvas: &mut C,
        area: &DamageRect,
        clip: &DamageRect,
        ink: impl Fn(i32, i32) -> Color32,
    ) {
        for y in area.y0.max(clip.y0)..=area.y1.min(clip.y1) {
            for x in area.x0.max(clip.x0)..=area.x1.min(clip.x1) {
                put_pixel_coverage(canvas, x, y, ink(x, y), self.coverage(x, y));
            }
        }
    }

    /// The shape in `color` within `clip`.
    pub fn draw<C: Canvas>(&self, canvas: &mut C, clip: &DamageRect, color: Color32) {
        for band in self.bands() {
            let (w, h) = (band.x1 - band.x0 + 1, band.y1 - band.y0 + 1);
            fill_rect_blended_clipped(canvas, band.x0, band.y0, w, h, color, clip);
        }
        for corner in self.corners() {
            self.fill(canvas, &corner, clip, |_, _| color);
        }
    }

    /// The band `width` px wide just inside the outline, in `color` within
    /// `clip`.
    pub fn stroke<C: Canvas>(&self, canvas: &mut C, width: i32, clip: &DamageRect, color: Color32) {
        self.stroke_with(canvas, width, clip, |_, _| color);
    }

    pub fn stroke_with<C: Canvas>(
        &self,
        canvas: &mut C,
        width: i32,
        clip: &DamageRect,
        ink: impl Fn(i32, i32) -> Color32,
    ) {
        let inner = Rounded::new(
            self.x + width,
            self.y + width,
            self.w - 2 * width,
            self.h - 2 * width,
            (self.radius - width).max(0),
        );
        let reach = self.corner_side().max(width);
        let b = self.bounds();
        let (top, bottom) = (
            (b.y0 + reach - 1).min(b.y1),
            (b.y1 - reach + 1).max(b.y0 + reach),
        );
        let (left, right) = (
            (b.x0 + reach - 1).min(b.x1),
            (b.x1 - reach + 1).max(b.x0 + reach),
        );
        let rim = [
            DamageRect { y1: top, ..b },
            DamageRect { y0: bottom, ..b },
            DamageRect {
                y0: top + 1,
                y1: bottom - 1,
                x1: left,
                ..b
            },
            DamageRect {
                y0: top + 1,
                y1: bottom - 1,
                x0: right,
                ..b
            },
        ];
        for part in rim {
            for y in part.y0.max(clip.y0)..=part.y1.min(clip.y1) {
                for x in part.x0.max(clip.x0)..=part.x1.min(clip.x1) {
                    let covered = self.coverage(x, y).saturating_sub(inner.coverage(x, y));
                    put_pixel_coverage(canvas, x, y, ink(x, y), covered);
                }
            }
        }
    }

    /// Fades from `color` at the top edge to nothing `fade` px below it.
    pub fn top_rim<C: Canvas>(&self, canvas: &mut C, clip: &DamageRect, color: Color32, fade: i32) {
        if fade <= 0 {
            return;
        }
        let clip = DamageRect {
            y1: clip.y1.min(self.y + fade - 1),
            ..*clip
        };
        let alpha = i32::from(color.alpha());
        self.stroke_with(canvas, 1, &clip, |_, y| {
            let left = (fade - (y - self.y)).clamp(0, fade);
            let a = (alpha * left / fade) as u8;
            Color32::new(color.red(), color.green(), color.blue(), a)
        });
    }

    /// The shadow the shape casts `spread` px past its outline, `color` at
    /// its darkest, within `clip` less `under`, which something opaque is
    /// drawn over next.
    pub fn draw_shadow<C: Canvas>(
        &self,
        canvas: &mut C,
        spread: i32,
        clip: &DamageRect,
        under: &DamageRect,
        color: Color32,
    ) {
        let area = self.grown(spread).bounds();
        for y in area.y0.max(clip.y0)..=area.y1.min(clip.y1) {
            for x in area.x0.max(clip.x0)..=area.x1.min(clip.x1) {
                if !under.contains(x, y) {
                    put_pixel_coverage(canvas, x, y, color, self.shadow(spread, x, y));
                }
            }
        }
    }

    /// Leaves the shadow out from under `pane`, which is translucent.
    pub fn draw_shadow_around<C: Canvas>(
        &self,
        canvas: &mut C,
        spread: i32,
        clip: &DamageRect,
        pane: &Rounded,
        color: Color32,
    ) {
        let area = self.grown(spread).bounds();
        let whole = pane.bands()[1];
        for y in area.y0.max(clip.y0)..=area.y1.min(clip.y1) {
            for x in area.x0.max(clip.x0)..=area.x1.min(clip.x1) {
                if whole.contains(x, y) {
                    continue;
                }
                let open = 255 - u32::from(pane.coverage(x, y));
                let dark = u32::from(self.shadow(spread, x, y)) * open / 255;
                put_pixel_coverage(canvas, x, y, color, dark as u8);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(shape: &Rounded) -> u64 {
        let r = *shape;
        (r.y - 2..r.y + r.h + 2)
            .flat_map(|y| (r.x - 2..r.x + r.w + 2).map(move |x| (x, y)))
            .map(|(x, y)| u64::from(shape.coverage(x, y)))
            .sum()
    }

    #[test]
    fn edges_on_pixel_boundaries_cover_whole_pixels() {
        let shape = Rounded::new(10, 20, 100, 60, 10);
        for y in 30..70 {
            assert_eq!(shape.coverage(10, y), 255);
            assert_eq!(shape.coverage(9, y), 0);
            assert_eq!(shape.coverage(109, y), 255);
            assert_eq!(shape.coverage(110, y), 0);
        }
        for x in 20..90 {
            assert_eq!(shape.coverage(x, 20), 255);
            assert_eq!(shape.coverage(x, 19), 0);
            assert_eq!(shape.coverage(x, 79), 255);
            assert_eq!(shape.coverage(x, 80), 0);
        }
    }

    #[test]
    fn a_square_corner_covers_its_corner_pixel() {
        let shape = Rounded::new(0, 0, 8, 8, 0);
        assert_eq!(shape.coverage(0, 0), 255);
        assert_eq!(shape.coverage(7, 7), 255);
        assert_eq!(shape.coverage(8, 7), 0);
    }

    #[test]
    fn corners_cut_the_area_a_quarter_circle_leaves() {
        for r in [4, 6, 10, 15, 30] {
            let shape = Rounded::new(3, 5, 80, 70, r);
            let exact = 80.0 * 70.0 - (4.0 - core::f64::consts::PI) * f64::from(r * r);
            let got = total(&shape) as f64 / 255.0;
            assert!(
                (got - exact).abs() < 1.0 + f64::from(r) * 0.05,
                "r {r}: {got} vs {exact}"
            );
        }
    }

    #[test]
    fn a_circle_is_symmetric_and_antialiased() {
        let circle = Rounded::circle(0, 0, 12);
        let mut partial = 0;
        for y in 0..12 {
            for x in 0..12 {
                let c = circle.coverage(x, y);
                assert_eq!(c, circle.coverage(11 - x, y));
                assert_eq!(c, circle.coverage(x, 11 - y));
                assert_eq!(c, circle.coverage(y, x));
                partial += i32::from(c > 0 && c < 255);
            }
        }
        assert!(partial >= 12, "{partial} edge pixels blended");
        let area = total(&circle) as f64 / 255.0;
        assert!((area - 36.0 * core::f64::consts::PI).abs() < 1.5, "{area}");
    }

    #[test]
    fn a_shadow_fades_out_at_its_spread_and_follows_the_corner() {
        let shape = Rounded::new(100, 100, 200, 150, 10);
        assert_eq!(shape.shadow(12, 150, 150), 255);
        assert_eq!(
            u32::from(shape.shadow(12, 150, 99)),
            255 * 23 * 23 / (24 * 24)
        );
        assert_eq!(shape.shadow(12, 150, 100 - 12), 0);
        let mut last = 255;
        for d in 0..14 {
            let now = shape.shadow(12, 300 + d, 160);
            assert!(now <= last, "{d}");
            last = now;
        }
        assert_eq!(last, 0);
        let along_edge = shape.shadow(12, 150, 95);
        let past_corner = shape.shadow(12, 95, 95);
        assert!(past_corner < along_edge, "{past_corner} {along_edge}");
        assert_eq!(shape.shadow(12, 95, 95), shape.shadow(12, 304, 95));
    }

    #[test]
    fn the_shadow_stays_within_its_bounds() {
        let shape = Rounded::new(40, 40, 120, 90, 10);
        let bounds = shape.grown(12).bounds();
        for y in bounds.y0 - 4..=bounds.y1 + 4 {
            for x in bounds.x0 - 4..=bounds.x1 + 4 {
                if !bounds.contains(x, y) {
                    assert_eq!(shape.shadow(12, x, y), 0, "({x}, {y})");
                }
            }
        }
    }

    #[test]
    fn an_odd_circle_is_round_rather_than_flat() {
        let circle = Rounded::circle(0, 0, 11);
        for y in 0..11 {
            for x in 0..11 {
                let c = circle.coverage(x, y);
                assert_eq!(c, circle.coverage(10 - x, y));
                assert_eq!(c, circle.coverage(y, x));
            }
        }
        assert_eq!((circle.coverage(5, 0), circle.coverage(0, 0)), (255, 0));
        assert!(circle.coverage(4, 0) < 255, "a flat top");
        let area = total(&circle) as f64 / 255.0;
        assert!((area - 30.25 * core::f64::consts::PI).abs() < 1.5, "{area}");
    }

    #[test]
    fn a_shape_wider_than_tall_or_empty_measures_without_panicking() {
        assert_eq!(Rounded::new(0, 0, -4, 3, 8).coverage(0, 0), 0);
        assert_eq!(Rounded::new(0, 0, 0, 0, 8).coverage(0, 0), 0);
    }

    #[test]
    fn the_bands_are_whole_and_the_corners_hold_every_partial_pixel() {
        for (w, h, r) in [
            (80, 60, 10),
            (41, 23, 7),
            (12, 12, 6),
            (9, 9, 9),
            (30, 20, 0),
        ] {
            let shape = Rounded::new(5, 7, w, h, r);
            let in_rect = |rect: &DamageRect, x: i32, y: i32| rect.contains(x, y);
            for y in shape.y..shape.y + h {
                for x in shape.x..shape.x + w {
                    let bands = shape.bands().iter().filter(|b| in_rect(b, x, y)).count();
                    let corners = shape.corners().iter().filter(|c| in_rect(c, x, y)).count();
                    assert_eq!(bands + corners.min(1), 1, "({w}x{h} r{r}) ({x}, {y})");
                    if bands == 1 {
                        assert_eq!(shape.coverage(x, y), 255, "({w}x{h} r{r}) ({x}, {y})");
                    }
                }
            }
        }
    }

    fn canvas<R>(
        bytes: &mut [u8],
        side: u32,
        paint: impl FnOnce(&mut crate::DrawBuffer<'_>) -> R,
    ) -> R {
        let mut buf = crate::DrawBuffer::new(bytes, side, side, side as usize * 4, 4).unwrap();
        buf.set_pixel_format(slopos_abi::pixel::PixelFormat::Argb8888);
        paint(&mut buf)
    }

    fn pixel(bytes: &[u8], side: usize, x: usize, y: usize) -> Color32 {
        let at = (y * side + x) * 4;
        let raw = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        slopos_abi::pixel::PixelFormat::Argb8888.decode(raw)
    }

    const WHOLE: DamageRect = DamageRect {
        x0: 0,
        y0: 0,
        x1: 63,
        y1: 63,
    };

    #[test]
    fn a_drawn_shape_matches_its_coverage_everywhere() {
        let mut bytes = [0u8; 64 * 64 * 4];
        let shape = Rounded::new(5, 7, 41, 30, 9);
        canvas(&mut bytes, 64, |b| shape.draw(b, &WHOLE, Color32::WHITE));
        for y in 0..64 {
            for x in 0..64 {
                let red = pixel(&bytes, 64, x, y).red();
                assert_eq!(red, shape.coverage(x as i32, y as i32), "({x}, {y})");
            }
        }
    }

    #[test]
    fn a_stroke_is_the_shape_less_the_one_inside_it() {
        for (r, width) in [(9, 1), (6, 2), (0, 1), (2, 3)] {
            let mut bytes = [0u8; 64 * 64 * 4];
            let shape = Rounded::new(5, 7, 41, 30, r);
            let inner = Rounded::new(
                5 + width,
                7 + width,
                41 - 2 * width,
                30 - 2 * width,
                r - width,
            );
            canvas(&mut bytes, 64, |b| {
                shape.stroke(b, width, &WHOLE, Color32::WHITE)
            });
            for y in 0..64 {
                for x in 0..64 {
                    let (x, y) = (x as i32, y as i32);
                    let want = shape.coverage(x, y).saturating_sub(inner.coverage(x, y));
                    let red = pixel(&bytes, 64, x as usize, y as usize).red();
                    assert!(
                        red.abs_diff(want) <= 1,
                        "r{r} w{width} ({x}, {y}): {red} vs {want}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_shadow_leaves_what_lies_under_the_shape_alone() {
        let mut bytes = [0u8; 64 * 64 * 4];
        let shape = Rounded::new(16, 16, 32, 24, 6);
        let ink = Color32::new(255, 255, 255, 255);
        canvas(&mut bytes, 64, |b| {
            shape.draw_shadow(b, 8, &WHOLE, &shape.bounds(), ink)
        });
        assert_eq!(pixel(&bytes, 64, 30, 30).red(), 0);
        assert_eq!(pixel(&bytes, 64, 30, 15).red(), shape.shadow(8, 30, 15));
        assert_eq!(pixel(&bytes, 64, 30, 63).red(), 0);
    }

    #[test]
    fn a_fill_blends_by_coverage_within_its_clip() {
        use crate::DrawBuffer;
        use slopos_abi::pixel::PixelFormat;
        let mut bytes = [0u8; 16 * 16 * 4];
        let mut buf = DrawBuffer::new(&mut bytes, 16, 16, 64, 4).unwrap();
        buf.set_pixel_format(PixelFormat::Argb8888);
        let shape = Rounded::new(2, 2, 12, 12, 4);
        let clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: 7,
            y1: 15,
        };
        shape.fill(&mut buf, &shape.bounds(), &clip, |_, _| Color32::WHITE);
        let red = |x: i32, y: i32| {
            let raw = buf.read_encoded_at(y as usize * 64 + x as usize * 4);
            PixelFormat::Argb8888.decode(raw).red()
        };
        assert_eq!(red(7, 8), 255);
        assert_eq!(red(8, 8), 0);
        assert_eq!(red(2, 2), shape.coverage(2, 2));
        assert_eq!(red(1, 8), 0);
    }
}
