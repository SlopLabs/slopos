//! The desktop's design tokens, which appkit and the compositor both read
//! through [`current`], so a second theme is a second table and no app changes.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod contrast;
mod dark;
mod icon;
mod insets;
mod text;
pub mod words;

use slopos_abi::draw::Color32;

pub use icon::Icon;
pub use insets::EdgeInsets;
pub use text::{TextRole, TextStyle, Weight};

pub fn current() -> &'static Theme {
    &dark::DARK
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorRole {
    /// A window's content.
    Surface,
    /// What floats above a surface: cards, popovers, dialogs.
    SurfaceRaised,
    /// What is set into one: fields, lists, sidebars.
    SurfaceSunken,
    /// Title bars and menu bars.
    Chrome,
    /// Laid over everything below a modal question.
    Scrim,
    Border,
    /// A control's edge, held to 3:1 against what it sits on.
    BorderStrong,
    Text,
    TextSecondary,
    TextDisabled,
    TextOnAccent,
    Accent,
    Danger,
    Warning,
    Success,
    Info,
    Focus,
    /// Laid over selected text and rows; translucent.
    Selection,
    /// Deep enough for `TextOnAccent`; `Accent` is the same hue as text ink.
    AccentFill,
    DangerFill,
    WarningFill,
    SuccessFill,
    InfoFill,
    NeutralFill,
    Control,
}

/// The ink a text or icon takes.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Tone {
    #[default]
    Normal,
    Secondary,
    Disabled,
    Accent,
    Danger,
    Warning,
    Success,
    Info,
}

impl Tone {
    pub const fn role(self) -> ColorRole {
        match self {
            Tone::Normal => ColorRole::Text,
            Tone::Secondary => ColorRole::TextSecondary,
            Tone::Disabled => ColorRole::TextDisabled,
            Tone::Accent => ColorRole::Accent,
            Tone::Danger => ColorRole::Danger,
            Tone::Warning => ColorRole::Warning,
            Tone::Success => ColorRole::Success,
            Tone::Info => ColorRole::Info,
        }
    }

    pub const fn fill(self) -> ColorRole {
        match self {
            Tone::Normal | Tone::Secondary | Tone::Disabled => ColorRole::NeutralFill,
            Tone::Accent => ColorRole::AccentFill,
            Tone::Danger => ColorRole::DangerFill,
            Tone::Warning => ColorRole::WarningFill,
            Tone::Success => ColorRole::SuccessFill,
            Tone::Info => ColorRole::InfoFill,
        }
    }
}

/// The fill behind a pane.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Surface {
    #[default]
    Base,
    Raised,
    Sunken,
    Chrome,
}

