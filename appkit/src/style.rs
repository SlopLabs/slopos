use slopos_theme_core::{DARK, Theme, space, stroke};

/// The theme widgets draw with, and the toolkit's own control dimensions.
pub struct StyleSheet {
    pub theme: &'static Theme,

    pub border_width: i32,
    pub focus_ring_width: i32,
    pub focus_ring_offset: i32,

    pub button_min_width: i32,

    pub field_padding_h: i32,
    pub field_min_width: i32,

    pub scrollbar_width: i32,
    pub scrollbar_thumb_min: i32,

    pub tab_height: i32,
    pub menu_item_height: i32,
    pub menu_min_width: i32,
    /// Height of one row in a tree or a list of files.
    pub row_height: i32,

    pub checkbox_size: i32,
    pub checkbox_gap: i32,
}

impl StyleSheet {
    pub fn dark() -> Self {
        Self::with_theme(&DARK)
    }

    pub const fn with_theme(theme: &'static Theme) -> Self {
        Self {
            theme,

            border_width: stroke::EDGE,
            focus_ring_width: stroke::FOCUS,
            focus_ring_offset: 1,

            button_min_width: 64,

            field_padding_h: space::SM,
            field_min_width: 80,

            scrollbar_width: 8,
            scrollbar_thumb_min: 20,

            tab_height: 34,
            menu_item_height: 26,
            menu_min_width: 160,
            row_height: 22,

            checkbox_size: 16,
            checkbox_gap: space::SM,
        }
    }
}

impl Default for StyleSheet {
    fn default() -> Self {
        Self::dark()
    }
}
