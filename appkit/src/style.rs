use slopos_abi::draw::Color32;

/// Central style sheet; widgets reference this instead of hardcoding colors and sizes.
///
/// The dark palette is One Dark — the values every editor that ships a "one
/// dark" uses, which is what makes an editor built on this toolkit look like an
/// editor rather than like a terminal wearing one.
pub struct StyleSheet {
    pub bg_primary: Color32,
    pub bg_secondary: Color32,
    pub bg_tertiary: Color32,
    /// Panels that float above the window: menus, popovers, dialogs.
    pub bg_elevated: Color32,
    pub bg_accent: Color32,
    pub bg_destructive: Color32,
    /// Row/chip background under the pointer.
    pub bg_hover: Color32,
    /// Row/chip background of the current selection.
    pub bg_selected: Color32,

    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_on_accent: Color32,
    pub text_disabled: Color32,
    /// Links, active tab underline, focused affordances.
    pub text_accent: Color32,

    pub border_default: Color32,
    pub border_focused: Color32,
    pub border_divider: Color32,

    pub shadow_color: Color32,
    pub focus_ring_color: Color32,

    /// Code surfaces: the editor body, its gutter and its decorations.
    pub code_bg: Color32,
    pub code_fg: Color32,
    pub gutter_fg: Color32,
    pub gutter_fg_active: Color32,
    pub line_highlight: Color32,
    pub selection_bg: Color32,
    pub cursor_color: Color32,
    pub match_highlight: Color32,

    pub font_size: i32,
    pub font_size_small: i32,
    pub font_size_heading: i32,
    pub line_height: i32,

    pub spacing_xs: i32,
    pub spacing_sm: i32,
    pub spacing_md: i32,
    pub spacing_lg: i32,
    pub spacing_xl: i32,

    pub corner_radius: i32,
    pub border_width: i32,
    pub focus_ring_width: i32,
    pub focus_ring_offset: i32,

    pub button_padding_h: i32,
    pub button_padding_v: i32,
    pub button_min_width: i32,

    pub field_padding_h: i32,
    pub field_padding_v: i32,
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
        Self {
            bg_primary: Color32::rgb(0x28, 0x2c, 0x33),
            bg_secondary: Color32::rgb(0x2f, 0x34, 0x3e),
            bg_tertiary: Color32::rgb(0x3b, 0x41, 0x4d),
            bg_elevated: Color32::rgb(0x32, 0x38, 0x43),
            bg_accent: Color32::rgb(0x74, 0xad, 0xe8),
            bg_destructive: Color32::rgb(0xd0, 0x72, 0x77),
            bg_hover: Color32::rgb(0x36, 0x3c, 0x46),
            bg_selected: Color32::rgb(0x45, 0x4a, 0x56),

            text_primary: Color32::rgb(0xdc, 0xe0, 0xe5),
            text_secondary: Color32::rgb(0xa9, 0xaf, 0xbc),
            text_on_accent: Color32::rgb(0x1a, 0x1d, 0x23),
            text_disabled: Color32::rgb(0x6b, 0x71, 0x7d),
            text_accent: Color32::rgb(0x74, 0xad, 0xe8),

            border_default: Color32::rgb(0x46, 0x4b, 0x57),
            border_focused: Color32::rgb(0x74, 0xad, 0xe8),
            border_divider: Color32::rgb(0x36, 0x3c, 0x46),

            shadow_color: Color32::new(0, 0, 0, 80),
            focus_ring_color: Color32::new(0x74, 0xad, 0xe8, 180),

            code_bg: Color32::rgb(0x28, 0x2c, 0x33),
            code_fg: Color32::rgb(0xac, 0xb2, 0xbe),
            gutter_fg: Color32::rgb(0x4e, 0x5a, 0x5f),
            gutter_fg_active: Color32::rgb(0xd0, 0xd4, 0xda),
            line_highlight: Color32::rgb(0x2f, 0x34, 0x3e),
            selection_bg: Color32::new(0x74, 0xad, 0xe8, 0x3d),
            cursor_color: Color32::rgb(0x74, 0xad, 0xe8),
            match_highlight: Color32::new(0xdf, 0xc1, 0x84, 0x4d),

            font_size: 14,
            font_size_small: 12,
            font_size_heading: 17,
            line_height: 20,

            spacing_xs: 4,
            spacing_sm: 8,
            spacing_md: 12,
            spacing_lg: 16,
            spacing_xl: 24,

            corner_radius: 6,
            border_width: 1,
            focus_ring_width: 2,
            focus_ring_offset: 1,

            button_padding_h: 12,
            button_padding_v: 6,
            button_min_width: 64,

            field_padding_h: 8,
            field_padding_v: 6,
            field_min_width: 80,

            scrollbar_width: 8,
            scrollbar_thumb_min: 20,

            tab_height: 34,
            menu_item_height: 26,
            menu_min_width: 160,
            row_height: 22,

            checkbox_size: 16,
            checkbox_gap: 8,
        }
    }
}

impl Default for StyleSheet {
    fn default() -> Self {
        Self::dark()
    }
}
