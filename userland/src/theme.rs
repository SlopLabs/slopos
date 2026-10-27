//! The compositor's chrome: its own geometry, and the theme's tokens under
//! the names its drawing code reads.

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, DARK, State, TextRole, radius, with_alpha};

// Owned by `slopos_chrome_core::status`, which computes the bar's damage rects
// and hit regions; a second declaration would drift an item's pixels away from
// its clickable area.
pub const SYSTEM_BAR_HEIGHT: i32 = slopos_chrome_core::BAR_HEIGHT;
pub const SYSTEM_BAR_PADDING_X: i32 = slopos_chrome_core::BAR_PADDING_X;
pub const SYSTEM_BAR_ICON_SIZE: i32 = 8;
pub const SYSTEM_BAR_ICON_GAP: i32 = 8;
pub const SYSTEM_BAR_MAX_APP_NAME_WIDTH: i32 = 200;

pub const BAR_TEXT_PX: u16 = TextRole::Label.style().size;
pub const TITLE_TEXT_PX: u16 = TextRole::Body.style().size;

pub const TITLE_BAR_HEIGHT: i32 = 28;
pub const WINDOW_CORNER_RADIUS: i32 = radius::WINDOW;
pub const TITLE_MAX_TEXT_WIDTH_MARGIN: i32 = 140;
pub const MIN_WINDOW_WIDTH: i32 = 200;
pub const MIN_WINDOW_HEIGHT: i32 = 150;
/// How far along each edge from a corner a resize takes both edges.
pub const RESIZE_CORNER: i32 = 24;

pub const SIGNAL_BUTTON_RADIUS: i32 = 6;
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
pub const SHELF_ICON_SPACING: i32 = 8;
pub const SHELF_SEPARATOR_WIDTH: i32 = 2;
pub const SHELF_SEPARATOR_HEIGHT: i32 = 32;
pub const SHELF_SEPARATOR_MARGIN_X: i32 = 8;
pub const SHELF_DOT_DIAMETER: i32 = 4;
pub const SHELF_DOT_MARGIN_Y: i32 = 3;
pub const SHELF_LABEL_PADDING_X: i32 = 6;
pub const SHELF_LABEL_PADDING_Y: i32 = 3;
pub const SHELF_LABEL_RADIUS: i32 = 4;
pub const SHELF_LABEL_GAP_Y: i32 = 6;
/// What the dock takes from the bottom of the screen, its dots included.
pub const DOCK_HEIGHT: i32 = SHELF_ICON_SIZE
    + 2 * SHELF_PILL_PADDING_Y
    + SHELF_BOTTOM_MARGIN
    + SHELF_DOT_DIAMETER
    + SHELF_DOT_MARGIN_Y;

pub const MAGNIFICATION_PROXIMITY_X: i32 = 120;
pub const MAGNIFICATION_PROXIMITY_Y: i32 = 80;
/// 0.33 in 8.8 fixed-point (0.33 * 256 = ~84).
pub const MAGNIFICATION_AMOUNT_256: i32 = 84;

/// Where windows open and what a maximized one fills: the screen between the
/// menu bar, its border row included, and the dock.
pub fn work_area(screen_w: u32, screen_h: u32) -> slopos_chrome_core::Rect {
    let top = SYSTEM_BAR_HEIGHT + 1;
    slopos_chrome_core::Rect::new(0, top, screen_w as i32, screen_h as i32 - top - DOCK_HEIGHT)
}

pub const SHADOW_SPREAD: i32 = DARK.shadow_window.spread;
pub const SHADOW_OFFSET_Y: i32 = DARK.shadow_window.offset_y;
pub const SHADOW_MAX_ALPHA: u8 = DARK.shadow_window.color.alpha();
/// How far past a frame a press takes its edge for a resize: as far as its
/// shadow reaches.
pub const RESIZE_REACH: i32 = SHADOW_SPREAD;

/// Over the wallpaper, so a little of it shows through.
pub const BAR_BG: Color32 = with_alpha(DARK.chrome, 0xcc);
pub const DOCK_BG: Color32 = DARK.chrome;
/// What text on the bar is anti-aliased towards, rather than whatever the
/// wallpaper put behind it.
pub const BAR_INK_BG: Color32 = DARK.chrome;
pub const BAR_HOVER_BG: Color32 = DARK.state(ColorRole::Chrome, State::Hover);
pub const TOOLTIP_BG: Color32 = DARK.surface_raised;

pub const DIVIDER: Color32 = DARK.border;
pub const TITLE_BAR: Color32 = DARK.chrome;
/// A window's content before its first buffer arrives, and where its frame
/// has grown past its buffer.
pub const SURFACE: Color32 = DARK.surface;
/// Laid over the wallpaper behind the lock and login screens.
pub const SCRIM: Color32 = DARK.scrim;
pub const DESKTOP_BG: Color32 = DARK.surface_sunken;

pub const SIGNAL_CLOSE: Color32 = DARK.danger;
pub const SIGNAL_MINIMIZE: Color32 = DARK.warning;
pub const SIGNAL_EXPAND: Color32 = DARK.success;
pub const SIGNAL_INACTIVE: Color32 = DARK.border_strong;
pub const SIGNAL_GLYPH: Color32 = DARK.text_on_accent;

pub const TEXT_PRIMARY: Color32 = DARK.text;
pub const TEXT_SECONDARY: Color32 = DARK.text_secondary;
pub const ACCENT: Color32 = DARK.accent;

/// The pointer is a sprite, white edged in black on any theme.
pub const CURSOR_FILL: Color32 = Color32::WHITE;
pub const CURSOR_EDGE: Color32 = Color32::BLACK;
