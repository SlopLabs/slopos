use std::vec;
use std::vec::Vec;

use slopos_abi::damage::DamageRect;
use slopos_abi::pixel::PixelFormat;
use slopos_splash_core::{self as splash, ALL, BACKDROP, SEGMENTS};
use slopos_theme_core::{self as theme, contrast};

use crate::layout;
use crate::scene::{self, Damage, Fonts, Scene, Screen};
use crate::spin::{self, Phase, Spin};
use crate::wheel::{self, POCKETS};

const REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");
const FORMAT: PixelFormat = PixelFormat::Xrgb8888;

/// Fates spread over the whole `u32` range, both parities.
fn fates() -> impl Iterator<Item = u32> {
    (0..4096u32).map(|i| i.wrapping_mul(0x9E37_79B9) ^ (i >> 3))
}

fn fonts() -> Fonts<'static> {
    Fonts::new(Some(REGULAR), Some(SEMIBOLD))
}

struct Frame {
    w: i32,
    h: i32,
    px: Vec<u8>,
}

impl Frame {
    fn new(w: u32, h: u32) -> Frame {
        Frame {
            w: w as i32,
            h: h as i32,
            px: vec![0x5A; (w * h * 4) as usize],
        }
    }

    fn screen(&mut self) -> Screen<'_> {
        Screen {
            px: &mut self.px,
            width: self.w,
            height: self.h,
            pitch: self.w as usize * 4,
            format: FORMAT,
        }
    }

    fn at(&self, x: i32, y: i32) -> u32 {
        let i = (y * self.w + x) as usize * 4;
        u32::from_le_bytes([self.px[i], self.px[i + 1], self.px[i + 2], self.px[i + 3]])
    }
}

fn scene_on(frame: &mut Frame, fate: u32) -> Scene<'static> {
    let l = layout::layout(frame.w as u32, frame.h as u32);
    Scene::new(l, &frame.screen(), fate, fonts())
}

#[test]
fn numbers_follow_the_clock_and_alternate() {
    let mut seen = [false; POCKETS + 1];
    for k in 0..POCKETS {
        let n = wheel::number(k);
        assert!(!seen[n as usize], "{n} twice");
        seen[n as usize] = true;
        let next = wheel::number(k + 1);
        assert_ne!(wheel::starts(n), wheel::starts(next), "{n} beside {next}");
    }
    assert!(seen[1..].iter().all(|s| *s));
    assert_eq!(wheel::number(0), 12, "12 is at the top");
    assert_eq!(wheel::number(3), 3, "3 is a quarter turn round");
}

#[test]
fn a_fate_lands_on_its_own_parity() {
    for fate in fates() {
        let n = wheel::number(wheel::landing(fate));
        assert_eq!(wheel::starts(n), wheel::fate_starts(fate), "fate {fate:#x}");
    }
}

#[test]
fn every_pocket_can_be_landed_on() {
    let mut hit = [false; POCKETS];
    for fate in fates() {
        hit[wheel::landing(fate)] = true;
    }
    assert!(hit.iter().all(|h| *h), "{hit:?}");
}

#[test]
fn phases_follow_one_another() {
    let order = [
        Phase::Intro,
        Phase::Chasing,
        Phase::Settling,
        Phase::Holding,
        Phase::Fading,
        Phase::Done,
    ];
    let mut at = 0;
    for t in 0..spin::END_MS + 100 {
        let p = spin::phase(t);
        while order[at] != p {
            at += 1;
            assert!(at < order.len(), "{p:?} out of order at {t} ms");
        }
    }
    assert_eq!(at, order.len() - 1);
}

#[test]
fn the_light_starts_at_the_top_and_stops_on_the_fate() {
    for fate in fates().take(256) {
        let s = Spin::new(fate);
        assert_eq!(s.head(0) % POCKETS as u32, 0, "fate {fate:#x}");
        assert_eq!(s.head(spin::INTRO_MS) % POCKETS as u32, 0, "fate {fate:#x}");
        let end = s.head(spin::LAND_MS);
        assert_eq!(end as usize % POCKETS, s.pocket(), "fate {fate:#x}");
        assert_eq!(
            s.head(spin::END_MS),
            end,
            "fate {fate:#x} moves after landing"
        );
        assert!(
            end >= 4 * POCKETS as u32,
            "fate {fate:#x} runs {end} segments"
        );
    }
}

#[test]
fn the_light_never_runs_back_and_hops_at_most_a_segment_a_frame() {
    for fate in fates().take(64) {
        let s = Spin::new(fate);
        let mut last = s.position(0);
        for t in (0..spin::END_MS).step_by(16) {
            let now = s.position(t);
            assert!(now >= last, "fate {fate:#x} runs back at {t} ms");
            assert!(
                now - last <= 0.75,
                "fate {fate:#x} skips {} at {t} ms",
                now - last
            );
            last = now;
        }
    }
}