impl Surface {
    pub const fn role(self) -> ColorRole {
        match self {
            Surface::Base => ColorRole::Surface,
            Surface::Raised => ColorRole::SurfaceRaised,
            Surface::Sunken => ColorRole::SurfaceSunken,
            Surface::Chrome => ColorRole::Chrome,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum State {
    #[default]
    Normal,
    Hover,
    Pressed,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Shadow {
    /// At its darkest, at the edge.
    pub color: Color32,
    /// How far past the edge it reaches, in pixels.
    pub spread: i32,
    pub offset_y: i32,
}

/// A gradient from `top` to `bottom` lit by a translucent `light` from above.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Wallpaper {
    pub top: Color32,
    pub bottom: Color32,
    pub light: Color32,
}

/// Distinct hues for things told apart by colour alone: a disk's partitions,
/// a chart's series.
pub const SERIES: usize = 7;

/// FNV-1a, so a name keeps its colour wherever it is shown.
fn fnv1a(name: &str) -> u32 {
    name.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

pub fn series_of(name: &str) -> usize {
    fnv1a(name) as usize % SERIES
}

pub const APP_TILES: usize = 8;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AppTile {
    Blue,
    Green,
    Orange,
    Red,
    Violet,
    Teal,
    Pink,
    Graphite,
}

impl AppTile {
    pub const ALL: [AppTile; APP_TILES] = [
        AppTile::Blue,
        AppTile::Green,
        AppTile::Orange,
        AppTile::Red,
        AppTile::Violet,
        AppTile::Teal,
        AppTile::Pink,
        AppTile::Graphite,
    ];

    /// Never graphite, which is kept for tools.
    pub fn of(id: &str) -> AppTile {
        Self::ALL[fnv1a(id) as usize % (APP_TILES - 1)]
    }
}

/// Share of white mixed into a tile's top-left corner, out of 255.
pub const APP_TILE_LIGHT: u8 = 0x20;

pub struct Theme {
    pub surface: Color32,
    pub surface_raised: Color32,
    pub surface_sunken: Color32,
    pub chrome: Color32,
    pub scrim: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    /// Quieter than `border_strong`, which only a lone box or knob needs.
    pub edge: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_disabled: Color32,
    pub text_on_accent: Color32,
    pub accent: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub success: Color32,
    pub info: Color32,
    pub focus: Color32,
    pub selection: Color32,
    pub accent_fill: Color32,
    pub danger_fill: Color32,
    pub warning_fill: Color32,
    pub success_fill: Color32,
    pub info_fill: Color32,
    pub neutral_fill: Color32,
    pub control: Color32,
    pub series: [Color32; SERIES],
    /// Light line inks, each paired with its `series` fill.
    pub series_ink: [Color32; SERIES],
    /// What hover and pressed mix over a surface, as libadwaita lays
    /// `currentColor` over a widget.
    state_ink: Color32,
    pub shade: Color32,
    pub rim_light: Color32,
    pub rim_dark: Color32,
    /// Translucent, over the wallpaper.
    pub glass: Color32,
    pub wallpaper: Wallpaper,
    pub app_tiles: [Color32; APP_TILES],
    /// Cards, popovers and dialogs.
    pub shadow_raised: Shadow,
    pub shadow_window: Shadow,
}

/// Share of the state's ink in a hovered and in a pressed colour, out of 255:
/// libadwaita's 7% and 16%.
const fn state_mix(state: State) -> u8 {
    match state {
        State::Normal => 0,
        State::Hover => 18,
        State::Pressed => 41,
    }
}

/// Share of `shade` in a pressed fill, out of 255: libadwaita's 20%.
const PRESSED_SHADE: u8 = 51;

impl Theme {
    const fn color(&self, role: ColorRole) -> Color32 {
        match role {
            ColorRole::Surface => self.surface,
            ColorRole::SurfaceRaised => self.surface_raised,
            ColorRole::SurfaceSunken => self.surface_sunken,
            ColorRole::Chrome => self.chrome,
            ColorRole::Scrim => self.scrim,
            ColorRole::Border => self.border,
            ColorRole::BorderStrong => self.border_strong,
            ColorRole::Text => self.text,
            ColorRole::TextSecondary => self.text_secondary,
            ColorRole::TextDisabled => self.text_disabled,
            ColorRole::TextOnAccent => self.text_on_accent,
            ColorRole::Accent => self.accent,
            ColorRole::Danger => self.danger,
            ColorRole::Warning => self.warning,
            ColorRole::Success => self.success,
            ColorRole::Info => self.info,
            ColorRole::Focus => self.focus,
            ColorRole::Selection => self.selection,
            ColorRole::AccentFill => self.accent_fill,
            ColorRole::DangerFill => self.danger_fill,
            ColorRole::WarningFill => self.warning_fill,
            ColorRole::SuccessFill => self.success_fill,
            ColorRole::InfoFill => self.info_fill,
            ColorRole::NeutralFill => self.neutral_fill,
            ColorRole::Control => self.control,
        }
    }

    const fn is_fill(role: ColorRole) -> bool {
        matches!(
            role,
            ColorRole::AccentFill
                | ColorRole::DangerFill
                | ColorRole::WarningFill
                | ColorRole::SuccessFill
                | ColorRole::InfoFill
                | ColorRole::NeutralFill
        )
    }

    /// `role` as a control in `state` shows it; alpha is kept. A pressed fill
    /// darkens rather than lightens, so its white ink keeps reading.
    pub const fn state(&self, role: ColorRole, state: State) -> Color32 {
        let base = self.color(role);
        match (Self::is_fill(role), state) {
            (_, State::Normal) => base,
            (true, State::Hover) => mix(base, self.text_on_accent, state_mix(state)),
            (true, State::Pressed) => mix(base, self.shade, PRESSED_SHADE),
            (false, _) => mix(base, self.state_ink, state_mix(state)),
        }
    }

    /// What `state` lays over whatever is below it, for a row whose surface
    /// the row does not know.
    pub const fn overlay(&self, state: State) -> Color32 {
        with_alpha(self.state_ink, state_mix(state))
    }

    pub const fn ink(&self, tone: Tone) -> Color32 {
        self.color(tone.role())
    }

    pub const fn filled(&self, tone: Tone) -> Color32 {
        self.color(tone.fill())
    }

    pub const fn fill(&self, surface: Surface) -> Color32 {
        self.color(surface.role())
    }

    /// The `n`th categorical colour, repeating past [`SERIES`].
    pub const fn series(&self, n: usize) -> Color32 {
        self.series[n % SERIES]
    }

    pub const fn series_ink(&self, n: usize) -> Color32 {
        self.series_ink[n % SERIES]
    }

    /// `(base, lit)`: the bottom-right and top-left ends of the gradient.
    pub const fn app_tile(&self, tile: AppTile) -> (Color32, Color32) {
        let base = self.app_tiles[tile as usize];
        (base, mix(base, self.text_on_accent, APP_TILE_LIGHT))
    }
}

/// `over` laid on `base` at `amount` out of 255; `base`'s alpha is kept.
pub const fn mix(base: Color32, over: Color32, amount: u8) -> Color32 {
    const fn channel(b: u8, o: u8, amount: u8) -> u8 {
        let (b, o, a) = (b as u32, o as u32, amount as u32);
        ((b * (255 - a) + o * a + 127) / 255) as u8
    }
    Color32::new(
        channel(base.red(), over.red(), amount),
        channel(base.green(), over.green(), amount),
        channel(base.blue(), over.blue(), amount),
        base.alpha(),
    )
}

/// `color` at `alpha` out of 255, whatever its own alpha was.
pub const fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::new(color.red(), color.green(), color.blue(), alpha)
}

pub mod space {
    pub const XS: i32 = 4;
    pub const SM: i32 = 8;
    pub const MD: i32 = 12;
    pub const LG: i32 = 16;
    pub const XL: i32 = 24;
}

pub mod inset {
    use super::EdgeInsets;
    use super::space::{LG, MD, XL, XS};

    /// Around a page's content.
    pub const PAGE: EdgeInsets = EdgeInsets::symmetric(XL, LG);
    /// Around a band of a page: a header, a footer, a toolbar.
    pub const SECTION: EdgeInsets = EdgeInsets::symmetric(XL, MD);
    pub const CARD: EdgeInsets = EdgeInsets::all(LG);
    /// Inside a button or a field.
    pub const CONTROL: EdgeInsets = EdgeInsets::symmetric(MD, XS);
}

/// Edge widths; a focused control's is thicker, so focus shows by more than
/// a change of hue.
pub mod stroke {
    pub const EDGE: i32 = 1;
    pub const FOCUS: i32 = 2;
}

pub mod radius {
    pub const CONTROL: i32 = 6;
    pub const CARD: i32 = 12;
    pub const WINDOW: i32 = 12;
    pub const PANEL: i32 = 18;
}

#[cfg(test)]
mod tests;
