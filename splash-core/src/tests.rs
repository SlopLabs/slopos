use std::collections::HashSet;
use std::vec;
use std::vec::Vec;

use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

use crate::dial::{self, ALL, Dial, SEGMENTS};
use crate::{Layout, boot_changed, boot_inks, boot_level, lit, logo, unlit};

const SCREENS: [(u32, u32); 7] = [
    (640, 480),
    (800, 600),
    (1280, 800),
    (1920, 1080),
    (2560, 1600),
    (3840, 2160),
    (1080, 1920),
];

struct Pixels {
    w: i32,
    h: i32,
    px: Vec<u32>,
}

impl Pixels {
    fn new(w: i32, h: i32, fill: u32) -> Pixels {
        Pixels {
            w,
            h,
            px: vec![fill; (w * h) as usize],
        }
    }
}

impl Canvas for Pixels {
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
        PixelFormat::Argb8888
    }
    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        self.px[byte_offset / 4] = pixel.0;
    }
    fn read_encoded_at(&self, byte_offset: usize) -> u32 {
        self.px[byte_offset / 4]
    }
}

fn covered(d: &Dial) -> Vec<(i32, i32, usize, u8)> {
    let b = d.bounds();
    let mut out = Vec::new();
    for y in b.y0 - 2..=b.y1 + 2 {
        for x in b.x0 - 2..=b.x1 + 2 {
            if let Some((k, c)) = d.coverage(x, y) {
                out.push((x, y, k, c));
            }
        }
    }
    out
}

#[test]
fn turn_agrees_with_atan2() {
    for dy in (-600i64..=600).step_by(7) {
        for dx in (-600i64..=600).step_by(11) {
            if dx == 0 && dy == 0 {
                continue;
            }
            let want = (dx as f64)
                .atan2(-dy as f64)
                .rem_euclid(core::f64::consts::TAU)
                / core::f64::consts::TAU
                * 65536.0;
            let got = f64::from(dial::turn(dx, dy));
            let miss = (got - want).abs();
            assert!(
                miss.min(65536.0 - miss) <= 2.0,
                "({dx}, {dy}): {got} for {want}"
            );
        }
    }
}

#[test]
fn the_ring_is_the_same_all_the_way_round() {
    for (w, h) in SCREENS {
        let d = Layout::for_screen(w, h).dial;
        let pixels = covered(&d);
        let at: std::collections::HashMap<(i32, i32), (usize, u8)> = pixels
            .iter()
            .map(|&(x, y, k, c)| ((x, y), (k, c)))
            .collect();
        for &(x, y, k, c) in &pixels {
            let (u, v) = (x - d.cx, y - d.cy);
            // A quarter turn clockwise about the centre's pixel corner, and a
            // mirror across the vertical through it.
            for ((x2, y2), k2) in [
                ((d.cx - v - 1, d.cy + u), (k + 3) % SEGMENTS),
                ((d.cx - u - 1, y), (SEGMENTS - k) % SEGMENTS),
            ] {
                let (k3, c2) = at.get(&(x2, y2)).copied().unwrap_or((k2, 0));
                assert_eq!(k3, k2, "{w}x{h}: ({x}, {y}) of {k}");
                assert!(c.abs_diff(c2) <= 2, "{w}x{h}: ({x}, {y}) {c} against {c2}");
            }
        }
        let mut ink = [0u64; SEGMENTS];
        for &(_, _, k, c) in &pixels {
            ink[k] += u64::from(c);
        }
        let (lo, hi) = (ink.iter().min().unwrap(), ink.iter().max().unwrap());
        assert!(hi - lo <= hi / 200, "{w}x{h}: segments differ, {ink:?}");
    }
}

#[test]
fn neighbouring_segments_never_touch() {
    for (w, h) in SCREENS {
        let d = Layout::for_screen(w, h).dial;
        let pixels = covered(&d);
        for k in 0..SEGMENTS {
            let next = (k + 1) % SEGMENTS;
            let a: Vec<_> = pixels.iter().filter(|p| p.2 == k).collect();
            let b: Vec<_> = pixels.iter().filter(|p| p.2 == next).collect();
            let nearest = a
                .iter()
                .flat_map(|p| {
                    b.iter()
                        .map(move |q| (p.0 - q.0).pow(2) + (p.1 - q.1).pow(2))
                })
                .min()
                .unwrap();
            assert!(
                nearest >= 4,
                "{w}x{h}: segments {k} and {next} {nearest} apart"
            );
        }
    }
}

