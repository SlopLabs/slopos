use slopos_abi::draw::Color32;

/// Pack an opaque `Color32` with a separate alpha byte into a single `u32`
/// in 0xAARRGGBB layout.
pub const fn argb(c: Color32, a: u8) -> u32 {
    (a as u32) << 24 | (c.red() as u32) << 16 | (c.green() as u32) << 8 | c.blue() as u32
}

// Owned by `slopos_chrome_core::status`, which computes the bar's damage rects
// and hit regions; a second declaration would drift an item's pixels away from
// its clickable area.
pub const SYSTEM_BAR_HEIGHT: i32 = slopos_chrome_core::BAR_HEIGHT;
pub const SYSTEM_BAR_PADDING_X: i32 = slopos_chrome_core::BAR_PADDING_X;
pub const SYSTEM_BAR_ICON_SIZE: i32 = 8;
pub const SYSTEM_BAR_ICON_GAP: i32 = 8;
pub const SYSTEM_BAR_MAX_APP_NAME_WIDTH: i32 = 200;
/// Gap between two adjacent status items on the right of the bar.
pub const SYSTEM_BAR_CLOCK_GAP: i32 = slopos_chrome_core::BAR_ITEM_GAP;

/// Scale of the network indicator's unit grid; 1 keeps every edge on a pixel
/// boundary.
pub const STATUS_GLYPH_SCALE: i32 = 1;

pub const TITLE_BAR_HEIGHT: i32 = 28;
pub const WINDOW_CORNER_RADIUS: i32 = 8;
pub const TITLE_MAX_TEXT_WIDTH_MARGIN: i32 = 140;
pub const MIN_WINDOW_WIDTH: i32 = 200;
pub const MIN_WINDOW_HEIGHT: i32 = 150;

pub const SIGNAL_BUTTON_DIAMETER: i32 = 12;
pub const SIGNAL_BUTTON_RADIUS: i32 = 6;
pub const SIGNAL_BUTTON_SPACING: i32 = 20;
pub const SIGNAL_BUTTON_1_CX: i32 = 18;
pub const SIGNAL_BUTTON_2_CX: i32 = 38;
pub const SIGNAL_BUTTON_3_CX: i32 = 58;
pub const SIGNAL_BUTTON_CY: i32 = 14;
pub const SIGNAL_GROUP_X: i32 = 6;
pub const SIGNAL_GROUP_Y: i32 = 2;
pub const SIGNAL_GROUP_W: i32 = 64;
pub const SIGNAL_GROUP_H: i32 = 24;
pub const SIGNAL_GLYPH_SIZE: i32 = 6;

pub const SHELF_BOTTOM_MARGIN: i32 = 4;
pub const SHELF_PILL_RADIUS: i32 = 12;
pub const SHELF_PILL_PADDING_X: i32 = 12;
pub const SHELF_PILL_PADDING_Y: i32 = 6;
pub const SHELF_ICON_SIZE: i32 = 48;
pub const SHELF_ICON_SIZE_MAX: i32 = 64;
pub const SHELF_ICON_SPACING: i32 = 8;
pub const SHELF_ICON_CORNER_RADIUS: i32 = 10;
pub const SHELF_SEPARATOR_WIDTH: i32 = 2;
pub const SHELF_SEPARATOR_HEIGHT: i32 = 32;
pub const SHELF_SEPARATOR_MARGIN_X: i32 = 8;
pub const SHELF_DOT_DIAMETER: i32 = 4;
pub const SHELF_DOT_MARGIN_Y: i32 = 3;
pub const SHELF_LABEL_PADDING_X: i32 = 6;
pub const SHELF_LABEL_PADDING_Y: i32 = 3;
pub const SHELF_LABEL_RADIUS: i32 = 4;
pub const SHELF_LABEL_GAP_Y: i32 = 6;

pub const MAGNIFICATION_PROXIMITY_X: i32 = 120;
pub const MAGNIFICATION_PROXIMITY_Y: i32 = 80;
/// 0.33 in 8.8 fixed-point (0.33 * 256 = ~84).
pub const MAGNIFICATION_AMOUNT_256: i32 = 84;

pub const SHADOW_SPREAD: i32 = 12;
pub const SHADOW_OFFSET_Y: i32 = 4;
pub const SHADOW_MAX_ALPHA: u8 = 50;

/// Grace period (ms) before force-terminating an app on close.
pub const CLOSE_GRACE_MS: u32 = 1500;

pub const PANEL_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);
pub const PANEL_BG_ALPHA: u8 = 0xCC;

/// The panel background with its transparency removed: anti-aliased ink blends
/// towards this rather than sampling whatever the desktop put behind the bar.
pub const PANEL_BG_OPAQUE: Color32 =
    Color32::new(PANEL_BG.red(), PANEL_BG.green(), PANEL_BG.blue(), 0xFF);

pub const PANEL_BORDER: Color32 = Color32::rgb(0x2A, 0x2A, 0x2C);
pub const PANEL_BORDER_ALPHA: u8 = 0xFF;

pub const SHELF_BG: Color32 = Color32::rgb(0x1A, 0x1A, 0x1C);
pub const SHELF_BG_ALPHA: u8 = 0xB0;

pub const SHELF_SEPARATOR: Color32 = Color32::rgb(0x3A, 0x3A, 0x3C);
pub const SHELF_SEPARATOR_ALPHA: u8 = 0xFF;

