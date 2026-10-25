use super::{Color32, Shadow, Theme};

const SURFACE: Color32 = Color32::rgb(0x28, 0x2c, 0x33);
const TEXT: Color32 = Color32::rgb(0xdc, 0xe0, 0xe5);
const BLUE: Color32 = Color32::rgb(0x74, 0xad, 0xe8);
const GREEN: Color32 = Color32::rgb(0x98, 0xc3, 0x79);
const YELLOW: Color32 = Color32::rgb(0xe5, 0xc0, 0x7b);
const RED: Color32 = Color32::rgb(0xe8, 0x83, 0x8c);
const PURPLE: Color32 = Color32::rgb(0xc6, 0x78, 0xdd);
const CYAN: Color32 = Color32::rgb(0x56, 0xb6, 0xc2);
const ORANGE: Color32 = Color32::rgb(0xd1, 0x9a, 0x66);

/// One Dark's hues, its red lifted to read on a raised surface.
pub static DARK: Theme = Theme {
    surface: SURFACE,
    surface_raised: Color32::rgb(0x32, 0x37, 0x3f),
    surface_sunken: Color32::rgb(0x21, 0x25, 0x2b),
    chrome: SURFACE,
    scrim: Color32::new(0, 0, 0, 0xa0),
    border: Color32::rgb(0x3e, 0x44, 0x51),
    border_strong: Color32::rgb(0x95, 0x9b, 0xa7),
    text: TEXT,
    text_secondary: Color32::rgb(0xb4, 0xba, 0xc6),
    text_disabled: Color32::rgb(0x83, 0x8a, 0x96),
    text_on_accent: Color32::rgb(0x1a, 0x1d, 0x23),
    accent: BLUE,
    danger: RED,
    warning: YELLOW,
    success: GREEN,
    info: CYAN,
    focus: BLUE,
    selection: Color32::new(0x74, 0xad, 0xe8, 0x26),
    series: [BLUE, GREEN, YELLOW, RED, PURPLE, CYAN, ORANGE],
    state_ink: TEXT,
    shadow_raised: Shadow {
        color: Color32::new(0, 0, 0, 40),
        spread: 4,
        offset_y: 1,
    },
    shadow_window: Shadow {
        color: Color32::new(0, 0, 0, 50),
        spread: 12,
        offset_y: 4,
    },
};