#[test]
fn the_light_slows_to_a_visible_stop() {
    for fate in fates().take(64) {
        let s = Spin::new(fate);
        let mut hops = Vec::new();
        let mut head = s.head(0);
        for t in spin::INTRO_MS..=spin::LAND_MS {
            if s.head(t) != head {
                head = s.head(t);
                hops.push(t);
            }
        }
        let gaps: Vec<u32> = hops.windows(2).map(|w| w[1] - w[0]).collect();
        let tail = &gaps[gaps.len() - 8..];
        assert!(
            tail.windows(2).all(|w| w[1] >= w[0]),
            "fate {fate:#x}: the last hops {tail:?} speed up"
        );
        assert!(
            *tail.last().unwrap() >= 120,
            "fate {fate:#x}: last hop {tail:?}"
        );
        let rest = spin::LAND_MS - hops.last().unwrap();
        assert!(rest >= 200, "fate {fate:#x} rests {rest} ms before landing");
    }
}

#[test]
fn the_light_glows_whole_and_fades_behind_it() {
    let s = Spin::new(0x1234_5671);
    for t in (spin::INTRO_MS..spin::LAND_MS).step_by(37) {
        let head = s.head(t) as usize % POCKETS;
        assert_eq!(s.glow(t, head), 1.0, "{t} ms");
        let behind: Vec<f32> = (1..POCKETS)
            .map(|d| s.glow(t, (head + POCKETS - d) % POCKETS))
            .collect();
        assert!(
            behind.windows(2).all(|w| w[0] >= w[1]),
            "{t} ms: the trail {behind:?} brightens away from the light"
        );
        assert!(behind[0] < 1.0, "{t} ms");
    }
    let settled = spin::LAND_MS + 1000;
    for k in (0..POCKETS).filter(|&k| k != s.pocket()) {
        assert!(s.glow(settled, k) < 1.0 / 255.0, "segment {k} still lit");
    }
    assert_eq!(s.glow(settled, s.pocket()), 1.0);
}

#[test]
fn decimal_verdict_and_caption_read_as_words() {
    let mut buf = [0u8; 3];
    assert_eq!(scene::decimal(12, &mut buf), "12");
    assert_eq!(scene::decimal(7, &mut buf), "7");
    let mut line = [0u8; 64];
    assert_eq!(scene::verdict(7, &mut line), "7 \u{2014} odd. You win.");
    assert_eq!(
        scene::verdict(12, &mut line),
        "12 \u{2014} even. The house wins."
    );
    assert_eq!(scene::caption(true), scene::words::ODD_STARTS);
    assert_eq!(scene::caption(false), scene::words::EVEN_RESTARTS);
}

#[test]
fn the_words_read_against_black() {
    let t = theme::current();
    for ink in [t.text_secondary, t.success, t.danger, splash::lit()] {
        let ratio = contrast::ratio(ink, BACKDROP);
        assert!(ratio >= contrast::TEXT, "{ink:?} at {ratio:.2}:1");
    }
    let numeral = scene::numeral_ink();
    let ratio = contrast::ratio(numeral, BACKDROP);
    assert!(ratio >= contrast::TEXT, "a pocket's number at {ratio:.2}:1");
}

#[test]
fn everything_fits_the_screen_apart() {
    for (w, h) in [
        (640, 480),
        (800, 600),
        (1024, 768),
        (1280, 800),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
        (1080, 1920),
    ] {
        let l = layout::layout(w, h);
        assert!(l.numerals.is_some(), "{w}x{h} shows no numbers");
        assert!(l.caption.is_some(), "{w}x{h} shows no words");
        let mut frame = Frame::new(w, h);
        let mut scene = scene_on(&mut frame, 0x2468_ACE1);
        scene.paint_all(&mut frame.screen());
        let areas: Vec<DamageRect> = scene.label_areas().collect();
        let dial = l.boot.dial;
        let ring_out = dial.radius + dial.width / 2 + 1;
        let ring_in = dial.radius - dial.width / 2 - 1;
        for a in &areas {
            assert!(
                a.x0 >= 0 && a.y0 >= 0 && a.x1 < w as i32 && a.y1 < h as i32,
                "{w}x{h}: {a:?} off screen"
            );
            assert!(
                !a.intersects(&l.boot.logo.bounds()),
                "{w}x{h}: {a:?} on the logo"
            );
            // Its nearest and furthest points from the ring's centre.
            let near = |lo: i32, hi: i32, c: i32| (lo - c).max(c - hi - 1).max(0);
            let far = |lo: i32, hi: i32, c: i32| (c - lo).max(hi + 1 - c);
            let (nx, ny) = (near(a.x0, a.x1, dial.cx), near(a.y0, a.y1, dial.cy));
            let (fx, fy) = (far(a.x0, a.x1, dial.cx), far(a.y0, a.y1, dial.cy));
            let clear =
                nx * nx + ny * ny >= ring_out * ring_out || fx * fx + fy * fy <= ring_in * ring_in;
            assert!(clear, "{w}x{h}: {a:?} crosses the ring");
        }
        for (i, a) in areas.iter().enumerate() {
            for b in &areas[i + 1..] {
                let same_line = a.y0 == b.y0 && a.y1 == b.y1;
                assert!(same_line || !a.intersects(b), "{w}x{h}: {a:?} on {b:?}");
            }
        }
    }
}

