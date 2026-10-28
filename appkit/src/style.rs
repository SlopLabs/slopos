use slopos_theme_core::{TextRole, Theme, inset, space};

/// The least a control is across, so a pointer finds it.
pub const MIN_TARGET: i32 = 28;

/// The theme widgets draw with, and the toolkit's own control dimensions.
pub struct StyleSheet {
    pub theme: &'static Theme,

    /// A button's, a field's and a partition bar's height.
    pub control_height: i32,
    pub button_min_width: i32,

    pub field_padding_h: i32,
    pub field_min_width: i32,

    pub scrollbar_width: i32,
    pub scrollbar_thumb_min: i32,

    pub tab_height: i32,
    pub menu_item_height: i32,
    pub menu_bar_height: i32,
    pub menu_min_width: i32,
    /// One row of a table or a tree, and a table's headings.
    pub row_height: i32,
    /// Across a divider a pane is resized by.
    pub divider_width: i32,

    pub checkbox_size: i32,
    pub checkbox_gap: i32,
}

impl StyleSheet {
    pub fn current() -> Self {
        Self::with_theme(slopos_theme_core::current())
    }

    const fn with_theme(theme: &'static Theme) -> Self {
        Self {
            theme,

            control_height: TextRole::Body.style().line_height + inset::CONTROL.vertical(),
            button_min_width: 64,

            field_padding_h: space::SM,
            field_min_width: 80,

            scrollbar_width: 8,
            scrollbar_thumb_min: 20,

            tab_height: 34,
            menu_item_height: MIN_TARGET,
            menu_bar_height: MIN_TARGET + space::XS,
            menu_min_width: 160,
            row_height: MIN_TARGET,
            divider_width: 6,

            checkbox_size: 16,
            checkbox_gap: space::SM,
        }
    }
}
