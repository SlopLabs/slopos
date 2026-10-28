//! Text for appkit and the compositor's chrome: text in a role ([`ui`]), set
//! in the fixed-cell atlas for [`TextRole::Mono`], and the icon font ([`icons`]).

mod loader;

pub use loader::{fonts_in, load_font};

use std::sync::OnceLock;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};
use slopos_font::atlas::GlyphAtlas;
use slopos_theme_core::TextRole;

static ATLAS: OnceLock<Option<GlyphAtlas>> = OnceLock::new();

fn atlas() -> Option<&'static GlyphAtlas> {
    ATLAS
        .get_or_init(|| GlyphAtlas::new(load_font("mono")?, TextRole::Mono.style().size))
        .as_ref()
}

const FALLBACK_CELL_W: i32 = 8;
const FALLBACK_CELL_H: i32 = 16;
/// A cell's baseline on its last row, as an atlas built without metrics has it.
const FALLBACK_ASCENT: i32 = FALLBACK_CELL_H;

pub fn cell_width() -> i32 {
    atlas().map_or(FALLBACK_CELL_W, |a| a.cell_width())
}

pub fn cell_height() -> i32 {
    atlas().map_or(FALLBACK_CELL_H, |a| a.cell_height())
}

/// The row of a cell its baseline is on.
pub fn cell_ascent() -> i32 {
    atlas().map_or(FALLBACK_ASCENT, |a| a.ascent())
}

