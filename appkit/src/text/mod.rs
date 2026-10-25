//! Text for appkit widgets and the compositor's chrome, in two roles.
//!
//! **UI text** is proportional (Inter, [`ui`]): every label, button, menu,
//! table header, window title and system dialog goes through it, because
//! chrome set in a fixed cell reads as a terminal rather than as an
//! application.
//!
//! **Code text** is the fixed-cell [`GlyphAtlas`] the terminal already uses, and
//! stays fixed-cell on purpose: a column is a multiple of one advance, which is
//! what lets a code view map a pixel to a column by dividing.
//!
//! Both fall back to the same atlas when a font file is missing, so a caller
//! never has to ask which one it got.

mod loader;

use std::sync::OnceLock;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;
use slopos_theme_core::{TextRole, TextStyle};

/// The line every label, button and field sets its text on.
pub const BODY: TextStyle = TextRole::Body.style();

static ATLAS: OnceLock<Option<GlyphAtlas>> = OnceLock::new();

fn atlas() -> Option<&'static GlyphAtlas> {
    ATLAS
        .get_or_init(|| {
            let font_data = loader::load_font("mono")?;
            GlyphAtlas::new_with_source(
                font_data,
                TextRole::Mono.style().size,
                slopos_font::FontSource::Filesystem,
            )
        })
        .as_ref()
}

const FALLBACK_CELL_W: i32 = 8;
const FALLBACK_CELL_H: i32 = 16;

pub fn cell_width() -> i32 {
    atlas().map_or(FALLBACK_CELL_W, |a| a.cell_width())
}

pub fn cell_height() -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| a.cell_height())
}

pub fn draw_char<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas()?.draw_char(target, x, y, ch as u32, fg, bg)
}

pub fn draw_string<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
) -> Option<DamageRect> {
    atlas()?.draw_str(target, x, y, text, fg, bg)
}

pub fn draw_str_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    text: &str,
    fg: Color32,
    bg: Color32,
    clip: &DamageRect,
) {
    if let Some(a) = atlas() {
        a.draw_str_clipped(target, x, y, text, fg, bg, clip);
    }
}

pub fn draw_char_clipped<T: Canvas>(
    target: &mut T,
    x: i32,
    y: i32,
    ch: u8,
    fg: Color32,
    bg: Color32,
    clip: &DamageRect,
) {
    if let Some(a) = atlas() {
        a.draw_char_clipped(target, x, y, ch as u32, fg, bg, clip);
    }
}

pub fn string_width(text: &str) -> i32 {
    atlas().map_or(text.len() as i32 * FALLBACK_CELL_W, |a| a.str_width(text))
}

pub fn string_height(text: &str) -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| {
        a.bytes_lines(text.as_bytes()) * a.cell_height()
    })
}

/// Proportional UI text.
///
/// One [`slopos_font::FontRenderer`] per weight, each owning the glyph cache
/// that keeps a re-drawn frame from re-rasterizing. The renderer needs `&mut`
/// for that cache, so it lives in a `RefCell`: painting is single-threaded, and
/// every borrow here is confined to one call.
pub mod ui {
    use std::cell::RefCell;

    use slopos_abi::damage::DamageRect;
    use slopos_abi::draw::{Canvas, Color32};
    use slopos_font::{FontRenderer, FontSource};

    /// What every cut-short text ends with; Inter draws it.
    pub const ELLIPSIS: &str = "\u{2026}";

    /// What a password field shows for each character typed.
    pub const BULLET: &str = "\u{2022}";

    /// [`Weight::Semibold`] falls back to regular when no semibold file is
    /// installed.
    pub use slopos_theme_core::Weight;

    thread_local! {
        static REGULAR: RefCell<Option<FontRenderer<'static>>> = RefCell::new(load("sans"));
        static SEMIBOLD: RefCell<Option<FontRenderer<'static>>> = RefCell::new(load("sans-semibold"));
    }

