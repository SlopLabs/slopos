//! Signed distance fields in 64ths of the tile's side. Only IEEE-exact
//! operations (no libm), so CI renders the committed PNGs byte for byte.

use slopos_abi::draw::Color32;

pub type Point = (f32, f32);

/// Negative inside.
#[derive(Clone, Debug)]
pub enum Shape {
    RoundRect { min: Point, max: Point, r: f32 },
    Circle { c: Point, r: f32 },
    Capsule { a: Point, b: Point, r: f32 },
    Polygon(Vec<Point>),
    Union(Box<Shape>, Box<Shape>),
    Intersect(Box<Shape>, Box<Shape>),
    Subtract(Box<Shape>, Box<Shape>),
    Grow(Box<Shape>, f32),
}

pub fn rrect(x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> Shape {
    Shape::RoundRect {
        min: (x0, y0),
        max: (x1, y1),
        r,
    }
}

pub fn circle(cx: f32, cy: f32, r: f32) -> Shape {
    Shape::Circle { c: (cx, cy), r }
}

pub fn capsule(a: Point, b: Point, width: f32) -> Shape {
    Shape::Capsule {
        a,
        b,
        r: width / 2.0,
    }
}

pub fn polyline(points: &[Point], width: f32) -> Shape {
    let mut segments = points.windows(2).map(|w| capsule(w[0], w[1], width));
    let first = segments.next().expect("a polyline has a segment");
    segments.fold(first, Shape::union)
}

pub fn polygon(points: &[Point]) -> Shape {
    Shape::Polygon(points.to_vec())
}

fn length(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

impl Shape {
    pub fn union(self, other: Shape) -> Shape {
        Shape::Union(Box::new(self), Box::new(other))
    }

    pub fn and(self, other: Shape) -> Shape {
        Shape::Intersect(Box::new(self), Box::new(other))
    }

    pub fn minus(self, other: Shape) -> Shape {
        Shape::Subtract(Box::new(self), Box::new(other))
    }

    pub fn grow(self, r: f32) -> Shape {
        Shape::Grow(Box::new(self), r)
    }

    pub fn distance(&self, p: Point) -> f32 {
        match self {
            Self::RoundRect { min, max, r } => {
                let half = ((max.0 - min.0) / 2.0, (max.1 - min.1) / 2.0);
                let qx = (p.0 - (min.0 + half.0)).abs() - half.0 + r;
                let qy = (p.1 - (min.1 + half.1)).abs() - half.1 + r;
                length(qx.max(0.0), qy.max(0.0)) + qx.max(qy).min(0.0) - r
            }
            Self::Circle { c, r } => length(p.0 - c.0, p.1 - c.1) - r,
            Self::Capsule { a, b, r } => {
                let (px, py) = (p.0 - a.0, p.1 - a.1);
                let (ex, ey) = (b.0 - a.0, b.1 - a.1);
                let h = ((px * ex + py * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
                length(px - ex * h, py - ey * h) - r
            }
            Self::Polygon(v) => {
                let mut d = (p.0 - v[0].0) * (p.0 - v[0].0) + (p.1 - v[0].1) * (p.1 - v[0].1);
                let mut sign = 1.0;
                let mut j = v.len() - 1;
                for i in 0..v.len() {
                    let (ex, ey) = (v[j].0 - v[i].0, v[j].1 - v[i].1);
                    let (wx, wy) = (p.0 - v[i].0, p.1 - v[i].1);
                    let h = ((wx * ex + wy * ey) / (ex * ex + ey * ey)).clamp(0.0, 1.0);
                    let (bx, by) = (wx - ex * h, wy - ey * h);
                    d = d.min(bx * bx + by * by);
                    let c = [p.1 >= v[i].1, p.1 < v[j].1, ex * wy > ey * wx];
                    if c.iter().all(|&c| c) || c.iter().all(|&c| !c) {
                        sign = -sign;
                    }
                    j = i;
                }
                sign * d.sqrt()
            }
            Self::Union(a, b) => a.distance(p).min(b.distance(p)),
            Self::Intersect(a, b) => a.distance(p).max(b.distance(p)),
            Self::Subtract(a, b) => a.distance(p).max(-b.distance(p)),
            Self::Grow(s, r) => s.distance(p) - r,
        }
    }
}

/// Premultiplied.
pub struct Canvas {
    size: u32,
    pixels: Vec<[f32; 4]>,
}

impl Canvas {
    pub fn over(size: u32, under: &[Color32]) -> Self {
        let channel = |v: u8| f32::from(v) / 255.0;
        let pixels = under
            .iter()
            .map(|c| {
                let a = channel(c.alpha());
                [
                    channel(c.red()) * a,
                    channel(c.green()) * a,
                    channel(c.blue()) * a,
                    a,
                ]
            })
            .collect();
        Self { size, pixels }
    }

    /// Only where something is beneath, so nothing reaches past the tile.
    pub fn white(&mut self, shape: &Shape, opacity: f32) {
        let s = self.size as f32;
        let unit = s / 64.0;
        for y in 0..self.size {
            for x in 0..self.size {
                let p = ((x as f32 + 0.5) / unit, (y as f32 + 0.5) / unit);
                let coverage = (0.5 - shape.distance(p) * unit).clamp(0.0, 1.0) * opacity;
                if coverage <= 0.0 {
                    continue;
                }
                let dst = &mut self.pixels[(y * self.size + x) as usize];
                let a = dst[3];
                let keep = 1.0 - coverage;
                *dst = [
                    a * coverage + dst[0] * keep,
                    a * coverage + dst[1] * keep,
                    a * coverage + dst[2] * keep,
                    a,
                ];
            }
        }
    }

    pub fn pixels(&self) -> Vec<Color32> {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8;
        self.pixels
            .iter()
            .map(|&[r, g, b, a]| match byte(a) {
                0 => Color32::TRANSPARENT,
                alpha => Color32::new(byte(r / a), byte(g / a), byte(b / a), alpha),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(size: u32, shape: &Shape) -> f32 {
        let black = vec![Color32::new(0, 0, 0, 0xff); (size * size) as usize];
        let mut c = Canvas::over(size, &black);
        c.white(shape, 1.0);
        c.pixels().iter().map(|p| f32::from(p.red()) / 255.0).sum()
    }

    #[test]
    fn a_concave_polygon_on_the_grid_covers_exactly_its_area() {
        let l = polygon(&[
            (16.0, 16.0),
            (32.0, 16.0),
            (32.0, 32.0),
            (48.0, 32.0),
            (48.0, 48.0),
            (16.0, 48.0),
        ]);
        assert_eq!(area(64, &l), 768.0);
        let reversed = polygon(&[
            (16.0, 48.0),
            (48.0, 48.0),
            (48.0, 32.0),
            (32.0, 32.0),
            (32.0, 16.0),
            (16.0, 16.0),
        ]);
        assert_eq!(area(64, &reversed), 768.0);
        assert_eq!(area(128, &l), 4.0 * 768.0);
    }

    #[test]
    fn antialiased_shapes_cover_their_area_at_any_size() {
        let pi = core::f32::consts::PI;
        for size in [64, 128] {
            let px = size as f32 / 64.0;
            let disc = area(size, &circle(30.1, 33.9, 19.2));
            let expected = pi * (19.2 * px) * (19.2 * px);
            assert!(
                (disc - expected).abs() < 0.01 * expected,
                "{size}: {disc} vs {expected}"
            );
            let stadium = area(size, &capsule((19.2, 26.2), (44.8, 26.2), 6.4));
            let expected = (25.6 * 6.4 + pi * 3.2 * 3.2) * px * px;
            assert!(
                (stadium - expected).abs() < 0.01 * expected,
                "{size}: {stadium} vs {expected}"
            );
        }
    }

    #[test]
    fn white_goes_only_where_something_is_beneath() {
        let mut under = vec![Color32::TRANSPARENT; 64 * 64];
        under[0] = Color32::new(0, 0, 0, 0x80);
        let mut c = Canvas::over(64, &under);
        c.white(&rrect(0.0, 0.0, 64.0, 64.0, 0.0), 1.0);
        let out = c.pixels();
        assert_eq!(out[0], Color32::new(0xff, 0xff, 0xff, 0x80));
        assert!(out[1..].iter().all(|p| *p == Color32::TRANSPARENT));
    }
}
