use std::vec;
use std::vec::Vec;

use slopos_abi::draw::Color32;
use slopos_abi::pixel::PixelFormat;
use slopos_theme_core::contrast;

use crate::layout::{self, Layout};
use crate::paint::{Mask, Pixmap, Place, blit, mix};
use crate::scene::{self, Fonts, Scene, Screen};
use crate::spin::{self, Phase, Spin};
use crate::wheel::{self, NUMBERS, POCKETS, TURN};

const REGULAR: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
const SEMIBOLD: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// Fates spread over the whole `u32` range, both parities.
fn fates() -> impl Iterator<Item = u32> {
    (0..4096u32).map(|i| i.wrapping_mul(0x9E37_79B9) ^ (i >> 3))
}

#[test]
fn numbers_alternate_and_cover_one_to_twelve() {
    let mut seen = [false; POCKETS + 1];
    for (k, &n) in NUMBERS.iter().enumerate() {
        assert!(!seen[n as usize], "{n} twice");
        seen[n as usize] = true;
        let next = NUMBERS[(k + 1) % POCKETS];
        assert_ne!(wheel::starts(n), wheel::starts(next), "{n} beside {next}");
    }
    assert!(seen[1..].iter().all(|s| *s));
}

#[test]
fn a_fate_lands_on_its_own_parity() {
    for fate in fates() {
        let landing = wheel::landing(fate);
        assert_eq!(
            wheel::starts(landing.number()),
            wheel::fate_starts(fate),
            "fate {fate:#x}"
        );
        assert!(
            (0x4CCC..0xB334).contains(&landing.depth),
            "fate {fate:#x} lands {:#x} into its pocket",
            landing.depth
        );
    }
}

#[test]
fn every_pocket_can_be_landed_on() {
    let mut hit = [false; POCKETS];
    for fate in fates() {
        hit[wheel::landing(fate).pocket] = true;
    }
    assert!(hit.iter().all(|h| *h), "{hit:?}");
}

#[test]
fn the_spin_starts_centred_on_the_first_pocket() {
    let spin = Spin::new(7);
    let (pocket, depth) = wheel::under_pointer(spin.rotation(0));
    assert_eq!(pocket, 0);
    assert!(depth.abs_diff(0x8000) < 16, "{depth:#x}");
}

#[test]
fn the_spin_stops_where_the_fate_landed() {
    for fate in fates() {
        let spin = Spin::new(fate);
        let landing = spin.landing();
        for t in [spin::LAND_MS, spin::LAND_MS + 1, spin::END_MS, u32::MAX] {
            let (pocket, depth) = wheel::under_pointer(spin.rotation(t));
            assert_eq!(pocket, landing.pocket, "fate {fate:#x} at {t} ms");
            assert!(
                depth.abs_diff(landing.depth) < 16,
                "fate {fate:#x} at {t} ms"
            );
        }
        assert_eq!(spin.starts(), wheel::fate_starts(fate));
    }
}

#[test]
fn the_wheel_draws_back_short_of_a_peg_before_the_flick() {
    let spin = Spin::new(5);
    let start = spin.rotation(0);
    let drawn = spin.rotation(spin::WIND_MS - 1);
    let back = (start + TURN - drawn) % TURN;
    assert!(back > 0 && back < TURN / POCKETS as u32 / 4, "{back}");
    for t in 0..spin::WIND_MS {
        assert_eq!(spin::flap(spin.rotation(t)), 0, "{t} ms");
        assert_eq!(spin.speed(t), 0.0);
    }
}

#[test]
fn the_wheel_never_turns_back_once_flicked_nor_skips_a_frame_s_worth() {
    for fate in fates().take(64) {
        let spin = Spin::new(fate);
        let mut last = spin.rotation(spin::WIND_MS);
        let mut travelled = 0u64;
        for t in spin::WIND_MS + 1..=spin::LAND_MS {
            let now = spin.rotation(t);
            let step = (now + TURN - last) % TURN;
            assert!(
                step < TURN / POCKETS as u32 / 2,
                "fate {fate:#x}: {step} at {t} ms"
            );
            travelled += step as u64;
            last = now;
        }
        let turns = travelled as f32 / TURN as f32;
        assert!((4.0..5.0).contains(&turns), "fate {fate:#x}: {turns} turns");
    }
}

