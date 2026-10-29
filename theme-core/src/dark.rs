use slopos_abi::draw::Color32;

use super::{Shadow, Theme, Wallpaper};

const SURFACE: Color32 = Color32::rgb(0x23, 0x25, 0x2b);
const TEXT: Color32 = Color32::rgb(0xe9, 0xeb, 0xef);
const BLUE: Color32 = Color32::rgb(0x5c, 0x9d, 0xff);

const BLUE_FILL: Color32 = Color32::rgb(0x2a, 0x64, 0xd6);
const GREEN_FILL: Color32 = Color32::rgb(0x1b, 0x7a, 0x43);
const AMBER_FILL: Color32 = Color32::rgb(0x9a, 0x5a, 0x08);
const RED_FILL: Color32 = Color32::rgb(0xc4, 0x35, 0x3a);
const VIOLET_FILL: Color32 = Color32::rgb(0x7f, 0x47, 0xcf);
const TEAL_FILL: Color32 = Color32::rgb(0x0b, 0x72, 0x87);
const ORANGE_FILL: Color32 = Color32::rgb(0xb5, 0x48, 0x16);
const PINK_FILL: Color32 = Color32::rgb(0xc2, 0x31, 0x6f);

pub(crate) static DARK: Theme = Theme {
    surface: SURFACE,
    surface_raised: Color32::rgb(0x2c, 0x2f, 0x37),
    surface_sunken: Color32::rgb(0x1b, 0x1d, 0x22),
    chrome: SURFACE,
    scrim: Color32::new(0x08, 0x09, 0x0c, 0x99),
    border: Color32::rgb(0x34, 0x37, 0x3f),
    edge: Color32::rgb(0x45, 0x49, 0x54),
    border_strong: Color32::rgb(0x80, 0x86, 0x9a),
    text: TEXT,
    text_secondary: Color32::rgb(0xad, 0xb2, 0xbc),
    text_disabled: Color32::rgb(0x73, 0x79, 0x87),
    text_on_accent: Color32::rgb(0xff, 0xff, 0xff),
    accent: BLUE,
    danger: Color32::rgb(0xff, 0x6b, 0x6b),
    warning: Color32::rgb(0xf1, 0xb5, 0x45),
    success: Color32::rgb(0x4c, 0xc3, 0x8a),
    info: Color32::rgb(0x4c, 0xc2, 0xd9),
    focus: BLUE,
    selection: Color32::new(0x5c, 0x9d, 0xff, 0x33),
    accent_fill: BLUE_FILL,
    danger_fill: RED_FILL,
    warning_fill: AMBER_FILL,
    success_fill: GREEN_FILL,
    info_fill: TEAL_FILL,
    neutral_fill: Color32::rgb(0x54, 0x5a, 0x66),
    control: Color32::rgb(0x37, 0x3a, 0x42),
    series: [
        BLUE_FILL,
        GREEN_FILL,
        PINK_FILL,
        RED_FILL,
        VIOLET_FILL,
        TEAL_FILL,
        ORANGE_FILL,
    ],
    state_ink: TEXT,
    shade: Color32::rgb(0x00, 0x00, 0x00),
    rim_light: Color32::new(0xff, 0xff, 0xff, 0x14),
    rim_dark: Color32::new(0x00, 0x00, 0x00, 0x99),
    glass: Color32::new(0x16, 0x18, 0x1c, 0xd9),
    wallpaper: Wallpaper {
        top: Color32::rgb(0x1c, 0x23, 0x33),
        bottom: Color32::rgb(0x0c, 0x0e, 0x12),
        light: Color32::new(0x5c, 0x86, 0xd6, 0x48),
    },
    app_tiles: [
        Color32::rgb(0x2f, 0x6e, 0xe6),
        Color32::rgb(0x1a, 0x91, 0x50),
        Color32::rgb(0xd9, 0x59, 0x0b),
        Color32::rgb(0xdc, 0x3c, 0x44),
        Color32::rgb(0x7a, 0x4d, 0xe6),
        Color32::rgb(0x0d, 0x8b, 0x9f),
        Color32::rgb(0xcf, 0x3a, 0x8c),
        Color32::rgb(0x3c, 0x42, 0x50),
    ],
    shadow_raised: Shadow {
        color: Color32::new(0, 0, 0, 0x66),
        spread: 14,
        offset_y: 4,
    },
    shadow_window: Shadow {
        color: Color32::new(0, 0, 0, 0x8c),
        spread: 30,
        offset_y: 10,
    },
};