#[test]
fn each_segment_keeps_to_its_bounds() {
    for (w, h) in SCREENS {
        let d = Layout::for_screen(w, h).dial;
        for (x, y, k, _) in covered(&d) {
            assert!(
                d.segment_bounds(k).contains(x, y),
                "{w}x{h}: ({x}, {y}) of {k}"
            );
            assert!(d.bounds().contains(x, y), "{w}x{h}: ({x}, {y})");
        }
    }
}

#[test]
fn every_covered_pixel_is_visited_once() {
    for (w, h) in SCREENS {
        let d = Layout::for_screen(w, h).dial;
        let want: HashSet<_> = covered(&d).into_iter().collect();
        let mut seen = HashSet::new();
        d.for_each_pixel(d.bounds(), |x, y, k, c| {
            assert!(seen.insert((x, y, k, c)), "{w}x{h}: ({x}, {y}) twice");
        });
        assert_eq!(seen, want, "{w}x{h}");
    }
}

#[test]
fn paint_draws_only_the_segments_asked_for() {
    let (w, h) = (800, 600);
    let d = Layout::for_screen(w, h).dial;
    let sentinel = 0x1234_5678;
    let mut canvas = Pixels::new(w as i32, h as i32, sentinel);
    let which = 1 << 2 | 1 << 7;
    let inks = [Color32::WHITE; SEGMENTS];
    d.paint(&mut canvas, &inks, which);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let px = canvas.px[(y * w as i32 + x) as usize];
            match d.coverage(x, y) {
                Some((k, c)) if which & (1 << k) != 0 => {
                    let want =
                        PixelFormat::Argb8888.encode(crate::over_backdrop(Color32::WHITE, c));
                    assert_eq!(px, want.0, "({x}, {y}) of {k}");
                }
                _ => assert_eq!(px, sentinel, "({x}, {y}) painted"),
            }
        }
    }
}

#[test]
fn boot_lights_the_ring_clockwise_from_the_top() {
    assert!((0..SEGMENTS).all(|k| boot_level(0, k) == 0));
    assert!((0..SEGMENTS).all(|k| boot_level(100, k) == 255));
    assert_eq!(boot_inks(0), [unlit(); SEGMENTS]);
    assert_eq!(boot_inks(100), [lit(); SEGMENTS]);
    for p in 0..=100 {
        for k in 0..SEGMENTS {
            assert!(
                boot_level(p, k) <= boot_level(p + 1, k),
                "{p}% segment {k} dims"
            );
            if k + 1 < SEGMENTS && boot_level(p, k + 1) > 0 {
                assert_eq!(boot_level(p, k), 255, "{p}%: {} before {k}", k + 1);
            }
        }
        let changed = boot_changed(p, p + 1);
        for k in 0..SEGMENTS {
            let moved = boot_level(p, k) != boot_level(p + 1, k);
            assert_eq!(changed & (1 << k) != 0, moved, "{p}% segment {k}");
        }
    }
    assert_eq!(boot_changed(0, 100), ALL);
}

#[test]
fn the_logo_is_assets_logo_png() {
    let png = slopos_image::load_path(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../assets/logo.png"),
        Default::default(),
    )
    .unwrap();
    assert_eq!(
        (png.width, png.height),
        (logo::WIDTH as u32, logo::HEIGHT as u32)
    );
    for y in 0..logo::HEIGHT {
        for x in 0..logo::WIDTH {
            let want = png.pixels[y * logo::WIDTH + x];
            let want = (want.alpha() != 0).then_some(Color32(want.0 | 0xFF00_0000));
            assert_eq!(logo::pixel(x, y), want, "({x}, {y})");
        }
    }
}

#[test]
fn the_ring_and_the_logo_fit_every_screen_apart() {
    for (w, h) in SCREENS.into_iter().chain([(320, 200), (1024, 768)]) {
        let l = Layout::for_screen(w, h);
        let (ring, mark) = (l.dial.bounds(), l.logo.bounds());
        for b in [ring, mark] {
            assert!(
                b.x0 >= 0 && b.y0 >= 0 && b.x1 < w as i32 && b.y1 < h as i32,
                "{w}x{h}: {b:?} off screen"
            );
        }
        // The wheel sets its numbers and a line of words in between.
        let room = mark.y0 - ring.y1;
        assert!(
            room >= l.dial.radius * 2 / 3,
            "{w}x{h}: {room} px between ring and logo"
        );
    }
}
