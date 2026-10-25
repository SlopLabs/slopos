use std::vec::Vec;

use super::contrast::{NON_TEXT, TEXT, over, ratio};
use super::*;

const PLAIN: [ColorRole; 4] = [
    ColorRole::Surface,
    ColorRole::SurfaceRaised,
    ColorRole::SurfaceSunken,
    ColorRole::Chrome,
];

/// Every surface body and secondary text may sit on: each plain one,
/// hovered, and with a selected row's wash on it.
fn reading_surfaces(t: &Theme) -> Vec<(&'static str, Color32)> {
    let mut out = Vec::new();
    for role in PLAIN {
        out.push(("plain", t.color(role)));
        out.push(("hovered", over(t.overlay(State::Hover), t.color(role))));
        out.push(("selected", over(t.selection, t.color(role))));
    }
    out
}

fn assert_floor(fg: Color32, bg: Color32, floor: f64, what: &str) {
    let r = ratio(fg, bg);
    assert!(
        r >= floor,
        "{what}: {fg:?} on {bg:?} is {r:.2}:1, under {floor}:1"
    );
}

#[test]
fn text_reads_on_every_surface_it_sits_on() {
    let t = &DARK;
    for tone in [Tone::Normal, Tone::Secondary] {
        for (how, bg) in reading_surfaces(t) {
            assert_floor(t.ink(tone), bg, TEXT, &std::format!("{tone:?} text, {how}"));
        }
    }
}

/// Body text also reads where a pressed control holds it, as a menu's open
/// title does, and secondary text on the stripes of free space.
#[test]
fn body_text_reads_pressed_and_secondary_text_on_an_edge() {
    let t = &DARK;
    for role in PLAIN {
        let pressed = t.state(role, State::Pressed);
        assert_floor(
            t.text,
            pressed,
            TEXT,
            &std::format!("text on {role:?} pressed"),
        );
    }
    assert_floor(
        t.text_secondary,
        t.border,
        TEXT,
        "secondary text on a border",
    );
}

#[test]
fn status_tones_read_on_plain_surfaces() {
    let t = &DARK;
    for tone in [
        Tone::Accent,
        Tone::Danger,
        Tone::Warning,
        Tone::Success,
        Tone::Info,
    ] {
        for role in PLAIN {
            assert_floor(
                t.ink(tone),
                t.color(role),
                TEXT,
                &std::format!("{tone:?} on {role:?}"),
            );
        }
    }
}

#[test]
fn text_on_accent_reads_on_every_fill_in_every_state() {
    let t = &DARK;
    for role in [
        ColorRole::Accent,
        ColorRole::Danger,
        ColorRole::Warning,
        ColorRole::Success,
        ColorRole::Info,
    ] {
        for state in [State::Normal, State::Hover, State::Pressed] {
            let what = std::format!("on {role:?} {state:?}");
            assert_floor(t.text_on_accent, t.state(role, state), TEXT, &what);
        }
    }
    for tone in [
        Tone::Normal,
        Tone::Secondary,
        Tone::Disabled,
        Tone::Accent,
        Tone::Danger,
        Tone::Warning,
        Tone::Success,
        Tone::Info,
    ] {
        let what = std::format!("on a {tone:?} progress fill");
        assert_floor(t.text_on_accent, t.ink(tone), TEXT, &what);
    }
    for (n, fill) in t.series.iter().enumerate() {
        assert_floor(
            t.text_on_accent,
            *fill,
            TEXT,
            &std::format!("on series {n}"),
        );
    }
}

#[test]
fn edges_focus_and_disabled_text_reach_three_to_one() {
    let t = &DARK;
    for role in [
        ColorRole::BorderStrong,
        ColorRole::Focus,
        ColorRole::TextDisabled,
    ] {
        for bg in PLAIN {
            let what = std::format!("{role:?} on {bg:?}");
            assert_floor(t.color(role), t.color(bg), NON_TEXT, &what);
        }
    }
}