#[test]
fn ease_out_runs_zero_to_one_and_only_up() {
    assert_eq!(spin::ease_out(0), 0);
    assert_eq!(spin::ease_out(1 << 16), 1 << 16);
    let mut last = 0;
    for u in (0..=1 << 16).step_by(97) {
        let e = spin::ease_out(u);
        assert!(e >= last);
        last = e;
    }
}

#[test]
fn the_flapper_hangs_straight_at_rest() {
    for fate in fates() {
        let spin = Spin::new(fate);
        assert_eq!(
            spin::flap(spin.rotation(spin::LAND_MS)),
            0,
            "fate {fate:#x}"
        );
    }
}

#[test]
fn a_peg_lifts_the_flapper_as_it_passes() {
    let at_divider = TURN - wheel::divider(3);
    assert_eq!(spin::flap(at_divider), 256);
    // A peg a tenth of a pocket short of the pointer has it part way up.
    let short = (at_divider + TURN - TURN / POCKETS as u32 / 10) % TURN;
    let lift = spin::flap(short);
    assert!(lift > 0 && lift < 256, "{lift}");
}

#[test]
fn speed_falls_to_rest_and_blur_with_it() {
    let spin = Spin::new(0x1234_5679);
    assert_eq!(spin.speed(spin::LAND_MS), 0.0);
    assert!(spin.speed(spin::WIND_MS) > spin.speed(spin::LAND_MS / 2));
    assert!(spin::blur(spin.speed(spin::WIND_MS)) > 0);
    assert_eq!(spin::blur(spin.speed(spin::LAND_MS - 400)), 0);
    assert!(spin::blur(f32::MAX) < 256);
}

#[test]
fn phases_follow_one_another() {
    assert_eq!(spin::phase(0), Phase::Spinning);
    assert_eq!(spin::phase(spin::LAND_MS - 1), Phase::Spinning);
    assert_eq!(spin::phase(spin::LAND_MS), Phase::Settling);
    assert_eq!(spin::phase(spin::LAND_MS + spin::SETTLE_MS), Phase::Verdict);
    assert_eq!(spin::phase(spin::END_MS), Phase::Done);
}

const SCREENS: [(u32, u32); 11] = [
    (320, 240),
    (640, 480),
    (800, 600),
    (1024, 768),
    (1280, 800),
    (1366, 768),
    (1920, 1080),
    (2560, 1440),
    (2880, 1800),
    (3840, 2160),
    (1080, 1920),
];

fn inside(l: &Layout, x: i32, y: i32) -> bool {
    x >= 0 && y >= 0 && x < l.width && y < l.height
}

#[test]
fn the_wheel_fits_every_screen() {
    for (w, h) in SCREENS {
        let l = layout::layout(w, h);
        let reach = l.radius + l.rim;
        assert!(inside(&l, l.cx - reach, l.cy - reach), "{w}x{h}: {l:?}");
        assert!(inside(&l, l.cx + reach, l.cy + reach), "{w}x{h}: {l:?}");
        assert!(l.pivot_y - l.flapper / 2 >= 0, "{w}x{h}: {l:?}");
        assert!(l.hub > 0 && l.hub < l.radius, "{w}x{h}");
        let b = l.wheel_box;
        assert!(
            b.x0 >= 0 && b.y0 >= 0 && b.x1 < l.width && b.y1 < l.height,
            "{w}x{h}"
        );
        assert!(
            b.y0 <= l.pivot_y - l.flapper / 2,
            "{w}x{h}: the flapper leaves its box"
        );
    }
}

#[test]
fn the_column_stacks_without_overlapping() {
    for (w, h) in SCREENS.into_iter().skip(1) {
        let l = layout::layout(w, h);
        let s = l.sizes;
        let mut bottom = 0;
        if let Some(y) = l.title_y {
            assert!(y >= bottom, "{w}x{h}");
            bottom = y + s.title as i32;
        }
        if let Some(y) = l.rule_y {
            assert!(y >= bottom, "{w}x{h}");
            bottom = y + s.rule as i32;
        }
        assert!(
            l.wheel_box.y0 >= bottom,
            "{w}x{h}: the title runs into the wheel's frames"
        );
        assert!(
            l.wheel_box.y1 < l.status_box.y0,
            "{w}x{h}: the wheel's frames repaint the verdict"
        );
        assert!(l.status_y >= l.cy + l.radius + l.rim, "{w}x{h}");
        bottom = l.status_y + s.status as i32;
        if let Some(y) = l.detail_y {
            assert!(y >= bottom, "{w}x{h}");
            bottom = y + s.detail as i32;
        }
        assert!(l.bar.y >= bottom, "{w}x{h}");
        assert!(l.bar.y + l.bar.h <= l.height, "{w}x{h}");
        if let Some(y) = l.hint_y {
            assert!(
                y >= l.bar.y + l.bar.h,
                "{w}x{h}: the hint runs into the bar"
            );
            assert!(y + s.hint as i32 <= l.height, "{w}x{h}");
        }
    }
}