pub const SHELF_DOT_ACTIVE: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const SHELF_DOT_ACTIVE_ALPHA: u8 = 0xFF;

pub const SHELF_LABEL_BG: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const SHELF_LABEL_BG_ALPHA: u8 = 0xCC;

pub const TITLE_BAR_FOCUSED: Color32 = Color32::rgb(0x2D, 0x2D, 0x30);
pub const TITLE_BAR_FOCUSED_ALPHA: u8 = 0xFF;

pub const TITLE_BAR_UNFOCUSED: Color32 = Color32::rgb(0x1E, 0x1E, 0x1E);
pub const TITLE_BAR_UNFOCUSED_ALPHA: u8 = 0xFF;

/// Close button (red).
pub const SIGNAL_CLOSE: Color32 = Color32::rgb(0xFF, 0x5F, 0x57);
pub const SIGNAL_CLOSE_ALPHA: u8 = 0xFF;

/// Minimize button (yellow).
pub const SIGNAL_MINIMIZE: Color32 = Color32::rgb(0xFF, 0xBD, 0x2E);
pub const SIGNAL_MINIMIZE_ALPHA: u8 = 0xFF;

/// Expand button (green).
pub const SIGNAL_EXPAND: Color32 = Color32::rgb(0x28, 0xC8, 0x40);
pub const SIGNAL_EXPAND_ALPHA: u8 = 0xFF;

pub const SIGNAL_INACTIVE: Color32 = Color32::rgb(0x3E, 0x3E, 0x42);
pub const SIGNAL_INACTIVE_ALPHA: u8 = 0xFF;

pub const SIGNAL_GLYPH: Color32 = Color32::rgb(0x1A, 0x0A, 0x0A);
pub const SIGNAL_GLYPH_ALPHA: u8 = 0xFF;

pub const DESKTOP_BG: Color32 = Color32::rgb(0x00, 0x11, 0x22);
pub const DESKTOP_BG_ALPHA: u8 = 0xFF;

pub const TEXT_PRIMARY: Color32 = Color32::rgb(0xE0, 0xE0, 0xE0);
pub const TEXT_PRIMARY_ALPHA: u8 = 0xFF;

pub const TEXT_SECONDARY: Color32 = Color32::rgb(0x90, 0x90, 0x90);
pub const TEXT_SECONDARY_ALPHA: u8 = 0xFF;

/// Ink for something switched off or unusable: dim enough to read as "not
/// available" beside SECONDARY, light enough to stay legible on the panel.
pub const TEXT_DISABLED: Color32 = Color32::rgb(0x5A, 0x5A, 0x5C);
pub const TEXT_DISABLED_ALPHA: u8 = 0xFF;

pub const STATUS_ITEM_HOVER_BG: Color32 = Color32::rgb(0x2E, 0x2E, 0x32);
pub const STATUS_ITEM_HOVER_BG_ALPHA: u8 = 0xFF;

/// Shadow base colour; the alpha varies per ring.
pub const SHADOW_COLOR: Color32 = Color32::rgb(0x00, 0x00, 0x00);

pub const ICON_DEFAULT: Color32 = Color32::rgb(0x6B, 0x5A, 0x8E);
pub const ICON_DEFAULT_ALPHA: u8 = 0xFF;

// TODO(tech-debt): the aliases below exist for consumers not yet ported to the
// new chrome names — port them and delete these.
pub const COLOR_BACKGROUND: Color32 = DESKTOP_BG;
pub const COLOR_CURSOR: Color32 = Color32::rgb(0xFF, 0xFF, 0xFF);
pub const COLOR_TEXT: Color32 = TEXT_PRIMARY;

pub const COLOR_TITLE_BAR: Color32 = TITLE_BAR_UNFOCUSED;
pub const COLOR_TITLE_BAR_FOCUSED: Color32 = TITLE_BAR_FOCUSED;
pub const COLOR_TITLE_BAR_TINT: Color32 = Color32::new(0x1E, 0x1E, 0x1E, 0xD0);
pub const COLOR_TITLE_BAR_FOCUSED_TINT: Color32 = Color32::new(0x2D, 0x2D, 0x30, 0xD0);
pub const COLOR_BUTTON: Color32 = SIGNAL_INACTIVE;
pub const COLOR_BUTTON_HOVER: Color32 = Color32::rgb(0x50, 0x50, 0x52);
pub const COLOR_BUTTON_CLOSE_HOVER: Color32 = SIGNAL_CLOSE;
pub const BUTTON_SIZE: i32 = 20;
pub const BUTTON_PADDING: i32 = 2;

pub const TASKBAR_HEIGHT: i32 = 32;
pub const TASKBAR_BUTTON_PADDING: i32 = 4;
pub const START_BUTTON_WIDTH: i32 = 56;
pub const START_APPS_GAP: i32 = 14;
pub const START_MENU_WIDTH: i32 = 180;
pub const START_MENU_ITEM_HEIGHT: i32 = 24;
pub const START_MENU_PADDING: i32 = 6;
pub const COLOR_TASKBAR: Color32 = PANEL_BG;
pub const COLOR_START_MENU_BG: Color32 = PANEL_BG;

pub const SYSINFO_BUTTON_WIDTH: i32 = 48;