    fn load(name: &str) -> Option<FontRenderer<'static>> {
        let data = super::loader::load_font(name)?;
        FontRenderer::new_with_source(data, FontSource::Filesystem)
    }

    /// Runs `f` against the renderer for `weight`, falling back to regular when
    /// that weight is not installed. `None` when no proportional font is.
    fn with<R>(weight: Weight, f: impl FnOnce(&mut FontRenderer<'static>) -> R) -> Option<R> {
        let mut f = Some(f);
        if weight == Weight::Semibold {
            let done = SEMIBOLD.with(|cell| {
                let mut slot = cell.borrow_mut();
                slot.as_mut().map(|r| (f.take().expect("one call"))(r))
            });
            if let Some(r) = done {
                return Some(r);
            }
        }
        REGULAR.with(|cell| {
            let mut slot = cell.borrow_mut();
            slot.as_mut().map(|r| (f.take().expect("one call"))(r))
        })
    }

    /// True when a proportional font is installed; false means every call here
    /// falls back to the fixed-cell atlas.
    pub fn available() -> bool {
        REGULAR.with(|cell| cell.borrow().is_some())
    }

    pub fn width(text: &str, size_px: u16) -> i32 {
        width_weighted(text, size_px, Weight::Regular)
    }

    pub fn width_weighted(text: &str, size_px: u16, weight: Weight) -> i32 {
        with(weight, |r| r.text_width(text, size_px)).unwrap_or_else(|| super::string_width(text))
    }

    pub fn line_height(size_px: u16) -> i32 {
        with(Weight::Regular, |r| r.line_height(size_px)).unwrap_or_else(super::cell_height)
    }

    /// Distance from the top of a line, where [`draw_clipped`] puts it, to its
    /// baseline. Without a proportional font, an estimate for the atlas.
    pub fn ascent(size_px: u16) -> i32 {
        with(Weight::Regular, |r| r.ascent(size_px)).unwrap_or_else(|| super::cell_height() * 4 / 5)
    }

    /// The top at which a line of `size_px` text sits centred in a box of
    /// `height` at `top`: the line box every one-line text in a taller row uses.
    pub fn centred_top(top: i32, height: i32, size_px: u16) -> i32 {
        top + (height - line_height(size_px)) / 2
    }

    /// Baseline of that centred line, from the box's top.
    pub fn baseline_in(height: i32, size_px: u16) -> i32 {
        centred_top(0, height, size_px) + ascent(size_px)
    }

    /// What of `text` fits `max_w` pixels: all of it, or the longest prefix
    /// that leaves room for [`ELLIPSIS`] after it, with `true` when the
    /// ellipsis follows; nothing when not even the ellipsis fits.
    ///
    /// A slice and a flag rather than a joined string, so a frame that draws a
    /// cut-short title allocates nothing: [`draw_elided`] draws the two apart.
    pub fn elide_end(text: &str, size_px: u16, weight: Weight, max_w: i32) -> (&str, bool) {
        if width_weighted(text, size_px, weight) <= max_w {
            return (text, false);
        }
        let room = max_w - width_weighted(ELLIPSIS, size_px, weight);
        if room < 0 {
            return ("", false);
        }
        let keep = prefix_fitting(text, size_px, room, weight);
        (text[..keep].trim_end(), true)
    }

    /// The width [`draw_elided`] covers.
    pub fn elided_width(text: &str, size_px: u16, weight: Weight, max_w: i32) -> i32 {
        let (kept, cut) = elide_end(text, size_px, weight, max_w);
        let ellipsis = if cut {
            width_weighted(ELLIPSIS, size_px, weight)
        } else {
            0
        };
        width_weighted(kept, size_px, weight) + ellipsis
    }

    /// [`draw_clipped`] of `text` cut short to `max_w` pixels by
    /// [`elide_end`], the ellipsis after what is kept.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_elided<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        weight: Weight,
        max_w: i32,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let (kept, cut) = elide_end(text, size_px, weight, max_w);
        draw_clipped(target, x, y, kept, size_px, weight, fg, bg, clip);
        if cut {
            let after = x + width_weighted(kept, size_px, weight);
            draw_clipped(target, after, y, ELLIPSIS, size_px, weight, fg, bg, clip);
        }
    }

    /// Longest prefix of `text` that fits in `budget` pixels, in bytes.
    pub fn prefix_fitting(text: &str, size_px: u16, budget: i32, weight: Weight) -> usize {
        with(weight, |r| r.prefix_fitting(text, size_px, budget)).unwrap_or_else(|| {
            let cw = super::cell_width().max(1);
            let chars = (budget.max(0) / cw) as usize;
            text.char_indices()
                .nth(chars)
                .map(|(b, _)| b)
                .unwrap_or(text.len())
        })
    }

    /// Draws `text` with its line box's top-left at `(x, y)`, clipped to `clip`.
    ///
    /// A transparent `bg` composites onto what the surface holds, which is what
    /// keeps ink over a selection or a hovered row free of dark fringes.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_clipped<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        weight: Weight,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let drawn = with(weight, |r| {
            r.draw_text_clipped(target, x, y, text, size_px, fg, bg, clip);
        });
        if drawn.is_none() {
            super::draw_str_clipped(target, x, y, text, fg, bg, clip);
        }
    }
}