#[test]
fn a_large_screen_gets_the_largest_wheel() {
    let l = layout::layout(1920, 1080);
    assert!(l.radius >= 260, "{l:?}");
    assert!(l.title_y.is_some() && l.rule_y.is_some() && l.hint_y.is_some());
}

#[test]
fn decimal_and_verdict_read_as_words() {
    let mut d = [0u8; 3];
    assert_eq!(scene::decimal(0, &mut d), "0");
    assert_eq!(scene::decimal(12, &mut d), "12");
    let mut b = [0u8; 64];
    assert_eq!(scene::verdict(7, &mut b), "7 \u{2014} odd. You win.");
    assert_eq!(
        scene::verdict(8, &mut b),
        "8 \u{2014} even. The house wins."
    );
}

#[test]
fn mix_reaches_both_ends_byte_by_byte() {
    let (a, b) = (0x10_20_30_40, 0xF0_E0_D0_C0);
    assert_eq!(mix(a, b, 0), a);
    assert_eq!(mix(a, b, 256), b);
    let half = mix(a, b, 128);
    assert_eq!(half, 0x80_80_80_80);
}

#[test]
fn a_turned_mask_lands_where_it_was_placed() {
    let mut dst = Pixmap::new(40, 40, PixelFormat::Xrgb8888);
    let dot = Mask::from_sdf(5, 5, |x, y| {
        libm::sqrtf((x - 2.5) * (x - 2.5) + (y - 2.5) * (y - 2.5)) - 2.0
    });
    // Anchor at the dot's own corner, placed half a turn round a point.
    let place = Place {
        anchor: (0.0, 0.0),
        at: (20.0, 20.0),
        angle: core::f32::consts::PI,
    };
    blit(&mut dst, &dot, &place, 0xFFFF_FFFF, 256);
    let lit = |x: i32, y: i32| dst.px[(y * 40 + x) as usize] != 0;
    assert!(lit(17, 17), "the dot belongs up and left of the anchor");
    assert!(!lit(22, 22));
}

fn render(width: u32, height: u32, fate: u32, at: &[u32]) -> (Layout, Vec<u8>, Vec<scene::Damage>) {
    let l = layout::layout(width, height);
    let pitch = width as usize * 4;
    let mut px = vec![0u8; pitch * height as usize];
    let fonts = Fonts::new(Some(REGULAR), Some(SEMIBOLD));
    let mut scene = Scene::new(l, PixelFormat::Xrgb8888, fate, fonts);
    let mut damage = Vec::new();
    let mut screen = Screen {
        px: &mut px,
        width: width as i32,
        height: height as i32,
        pitch,
        format: PixelFormat::Xrgb8888,
    };
    scene.paint_all(&mut screen);
    for &t in at {
        damage.push(scene.paint(&mut screen, t));
    }
    (l, px, damage)
}

fn pixel(l: &Layout, px: &[u8], x: i32, y: i32) -> Color32 {
    let at = (y * l.width + x) as usize * 4;
    PixelFormat::Xrgb8888.decode(u32::from_le_bytes([
        px[at],
        px[at + 1],
        px[at + 2],
        px[at + 3],
    ]))
}

/// The pocket band just below the flapper's tip, where it points.
fn under_tip(l: &Layout, px: &[u8]) -> Color32 {
    pixel(l, px, l.cx, l.tip_y + (l.radius - l.hub) / 3)
}

fn greener(c: Color32) -> bool {
    c.green() > c.red()
}