#[test]
fn the_first_frame_is_the_splash_s_last() {
    for (w, h) in [(800, 600), (1920, 1080)] {
        let mut splash = Frame::new(w, h);
        {
            let mut screen = splash.screen();
            let boot = splash::Layout::for_screen(w, h);
            screen
                .px
                .chunks_exact_mut(4)
                .for_each(|p| p.copy_from_slice(&FORMAT.encode(BACKDROP).0.to_le_bytes()));
            boot.dial.paint(&mut screen, &splash::boot_inks(100), ALL);
            boot.logo.paint(&mut screen, u8::MAX);
        }
        let mut frame = Frame::new(w, h);
        let mut scene = scene_on(&mut frame, 7);
        scene.paint_all(&mut frame.screen());
        assert!(frame.px == splash.px, "{w}x{h}");
        assert!(scene.paint(&mut frame.screen(), 0).is_empty(), "{w}x{h}");
        assert!(frame.px == splash.px, "{w}x{h}");
    }
}

/// Every pixel that differs between `before` and `after` lies in `damage`.
fn damaged_only(before: &Frame, after: &Frame, damage: &Damage) -> Option<(i32, i32)> {
    for y in 0..after.h {
        for x in 0..after.w {
            if before.at(x, y) != after.at(x, y) && !damage.rects().any(|r| r.contains(x, y)) {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn a_frame_damages_only_what_it_changed_and_the_wheel_ends_black() {
    for fate in [0x0000_0003, 0x0000_0008] {
        let mut frame = Frame::new(640, 480);
        let mut scene = scene_on(&mut frame, fate);
        scene.paint_all(&mut frame.screen());
        let mut t = 0;
        while t <= spin::END_MS {
            let before = Frame {
                px: frame.px.clone(),
                ..frame
            };
            let damage = scene.paint(&mut frame.screen(), t);
            if let Some((x, y)) = damaged_only(&before, &frame, &damage) {
                panic!("fate {fate:#x}: ({x}, {y}) changed outside {damage:?} at {t} ms");
            }
            t += 29;
        }
        scene.paint(&mut frame.screen(), spin::END_MS);
        let black = FORMAT.encode(BACKDROP).0;
        assert!(
            (0..frame.h).all(|y| (0..frame.w).all(|x| frame.at(x, y) == black)),
            "fate {fate:#x} leaves something on screen"
        );
    }
}

#[test]
fn the_ring_shows_the_pocket_the_kernel_decided() {
    for fate in fates().take(24) {
        let mut frame = Frame::new(800, 600);
        let mut scene = scene_on(&mut frame, fate);
        scene.paint_all(&mut frame.screen());
        let mut t = 0;
        while t < spin::FADE_AT - 1 {
            scene.paint(&mut frame.screen(), t);
            t += 50;
        }
        scene.paint(&mut frame.screen(), spin::FADE_AT - 1);
        let t = theme::current();
        let starts = wheel::fate_starts(fate);
        let tone = FORMAT.encode(if starts { t.success } else { t.danger }).0;
        let dial = layout::layout(800, 600).boot.dial;
        let mut toned = [0usize; SEGMENTS];
        let mut lit = [0usize; SEGMENTS];
        dial.for_each_pixel(dial.bounds(), |x, y, k, cover| {
            if cover == 255 {
                lit[k] += 1;
                if frame.at(x, y) == tone {
                    toned[k] += 1;
                }
            }
        });
        let pocket = wheel::landing(fate);
        for k in 0..SEGMENTS {
            if k == pocket {
                assert_eq!(toned[k], lit[k], "fate {fate:#x}: pocket {k} not all toned");
            } else {
                assert_eq!(toned[k], 0, "fate {fate:#x}: pocket {k} toned too");
            }
        }
    }
}
