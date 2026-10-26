#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Weight {
    #[default]
    Regular,
    Semibold,
}

/// A size and weight in pixels, on a line box of its own height, and the
/// size an icon set beside it takes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TextStyle {
    pub size: u16,
    pub weight: Weight,
    pub line_height: i32,
    pub icon: i32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TextRole {
    Display,
    Title,
    Heading,
    Body,
    Label,
    Caption,
    /// Code and command lines, in the fixed-cell face.
    Mono,
}

impl TextRole {
    pub const ALL: [TextRole; 7] = [
        TextRole::Display,
        TextRole::Title,
        TextRole::Heading,
        TextRole::Body,
        TextRole::Label,
        TextRole::Caption,
        TextRole::Mono,
    ];

    pub const fn style(self) -> TextStyle {
        let (size, weight, line_height, icon) = match self {
            TextRole::Display => (24, Weight::Semibold, 32, 32),
            TextRole::Title => (18, Weight::Semibold, 24, 20),
            TextRole::Heading => (15, Weight::Semibold, 20, 18),
            TextRole::Body => (14, Weight::Regular, 20, 16),
            TextRole::Label => (13, Weight::Regular, 18, 16),
            TextRole::Caption => (12, Weight::Regular, 16, 14),
            TextRole::Mono => (13, Weight::Regular, 18, 16),
        };
        TextStyle {
            size,
            weight,
            line_height,
            icon,
        }
    }
}