#[test]
fn the_pointer_shows_the_pocket_the_kernel_decided() {
    for fate in [
        0x0000_0001,
        0x0000_0002,
        0xDEAD_BEEF,
        0xCAFE_F00C,
        0x1357_9BDF,
        0x2468_ACE0,
    ] {
        let (l, px, _) = render(1280, 800, fate, &[spin::END_MS]);
        assert_eq!(
            greener(under_tip(&l, &px)),
            wheel::fate_starts(fate),
            "fate {fate:#x}: {:?}",
            under_tip(&l, &px)
        );
    }
}

#[test]
fn the_first_frame_points_at_the_first_pocket() {
    let (l, px, _) = render(1280, 800, 2, &[]);
    assert!(greener(under_tip(&l, &px)), "pocket 0 holds 1, a win");
}

#[test]
fn a_frame_damages_only_what_changed() {
    let t = [
        500,
        500,
        2000,
        spin::LAND_MS + 10,
        spin::END_MS + 5,
        spin::END_MS + 500,
    ];
    let (l, _, damage) = render(1024, 768, 3, &t);
    assert!(damage[0].wheel.is_some(), "the wheel turns");
    assert!(damage[1].is_empty(), "the same moment twice paints nothing");
    assert_eq!(damage[2].wheel, Some(l.wheel_box));
    assert_eq!(
        damage[2].column, None,
        "the column says the same while it spins"
    );
    assert!(damage[3].column.is_some(), "landing changes the words");
    assert!(damage[5].is_empty(), "nothing moves once it is over");
}

#[test]
fn the_wheel_is_painted_at_one_size_on_every_screen() {
    for (w, h) in [(320, 240), (3840, 2160)] {
        let (l, px, _) = render(w, h, 9, &[spin::LAND_MS / 2, spin::END_MS]);
        assert!(px.iter().any(|b| *b != 0), "{w}x{h}");
        let _ = under_tip(&l, &px);
    }
}

#[test]
fn numbers_read_against_their_pockets() {
    let t = slopos_theme_core::current();
    for pocket in [scene::pocket_color(t, true), scene::pocket_color(t, false)] {
        let r = contrast::ratio(t.text_on_accent, pocket);
        assert!(r >= 3.0, "{pocket:?}: {r}");
    }
}

#[test]
fn turn_of_agrees_with_atan2() {
    for i in 0..2000 {
        let a = i as f32 * 0.0031;
        let (x, y) = (
            libm::cosf(a) * (1.0 + i as f32),
            libm::sinf(a) * (1.0 + i as f32),
        );
        let want = (libm::atan2f(x, -y) / core::f32::consts::TAU).rem_euclid(1.0) * 65536.0;
        let got = scene::turn_of(x, y) as f32;
        let err = (got - want).abs().min(65536.0 - (got - want).abs());
        assert!(err < 3.0, "({x}, {y}): {got} vs {want}");
    }
}

#[test]
fn revealing_the_number_repaints_only_the_hub() {
    let (l, px, _) = render(1280, 800, 0x51, &[spin::LAND_MS + 1000]);
    // Inside the hub's box but past its edge: a pocket, not the backdrop.
    let off = l.hub * 9 / 10;
    for (dx, dy) in [(-off, -off), (off, -off), (-off, off), (off, off)] {
        let c = pixel(&l, &px, l.cx + dx, l.cy + dy);
        let brightest = c.red().max(c.green()).max(c.blue());
        assert!(brightest > 0x40, "({dx}, {dy}): {c:?}");
    }
}

#[test]
fn the_wheel_reads_the_same_all_the_way_round() {
    // Twelve pockets: a quarter turn is three, of the other parity, and half
    // a turn is six, of the same; every quadrant must agree.
    for fate in [0x0000_0003, 0x0000_0004, 0x7777_7777, 0x8888_8888] {
        let (l, px, _) = render(1280, 800, fate, &[spin::END_MS]);
        let mid = (l.radius + l.hub) / 2;
        let starts = wheel::fate_starts(fate);
        assert_eq!(
            greener(pixel(&l, &px, l.cx, l.cy + mid)),
            starts,
            "{fate:#x} below"
        );
        assert_eq!(
            greener(pixel(&l, &px, l.cx + mid, l.cy)),
            !starts,
            "{fate:#x} right"
        );
        assert_eq!(
            greener(pixel(&l, &px, l.cx - mid, l.cy)),
            !starts,
            "{fate:#x} left"
        );
    }
}
