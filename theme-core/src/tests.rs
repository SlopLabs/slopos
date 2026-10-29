use std::vec::Vec;

use super::contrast::{NON_TEXT, TEXT, over, ratio};
use super::dark::DARK;
use super::*;

const PLAIN: [ColorRole; 4] = [
    ColorRole::Surface,
    ColorRole::SurfaceRaised,
    ColorRole::SurfaceSunken,
    ColorRole::Chrome,
];

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

/// A menu's open title sits on a pressed fill, free space on its edge stripes.
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

const FILLS: [ColorRole; 6] = [
    ColorRole::AccentFill,
    ColorRole::DangerFill,
    ColorRole::WarningFill,
    ColorRole::SuccessFill,
    ColorRole::InfoFill,
    ColorRole::NeutralFill,
];

#[test]
fn text_on_accent_reads_on_every_fill_in_every_state() {
    let t = &DARK;
    for role in FILLS {
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
        assert_floor(t.text_on_accent, t.filled(tone), TEXT, &what);
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
fn a_tone_s_fill_is_darker_than_its_ink() {
    let t = &DARK;
    for tone in [
        Tone::Accent,
        Tone::Danger,
        Tone::Warning,
        Tone::Success,
        Tone::Info,
    ] {
        let (ink, fill) = (t.ink(tone), t.filled(tone));
        assert!(
            super::contrast::luminance(fill) < super::contrast::luminance(ink),
            "{tone:?}: {fill:?} is not darker than {ink:?}"
        );
    }
}

#[test]
fn text_reads_on_a_secondary_control_in_every_state() {
    let t = &DARK;
    for state in [State::Normal, State::Hover, State::Pressed] {
        let fill = t.state(ColorRole::Control, state);
        assert_floor(t.text, fill, TEXT, &std::format!("on a control {state:?}"));
    }
}

/// A glyph is not text: 1.4.11's 3:1 floor.
#[test]
fn an_app_glyph_reads_on_every_tile_and_its_lit_corner() {
    let t = &DARK;
    for tile in AppTile::ALL {
        let (base, lit) = t.app_tile(tile);
        assert!(
            super::contrast::luminance(lit) > super::contrast::luminance(base),
            "{tile:?}"
        );
        for (end, fill) in [("base", base), ("lit", lit)] {
            let what = std::format!("a glyph on {tile:?}'s {end}");
            assert_floor(t.text_on_accent, fill, NON_TEXT, &what);
        }
    }
}

#[test]
fn an_app_without_an_icon_keeps_its_tile_and_graphite_is_never_given() {
    assert_eq!(
        AppTile::of("org.example.app"),
        AppTile::of("org.example.app")
    );
    let given: Vec<AppTile> = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]
        .iter()
        .map(|id| AppTile::of(id))
        .collect();
    assert!(!given.contains(&AppTile::Graphite));
    let distinct = AppTile::ALL.iter().filter(|t| given.contains(t)).count();
    assert!(distinct >= 4, "{given:?}");
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

#[test]
fn a_focused_edge_changes_more_than_its_hue() {
    assert!(stroke::FOCUS > stroke::EDGE);
    assert_floor(DARK.focus, DARK.surface_sunken, NON_TEXT, "a focused edge");
}

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
fn chrome_is_the_surface_so_frame_and_content_read_as_one() {
    assert_eq!(DARK.chrome, DARK.surface);
}

#[test]
fn states_move_visibly_away_from_the_base_and_keep_its_alpha() {
    let t = &DARK;
    for role in PLAIN.into_iter().chain(FILLS).chain([ColorRole::Control]) {
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
fn a_pressed_fill_is_darker_and_a_hovered_one_lighter() {
    let t = &DARK;
    for role in FILLS {
        let l = |state| super::contrast::luminance(t.state(role, state));
        assert!(l(State::Hover) > l(State::Normal), "{role:?} hovered");
        assert!(l(State::Pressed) < l(State::Normal), "{role:?} pressed");
    }
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

/// WCAG 1.4.11: a graph's line is the only mark of its data.
#[test]
fn series_inks_reach_three_to_one_on_every_plain_surface() {
    let t = &DARK;
    for n in 0..SERIES {
        for role in [
            ColorRole::Surface,
            ColorRole::SurfaceSunken,
            ColorRole::SurfaceRaised,
        ] {
            let what = std::format!("series ink {n} on {role:?}");
            assert_floor(t.series_ink(n), t.color(role), NON_TEXT, &what);
        }
    }
}

#[test]
fn tones_and_surfaces_name_their_roles() {
    assert_eq!(DARK.ink(Tone::Normal), DARK.text);
    assert_eq!(DARK.ink(Tone::Danger), DARK.danger);
    assert_eq!(DARK.filled(Tone::Danger), DARK.danger_fill);
    assert_eq!(DARK.filled(Tone::Secondary), DARK.neutral_fill);
    assert_eq!(DARK.fill(Surface::Sunken), DARK.surface_sunken);
    assert_eq!(DARK.fill(Surface::Chrome), DARK.chrome);
}

#[test]
fn text_roles_descend_and_fit_their_line() {
    let sizes: Vec<u16> = TextRole::ALL[..7].iter().map(|r| r.style().size).collect();
    assert!(sizes.windows(2).all(|w| w[0] > w[1]), "{sizes:?}");
    for role in TextRole::ALL {
        let s = role.style();
        assert!(s.line_height > s.size as i32, "{role:?}");
    }
}

#[test]
fn spacing_is_a_four_pixel_scale() {
    let scale = [space::XS, space::SM, space::MD, space::LG, space::XL];
    assert_eq!(scale, [4, 8, 12, 16, 24]);
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

#[test]
fn every_icon_is_a_glyph_of_the_shipped_font() {
    let data = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../assets/fonts/Lucide.ttf"
    ))
    .expect("the icon font ships in assets/fonts");
    let font = slopos_font::ttf_parser::TtfFont::parse(&data).expect("the icon font parses");
    for icon in Icon::ALL {
        let glyph = font
            .glyph_index(icon.codepoint() as u32)
            .unwrap_or_else(|| panic!("{} has no glyph", icon.name()));
        let outline = font
            .glyph_outline(glyph)
            .unwrap_or_else(|| panic!("{} has no outline", icon.name()));
        assert!(
            !outline.contours.is_empty(),
            "{} draws nothing",
            icon.name()
        );
    }
}

#[test]
fn icons_are_named_once_and_listed_once() {
    for (i, icon) in Icon::ALL.iter().enumerate() {
        assert_eq!(*icon as usize, i, "{} is out of place", icon.name());
    }
    assert_eq!(Icon::ALL.len(), Icon::ZoomOut as usize + 1);
    for (i, a) in Icon::ALL.iter().enumerate() {
        for b in &Icon::ALL[i + 1..] {
            assert_ne!(a, b);
            assert_ne!(a.name(), b.name());
            assert_ne!(a.codepoint(), b.codepoint());
        }
    }
}

#[test]
fn a_name_keeps_its_series_and_names_spread_over_them() {
    assert_eq!(series_of("carol"), series_of("carol"));
    let mut seen = [false; SERIES];
    for name in [
        "user", "bob", "carol", "dave", "alice", "erin", "frank", "grace",
    ] {
        seen[series_of(name)] = true;
    }
    assert!(seen.iter().filter(|&&s| s).count() >= 4, "{seen:?}");
}