/// A disabled control's edge is told from an enabled one's, and its tick
/// shows on its disabled fill.
#[test]
fn a_disabled_control_reads_as_one() {
    let t = &DARK;
    let apart = ratio(t.border_strong, t.text_disabled);
    assert!(
        apart >= 1.2,
        "enabled and disabled edges {apart:.2}:1 apart"
    );
    assert_floor(
        t.text_on_accent,
        t.text_disabled,
        NON_TEXT,
        "a disabled tick",
    );
}

#[test]
fn the_old_disabled_text_fails_the_floor() {
    assert!(ratio(Color32::rgb(0x6b, 0x71, 0x7d), DARK.surface) < NON_TEXT);
}

#[test]
fn chrome_is_the_surface_so_frame_and_content_read_as_one() {
    assert_eq!(DARK.chrome, DARK.surface);
}

/// Each state shows: hovered at least 1.1:1 from idle, pressed further.
#[test]
fn states_move_visibly_away_from_the_base_and_keep_its_alpha() {
    let t = &DARK;
    for role in PLAIN.into_iter().chain([
        ColorRole::Accent,
        ColorRole::Danger,
        ColorRole::Warning,
        ColorRole::Success,
        ColorRole::Info,
    ]) {
        let normal = t.state(role, State::Normal);
        let hover = t.state(role, State::Hover);
        let pressed = t.state(role, State::Pressed);
        assert_eq!(normal, t.color(role));
        assert_eq!(hover.alpha(), normal.alpha());
        assert_eq!(pressed.alpha(), normal.alpha());
        let hovered = ratio(hover, normal);
        let pressed_by = ratio(pressed, normal);
        assert!(hovered >= 1.1, "{role:?} hovered {hovered:.2}:1");
        assert!(pressed_by > hovered, "{role:?} pressed {pressed_by:.2}:1");
    }
    let selection = t.state(ColorRole::Selection, State::Hover);
    assert_eq!(selection.alpha(), t.selection.alpha());
}

#[test]
fn an_overlay_over_a_surface_is_that_surface_in_its_state() {
    let t = &DARK;
    for role in PLAIN {
        for state in [State::Normal, State::Hover, State::Pressed] {
            assert_eq!(over(t.overlay(state), t.color(role)), t.state(role, state));
        }
    }
}

#[test]
fn series_hues_are_distinct() {
    let s = DARK.series;
    for i in 0..SERIES {
        for j in i + 1..SERIES {
            assert_ne!(s[i], s[j]);
        }
        assert_eq!(DARK.series(i + SERIES), s[i]);
    }
}

#[test]
fn tones_and_surfaces_name_their_roles() {
    assert_eq!(DARK.ink(Tone::Normal), DARK.text);
    assert_eq!(DARK.ink(Tone::Danger), DARK.danger);
    assert_eq!(DARK.fill(Surface::Sunken), DARK.surface_sunken);
    assert_eq!(DARK.fill(Surface::Chrome), DARK.chrome);
}

#[test]
fn text_roles_descend_and_fit_their_line() {
    let sizes: Vec<u16> = TextRole::ALL[..6].iter().map(|r| r.style().size).collect();
    assert!(sizes.windows(2).all(|w| w[0] > w[1]), "{sizes:?}");
    for role in TextRole::ALL {
        let s = role.style();
        assert!(s.line_height > s.size as i32, "{role:?}");
    }
}

#[test]
fn spacing_is_a_four_pixel_scale() {
    let scale = [
        space::XS,
        space::SM,
        space::MD,
        space::LG,
        space::XL,
        space::XXL,
    ];
    assert_eq!(scale, [4, 8, 12, 16, 24, 32]);
    for insets in [inset::PAGE, inset::SECTION, inset::CARD, inset::CONTROL] {
        for side in [insets.top, insets.right, insets.bottom, insets.left] {
            assert!(scale.contains(&side), "{insets:?}");
        }
    }
}

#[test]
fn a_control_inset_makes_a_body_line_a_full_target() {
    let height = TextRole::Body.style().line_height + inset::CONTROL.vertical();
    assert!(height >= 28, "{height}");
}