fn draw_str_clipped<T: Canvas>(
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

fn string_width(text: &str) -> i32 {
    slopos_font::atlas::cells(text) * cell_width()
}

fn prefix_within(text: &str, budget: i32) -> usize {
    slopos_font::atlas::prefix_within(text, budget.max(0) / cell_width().max(1))
}

/// Text in a [`TextRole`]. Each face's renderer keeps the glyph cache a redrawn
/// frame reuses, behind a `RefCell` borrowed for one call at a time.
pub mod ui {
    use std::cell::RefCell;

    use slopos_abi::damage::DamageRect;
    use slopos_abi::draw::{Canvas, Color32};
    use slopos_font::FontRenderer;
    use slopos_theme_core::{TextRole, Weight};

    /// What every cut-short text ends with.
    pub const ELLIPSIS: &str = "\u{2026}";

    /// What a password field shows for each character typed.
    pub const BULLET: &str = "\u{2022}";

    thread_local! {
        static REGULAR: RefCell<Option<FontRenderer<'static>>> = RefCell::new(load("sans"));
        static SEMIBOLD: RefCell<Option<FontRenderer<'static>>> = RefCell::new(load("sans-semibold"));
    }

    fn load(name: &str) -> Option<FontRenderer<'static>> {
        FontRenderer::new(super::load_font(name)?)
    }

    /// Runs `f` on Inter in `role`'s weight, regular when that weight is not
    /// installed; `None` sets the text in the atlas, as [`TextRole::Mono`] is
    /// always, so its columns line up with a code view's.
    fn with<R>(role: TextRole, f: impl FnOnce(&mut FontRenderer<'static>, u16) -> R) -> Option<R> {
        if role == TextRole::Mono {
            return None;
        }
        let style = role.style();
        let mut f = Some(f);
        let mut on = |cell: &RefCell<Option<FontRenderer<'static>>>| {
            let mut slot = cell.borrow_mut();
            let renderer = slot.as_mut()?;
            Some((f.take().expect("one call"))(renderer, style.size))
        };
        if style.weight == Weight::Semibold
            && let Some(r) = SEMIBOLD.with(&mut on)
        {
            return Some(r);
        }
        REGULAR.with(&mut on)
    }

    pub fn width(text: &str, role: TextRole) -> i32 {
        with(role, |r, size| r.text_width(text, size)).unwrap_or_else(|| super::string_width(text))
    }

    /// From the top of a line of `role` text, where [`draw`] puts it, to its
    /// baseline.
    pub fn ascent(role: TextRole) -> i32 {
        with(role, |r, size| r.ascent(size)).unwrap_or_else(super::cell_ascent)
    }

    /// Ascender to descender of the face `role` is set in: the part of the
    /// role's line box its text is drawn in.
    pub fn font_height(role: TextRole) -> i32 {
        with(role, |r, size| r.line_height(size)).unwrap_or_else(super::cell_height)
    }

    /// The top at which a line of `role` text sits centred in the box
    /// `height` tall at `top`.
    pub fn centred_top(top: i32, height: i32, role: TextRole) -> i32 {
        top + (height - font_height(role)) / 2
    }

    /// The baseline of that centred line, from the box's top.
    pub fn baseline_in(height: i32, role: TextRole) -> i32 {
        centred_top(0, height, role) + ascent(role)
    }

    /// The longest prefix of `text` that fits `budget` pixels, in bytes.
    pub fn prefix_fitting(text: &str, role: TextRole, budget: i32) -> usize {
        with(role, |r, size| r.prefix_fitting(text, size, budget))
            .unwrap_or_else(|| super::prefix_within(text, budget))
    }

    /// What of `text` fits `max_w`: all of it, or the longest prefix that
    /// leaves room for [`ELLIPSIS`] after it, with `true` when the ellipsis
    /// follows; nothing when not even the ellipsis fits.
    pub fn elide_end(text: &str, role: TextRole, max_w: i32) -> (&str, bool) {
        if width(text, role) <= max_w {
            return (text, false);
        }
        let room = max_w - width(ELLIPSIS, role);
        if room < 0 {
            return ("", false);
        }
        let keep = prefix_fitting(text, role, room);
        (text[..keep].trim_end(), true)
    }

    /// [`elide_end`] from the start, keeping the longest suffix that leaves
    /// room for [`ELLIPSIS`] before it, as a file name's end tells it apart.
    pub fn elide_start(text: &str, role: TextRole, max_w: i32) -> (&str, bool) {
        if width(text, role) <= max_w {
            return (text, false);
        }
        let room = max_w - width(ELLIPSIS, role);
        if room < 0 {
            return ("", false);
        }
        let starts: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        let first = starts.partition_point(|&at| width(&text[at..], role) > room);
        let kept = starts.get(first).map_or("", |&at| &text[at..]);
        (kept.trim_start(), true)
    }

    /// The width a line cut short to `max_w` covers, its ellipsis included.
    pub fn elided_width(text: &str, role: TextRole, max_w: i32) -> i32 {
        let (kept, cut) = elide_end(text, role, max_w);
        width(kept, role) + if cut { width(ELLIPSIS, role) } else { 0 }
    }

    /// `text` with its line's top-left at `(x, y)`. A transparent `bg`
    /// composites over what the surface holds.
    #[allow(clippy::too_many_arguments)]
    pub fn draw<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let drawn = with(role, |r, size| {
            r.draw_text_clipped(target, x, y, text, size, fg, bg, clip);
        });
        if drawn.is_none() {
            super::draw_str_clipped(target, x, y, text, fg, bg, clip);
        }
    }

    /// [`draw`] of `text` cut short to `max_w` by [`elide_end`]; answers
    /// whether any of it was cut.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_elided<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        max_w: i32,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) -> bool {
        let (kept, cut) = elide_end(text, role, max_w);
        draw(target, x, y, kept, role, fg, bg, clip);
        if cut {
            let after = x + width(kept, role);
            draw(target, after, y, ELLIPSIS, role, fg, bg, clip);
        }
        kept.len() < text.len()
    }
}

/// The icon font's glyphs.
pub mod icons {
    use std::cell::RefCell;

    use slopos_abi::damage::DamageRect;
    use slopos_abi::draw::{Canvas, Color32};
    use slopos_font::FontRenderer;
    use slopos_theme_core::Icon;

    thread_local! {
        static FONT: RefCell<Option<FontRenderer<'static>>> =
            RefCell::new(super::load_font("icons").and_then(FontRenderer::new));
    }

    /// `icon` in the `size × size` box at `(x, y)`; nothing when the font is
    /// missing.
    pub fn draw_clipped<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        icon: Icon,
        size: i32,
        color: Color32,
        clip: &DamageRect,
    ) {
        let mut glyph = [0u8; 4];
        let glyph = icon.codepoint().encode_utf8(&mut glyph);
        FONT.with(|cell| {
            if let Some(font) = cell.borrow_mut().as_mut() {
                let px = size.clamp(1, u16::MAX as i32) as u16;
                font.draw_text_clipped(target, x, y, glyph, px, color, Color32::TRANSPARENT, clip);
            }
        });
    }
}
