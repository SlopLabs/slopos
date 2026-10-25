//! TrueType font rasterizer for SlopOS.
//!
//! Parses `.ttf` outline data, rasterizes glyphs with coverage-based
//! anti-aliasing, and caches the results.
//!
//! # Usage
//!
//! ```ignore
//! let font_data: &[u8] = /* load .ttf file */;
//! let mut renderer = FontRenderer::new(font_data).unwrap();
//! renderer.draw_text(&mut canvas, 10, 20, "Hello", 16, Color32::WHITE, Color32::BLACK);
//! ```

#![no_std]
#![forbid(unsafe_code)]

pub mod atlas;
pub mod bitmap;
pub mod boxdraw;
pub mod cache;
pub mod metrics;
pub mod outline;
pub mod rasterizer;
pub mod ttf_parser;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

/// First printable ASCII codepoint (space).
pub const ASCII_FIRST: u32 = 0x20;
/// Last printable ASCII codepoint (tilde).
pub const ASCII_LAST: u32 = 0x7E;

/// The glyph set, in slot order: inclusive codepoint ranges whose slots are
/// contiguous. Ascending and non-overlapping — [`glyph_slot`] relies on both
/// to stop at the first range starting above the codepoint.
pub const GLYPH_RANGES: [(u32, u32); 12] = [
    (0x0020, 0x007E), // printable ASCII
    (0x00A0, 0x00FF), // Latin-1 Supplement
    (0x0100, 0x017F), // Latin Extended-A
    (0x02C6, 0x02DD), // spacing modifier letters (dead-key accents)
    (0x0370, 0x03FF), // Greek and Coptic
    (0x0400, 0x04FF), // Cyrillic
    (0x2010, 0x203E), // General Punctuation
    (0x20A0, 0x20BF), // Currency Symbols
    (0x2190, 0x21FF), // Arrows
    (0x2500, 0x257F), // Box Drawing
    (0x2580, 0x259F), // Block Elements
    (0x25A0, 0x25FF), // Geometric Shapes
];

/// Total glyph slots in an atlas; also the `glyph_count` of the
/// `SYSCALL_FONT_SET` coverage wire format — both sides compile against it.
pub const GLYPH_COUNT: usize = {
    let mut total = 0usize;
    let mut i = 0;
    while i < GLYPH_RANGES.len() {
        let (lo, hi) = GLYPH_RANGES[i];
        total += (hi - lo + 1) as usize;
        i += 1;
    }
    total
};

/// Atlas slot for a codepoint, or `None` for codepoints outside the glyph set
/// (those render as the replacement glyph).
#[inline]
pub fn glyph_slot(cp: u32) -> Option<usize> {
    let mut base = 0usize;
    for (lo, hi) in GLYPH_RANGES {
        if cp < lo {
            return None;
        }
        if cp <= hi {
            return Some(base + (cp - lo) as usize);
        }
        base += (hi - lo + 1) as usize;
    }
    None
}

/// Inverse of [`glyph_slot`]: the codepoint a slot holds.
#[inline]
pub fn slot_codepoint(slot: usize) -> Option<u32> {
    let mut rest = slot;
    for (lo, hi) in GLYPH_RANGES {
        let count = (hi - lo + 1) as usize;
        if rest < count {
            return Some(lo + rest as u32);
        }
        rest -= count;
    }
    None
}

use cache::GlyphCache;
use outline::outline_to_edges;
use rasterizer::{RasterizedGlyph, rasterize};
use ttf_parser::TtfFont;

/// Describes the origin of font data for diagnostics and fallback logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FontSource {
    /// Compiled into the binary via `include_bytes!`.
    Embedded,
    Filesystem,
    /// Loaded via `SYS_FONT_SET` from userland-provided bitmap or coverage data.
    Syscall,
    /// Minimal bitmap fallback (VGA 8\u{d7}16) used when no other font is available.
    BitmapFallback,
}

pub struct FontRenderer<'a> {
    pub(crate) font: TtfFont<'a>,
    cache: GlyphCache,
    source: FontSource,
}

impl<'a> FontRenderer<'a> {
    /// Create a new font renderer from raw TrueType font data.
    ///
    /// Returns `None` if the font data cannot be parsed. Source defaults to
    /// [`FontSource::Embedded`].
    pub fn new(ttf_data: &'a [u8]) -> Option<Self> {
        Self::new_with_source(ttf_data, FontSource::Embedded)
    }

    pub fn new_with_source(ttf_data: &'a [u8], source: FontSource) -> Option<Self> {
        let font = TtfFont::parse(ttf_data)?;
        Some(Self {
            font,
            cache: GlyphCache::new(),
            source,
        })
    }

    pub fn source(&self) -> FontSource {
        self.source
    }

    /// Draw a text string onto a canvas at the given position.
    ///
    /// `(x, y)` is the top-left corner of the text's line: its baseline lands
    /// on row `y + ascent(size_px)`. `bg` is the background colour for
    /// anti-alias blending; pass an opaque colour for MMIO/write-only
    /// surfaces, or `Color32::TRANSPARENT` to composite over existing content
    /// (requires a readable surface). Returns the damage rectangle covering all
    /// rendered glyphs.
    pub fn draw_text<T: Canvas>(
        &mut self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        color: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        self.draw_text_inner(target, x, y, text, size_px, color, bg, None)
    }

    /// [`Self::draw_text`] confined to `clip`; pixels outside it are untouched.
    ///
    /// The advance is unaffected, so a clipped run lands on the same pixels as
    /// the unclipped one — which is what lets a caller scroll text under a
    /// viewport instead of re-laying it out.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_text_clipped<T: Canvas>(
        &mut self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        color: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) -> Option<DamageRect> {
        self.draw_text_inner(target, x, y, text, size_px, color, bg, Some(*clip))
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text_inner<T: Canvas>(
        &mut self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        size_px: u16,
        color: Color32,
        bg: Color32,
        clip: Option<DamageRect>,
    ) -> Option<DamageRect> {
        let scale = scale(&self.font, size_px);
        let baseline = y + self.ascent(size_px);
        let mut damage: Option<DamageRect> = None;

        for placed in Run::new(&self.font, text, size_px) {
            // Pen and bitmap round once, together, to the nearest quarter
            // pixel: the whole pixel places the bitmap, the quarter picks the
            // phase it was rasterised at.
            let quarters = libm::floorf((x as f32 + placed.pen) * 4.0 + 0.5) as i32;
            let column = quarters.div_euclid(4);
            let key = (placed.codepoint, size_px, quarters.rem_euclid(4) as u8);

            if self.cache.get(key).is_none() {
                if let Some(glyph) = rasterize_glyph(&self.font, placed.codepoint, scale, key.2) {
                    self.cache.insert(key, glyph);
                }
            }
            // The cache borrow is held across the blit rather than cloning the
            // coverage: a clone here is one heap allocation per glyph per frame.
            let glyph_damage = self.cache.get(key).and_then(|g| {
                Self::draw_glyph_coverage_clipped(
                    target,
                    column + g.bearing_x as i32,
                    baseline - g.bearing_y as i32,
                    g.width as i32,
                    g.height as i32,
                    g.coverage.as_slice(),
                    color,
                    bg,
                    clip.as_ref(),
                )
            });
            damage = match (damage, glyph_damage) {
                (Some(d), Some(g)) => Some(d.union(&g)),
                (d, g) => d.or(g),
            };
        }

        if let Some(d) = damage {
            target.report_damage(d);
        }
        damage
    }

    /// Measure the width and height of a text string at the given size.
    pub fn measure_text(&self, text: &str, size_px: u16) -> (i32, i32) {
        metrics::measure_text(&self.font, text, size_px)
    }

    /// Advance width of `text` at `size_px`, kerning included, in pixels.
    pub fn text_width(&self, text: &str, size_px: u16) -> i32 {
        metrics::measure_text(&self.font, text, size_px).0
    }

    /// Baseline offset from the top of a line box at `size_px`.
    pub fn ascent(&self, size_px: u16) -> i32 {
        libm::roundf(self.font.hhea().ascender as f32 * scale(&self.font, size_px)) as i32
    }

    /// Ascender-to-descender height of one line at `size_px`.
    pub fn line_height(&self, size_px: u16) -> i32 {
        metrics::measure_text(&self.font, "", size_px).1
    }

    /// Longest prefix of `text` whose advance fits in `budget` pixels, in bytes.
    pub fn prefix_fitting(&self, text: &str, size_px: u16, budget: i32) -> usize {
        if budget <= 0 {
            return 0;
        }
        for placed in Run::new(&self.font, text, size_px) {
            if libm::ceilf(placed.pen + placed.advance) as i32 > budget {
                return placed.offset;
            }
        }
        text.len()
    }

    /// Draw a coverage bitmap onto a canvas.
    ///
    /// An opaque `bg` blends fg/bg directly, with no framebuffer read-back, so
    /// it is safe over MMIO; a transparent `bg` composites onto the existing
    /// pixel and requires a readable surface like `DrawBuffer`.
    /// Confined to `clip` when one is given, and to the buffer bounds always.
    ///
    /// A transparent `bg` composites each partial-coverage pixel onto what the
    /// surface already holds, so anti-aliased ink over a coloured panel is the
    /// panel's colour rather than a dark fringe. A write-only surface reads back
    /// 0 and so still blends towards black, which is what it did before.
    #[allow(clippy::too_many_arguments)]
    fn draw_glyph_coverage_clipped<T: Canvas>(
        target: &mut T,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        coverage: &[u8],
        color: Color32,
        bg: Color32,
        clip: Option<&DamageRect>,
    ) -> Option<DamageRect> {
        if w <= 0 || h <= 0 || coverage.is_empty() {
            return None;
        }

        let buf_w = target.width() as i32;
        let buf_h = target.height() as i32;

        let mut x0 = x.max(0);
        let mut y0 = y.max(0);
        let mut x1 = (x + w - 1).min(buf_w - 1);
        let mut y1 = (y + h - 1).min(buf_h - 1);
        if let Some(c) = clip {
            x0 = x0.max(c.x0);
            y0 = y0.max(c.y0);
            x1 = x1.min(c.x1);
            y1 = y1.min(c.y1);
        }

        if x0 > x1 || y0 > y1 {
            return None;
        }

        let has_bg = bg.0 != 0;
        let fmt = target.pixel_format();

        for row in y0..=y1 {
            for col in x0..=x1 {
                let cov_x = (col - x) as usize;
                let cov_y = (row - y) as usize;
                let idx = cov_y * (w as usize) + cov_x;

                if idx < coverage.len() {
                    let cov = coverage[idx];
                    if cov == 0 {
                        continue;
                    }
                    if cov == 255 {
                        target.put_pixel(col, row, fmt.encode(color));
                    } else {
                        let under = if has_bg {
                            bg
                        } else {
                            fmt.decode(target.read_pixel(col, row))
                        };
                        let blended = atlas::blend_color32(cov, color, under);
                        target.put_pixel(col, row, fmt.encode(blended));
                    }
                }
            }
        }

        Some(DamageRect { x0, y0, x1, y1 })
    }
}

/// Pixels per font unit at `size_px`. `TtfFont::parse` refuses a zero
/// `unitsPerEm`, so this never divides by zero.
#[inline]
pub(crate) fn scale(font: &TtfFont<'_>, size_px: u16) -> f32 {
    size_px as f32 / font.units_per_em() as f32
}

/// The glyph a codepoint draws with. `.notdef` is "no glyph": measuring its
/// advance while refusing to draw it is how a measured width and a drawn run
/// would disagree on exactly the codepoints the half-em fallback exists for.
#[inline]
fn drawable_glyph(font: &TtfFont<'_>, codepoint: u32) -> Option<u16> {
    font.glyph_index(codepoint)
        .filter(|&gid| gid != 0 || codepoint == 0)
}

/// One glyph of a laid-out run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placed {
    /// Byte offset of its character in the text.
    pub offset: usize,
    pub codepoint: u32,
    /// Where its origin sits, in pixels from the run's, kerning included.
    pub pen: f32,
    pub advance: f32,
}

/// A run's glyphs in order, each at its unrounded pen. Measuring, fitting and
/// drawing all walk this one layout, so what is measured is what is drawn.
pub(crate) struct Run<'f, 'a, 't> {
    font: &'f TtfFont<'a>,
    chars: core::str::CharIndices<'t>,
    scale: f32,
    size_px: u16,
    pen: f32,
    previous: Option<u16>,
}

impl<'f, 'a, 't> Run<'f, 'a, 't> {
    pub(crate) fn new(font: &'f TtfFont<'a>, text: &'t str, size_px: u16) -> Self {
        Self {
            font,
            chars: text.char_indices(),
            scale: scale(font, size_px),
            size_px,
            pen: 0.0,
            previous: None,
        }
    }
}

impl Iterator for Run<'_, '_, '_> {
    type Item = Placed;

    fn next(&mut self) -> Option<Placed> {
        let (offset, ch) = self.chars.next()?;
        let codepoint = ch as u32;
        let glyph = drawable_glyph(self.font, codepoint);
        if let (Some(left), Some(right)) = (self.previous, glyph) {
            self.pen += self.font.kerning(left, right) as f32 * self.scale;
        }
        let advance = glyph
            .and_then(|gid| self.font.h_metrics(gid))
            .map_or(self.size_px as f32 * 0.5, |hm| {
                hm.advance_width as f32 * self.scale
            });
        let placed = Placed {
            offset,
            codepoint,
            pen: self.pen,
            advance,
        };
        self.pen += advance;
        self.previous = glyph;
        Some(placed)
    }
}

/// Width of `text`'s run at `size_px`, unrounded.
pub(crate) fn run_width(font: &TtfFont<'_>, text: &str, size_px: u16) -> f32 {
    Run::new(font, text, size_px)
        .last()
        .map_or(0.0, |last| last.pen + last.advance)
}

/// Rasterize one glyph with its origin `phase` quarter pixels right of a pixel
/// boundary.
///
/// The bitmap's box is whole pixels: its top is `ceil(y_max · scale)` above
/// the baseline and its left `floor(x_min · scale + phase / 4)` right of the
/// origin, with the outline drawn at its exact offset inside. The baseline
/// therefore falls on a row boundary in every glyph, so a run's glyphs share
/// one bottom edge.
pub(crate) fn rasterize_glyph(
    font: &TtfFont<'_>,
    codepoint: u32,
    scale: f32,
    phase: u8,
) -> Option<RasterizedGlyph> {
    let glyph_id = drawable_glyph(font, codepoint)?;
    let hm = font.h_metrics(glyph_id)?;
    let empty = RasterizedGlyph {
        width: 0,
        height: 0,
        bearing_x: 0,
        bearing_y: 0,
        advance: libm::roundf(hm.advance_width as f32 * scale) as u16,
        coverage: slopos_ostd::KVec::new(),
    };
    let Some(outline) = font
        .glyph_outline(glyph_id)
        .filter(|outline| !outline.contours.is_empty())
    else {
        return Some(empty);
    };

    let shift = phase as f32 * 0.25;
    let left = libm::floorf(outline.x_min as f32 * scale + shift);
    let right = libm::ceilf(outline.x_max as f32 * scale + shift);
    let top = libm::ceilf(outline.y_max as f32 * scale);
    let bottom = libm::floorf(outline.y_min as f32 * scale);
    let width = (right - left) as i32;
    let height = (top - bottom) as i32;
    if width <= 0 || height <= 0 {
        return Some(empty);
    }

    let edges = outline_to_edges(&outline, scale, shift - left, top);
    let coverage = rasterize(&edges, width as usize, height as usize)?;
    Some(RasterizedGlyph {
        width: width as u16,
        height: height as u16,
        bearing_x: left as i16,
        bearing_y: top as i16,
        coverage,
        ..empty
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::GlyphAtlas;
    use slopos_abi::draw::{Canvas, EncodedPixel};
    use slopos_abi::pixel::PixelFormat;

    const INTER_TTF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../assets/fonts/Inter-Regular.ttf"
    ));

    struct TestCanvas {
        data: slopos_ostd::KVec<u8>,
        width: u32,
        height: u32,
    }

    impl TestCanvas {
        fn new(width: u32, height: u32) -> Self {
            Self {
                data: slopos_ostd::KVec::<u8>::zeroed((width * height * 4) as usize)
                    .expect("test alloc"),
                width,
                height,
            }
        }

        fn has_any_nonzero(&self) -> bool {
            self.data.iter().any(|&b| b != 0)
        }

        fn read_pixel(&self, x: u32, y: u32) -> Color32 {
            let off = (y * self.width + x) as usize * 4;
            let raw = u32::from_le_bytes([
                self.data[off],
                self.data[off + 1],
                self.data[off + 2],
                self.data[off + 3],
            ]);
            PixelFormat::Argb8888.decode(raw)
        }
    }

    impl Canvas for TestCanvas {
        fn width(&self) -> u32 {
            self.width
        }
        fn height(&self) -> u32 {
            self.height
        }
        fn pitch_bytes(&self) -> usize {
            self.width as usize * 4
        }
        fn bytes_per_pixel(&self) -> u8 {
            4
        }
        fn pixel_format(&self) -> PixelFormat {
            PixelFormat::Argb8888
        }
        fn write_encoded_at(&mut self, off: usize, pixel: EncodedPixel) {
            let bytes = pixel.to_u32().to_le_bytes();
            if off + 4 <= self.data.len() {
                self.data[off..off + 4].copy_from_slice(&bytes);
            }
        }
        fn read_encoded_at(&self, off: usize) -> u32 {
            if off + 4 <= self.data.len() {
                u32::from_le_bytes([
                    self.data[off],
                    self.data[off + 1],
                    self.data[off + 2],
                    self.data[off + 3],
                ])
            } else {
                0
            }
        }
    }

    #[test]
    fn parse_inter_font() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        assert!(font.units_per_em() > 0);
        assert!(font.num_glyphs() > 100);
    }

    #[test]
    fn glyph_index_ascii() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        assert!(gid > 0);
        assert!(font.glyph_index(' ' as u32).is_some());
    }

    #[test]
    fn glyph_outline_has_contours() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        let outline = font.glyph_outline(gid).unwrap();
        assert!(!outline.contours.is_empty());
    }

    #[test]
    fn h_metrics_for_ascii() {
        let font = ttf_parser::TtfFont::parse(INTER_TTF).unwrap();
        let gid = font.glyph_index('A' as u32).unwrap();
        let hm = font.h_metrics(gid).unwrap();
        assert!(hm.advance_width > 0);
    }

    #[test]
    fn measure_text_nonzero() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w, h) = renderer.measure_text("Hello", 16);
        assert!(w > 0 && h > 0, "w={w}, h={h}");
    }

    #[test]
    fn measure_empty_text_zero_width() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w, _) = renderer.measure_text("", 16);
        assert_eq!(w, 0);
    }

    #[test]
    fn draw_text_produces_pixels() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(200, 50);
        let d = renderer.draw_text(
            &mut canvas,
            10,
            10,
            "Hello",
            16,
            Color32::WHITE,
            Color32::BLACK,
        );
        assert!(d.is_some());
        assert!(canvas.has_any_nonzero());
    }

    #[test]
    fn draw_text_damage_rect_sane() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(200, 50);
        let d = renderer
            .draw_text(
                &mut canvas,
                10,
                10,
                "AB",
                16,
                Color32::WHITE,
                Color32::BLACK,
            )
            .unwrap();
        assert!(d.x0 >= 0 && d.y0 >= 0 && d.x1 < 200 && d.y1 < 50);
        assert!(d.x1 - d.x0 > 5, "width {} too small", d.x1 - d.x0);
    }

    #[test]
    fn rasterizer_full_coverage_reaches_255() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut canvas = TestCanvas::new(100, 100);
        renderer.draw_text(&mut canvas, 10, 10, "O", 48, Color32::WHITE, Color32::BLACK);
        let max_a = (0..100u32)
            .flat_map(|y| (0..100u32).map(move |x| (x, y)))
            .map(|(x, y)| canvas.read_pixel(x, y).alpha())
            .max()
            .unwrap_or(0);
        assert_eq!(max_a, 255, "max coverage={max_a}, expected 255");
    }

    #[test]
    fn different_sizes_produce_different_metrics() {
        let r1 = FontRenderer::new(INTER_TTF).unwrap();
        let r2 = FontRenderer::new(INTER_TTF).unwrap();
        let (w1, h1) = r1.measure_text("A", 12);
        let (w2, h2) = r2.measure_text("A", 24);
        assert!(w2 > w1, "24px should be wider than 12px");
        assert!(h2 > h1, "24px should be taller than 12px");
    }

    #[test]
    fn space_advances_cursor() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let (w_a, _) = renderer.measure_text("A", 16);
        let (w_aa, _) = renderer.measure_text("A A", 16);
        assert!(w_aa > w_a * 2 - 5, "A={w_a}, A_A={w_aa}");
    }

    #[test]
    fn font_source_default_is_embedded() {
        let r = FontRenderer::new(INTER_TTF).unwrap();
        assert_eq!(r.source(), FontSource::Embedded);
    }

    #[test]
    fn font_source_filesystem_tracking() {
        let r = FontRenderer::new_with_source(INTER_TTF, FontSource::Filesystem).unwrap();
        assert_eq!(r.source(), FontSource::Filesystem);
    }

    #[test]
    fn atlas_source_default_is_embedded() {
        let atlas = GlyphAtlas::new(INTER_TTF, 16).unwrap();
        assert_eq!(atlas.source(), FontSource::Embedded);
    }

    #[test]
    fn atlas_source_with_override() {
        let atlas = GlyphAtlas::new_with_source(INTER_TTF, 16, FontSource::Filesystem).unwrap();
        assert_eq!(atlas.source(), FontSource::Filesystem);
    }

    #[test]
    fn bitmap_font_data_has_correct_size() {
        assert_eq!(
            bitmap::VGA_FONT_8X16.len(),
            bitmap::BITMAP_FONT_GLYPH_COUNT * bitmap::BITMAP_FONT_BYTES_PER_GLYPH
        );
    }

    #[test]
    fn bitmap_glyph_a_is_nonzero() {
        let glyph = bitmap::glyph_bitmap(b'A');
        let nonzero = glyph.iter().any(|&b| b != 0);
        assert!(nonzero, "glyph 'A' should have non-zero pixels");
    }

    #[test]
    fn bitmap_render_produces_coverage() {
        let mut buf = [0u8; 128]; // 8 * 16
        let (w, h) = bitmap::render_bitmap_glyph(b'A', &mut buf);
        assert_eq!(w, 8);
        assert_eq!(h, 16);
        let has_pixels = buf.iter().any(|&b| b == 255);
        assert!(has_pixels, "rendered 'A' should have foreground pixels");
    }

    #[test]
    fn bitmap_space_is_blank() {
        let glyph = bitmap::glyph_bitmap(b' ');
        assert!(
            glyph.iter().all(|&b| b == 0),
            "space glyph should be all zeros"
        );
    }

    impl TestCanvas {
        fn clear(&mut self) {
            self.data.iter_mut().for_each(|b| *b = 0);
        }

        /// Coverage of white ink drawn over an opaque black background.
        fn ink(&self, x: u32, y: u32) -> u32 {
            self.read_pixel(x, y).red() as u32
        }

        fn row_has_ink(&self, y: u32) -> bool {
            (0..self.width).any(|x| self.ink(x, y) != 0)
        }

        /// Total ink per column.
        fn columns(&self) -> slopos_ostd::KVec<u32> {
            slopos_ostd::KVec::from_iter_fallible(
                (0..self.width).map(|x| (0..self.height).map(|y| self.ink(x, y)).sum()),
            )
            .expect("test alloc")
        }
    }

    /// Ink-weighted mean column, pixel centres at `x + 0.5`.
    fn centroid(columns: &[u32]) -> f32 {
        let total: u32 = columns.iter().sum();
        let moment: f32 = columns
            .iter()
            .enumerate()
            .map(|(x, &ink)| (x as f32 + 0.5) * ink as f32)
            .sum();
        moment / total as f32
    }

    #[test]
    fn every_glyph_standing_on_the_baseline_ends_exactly_on_it() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let mut standing = 0usize;
        for size in 11u16..=24 {
            let mut canvas = TestCanvas::new(96, 64);
            let (x, y) = (8, 8);
            let baseline = (y + renderer.ascent(size)) as u32;
            for slot in 0..GLYPH_COUNT {
                let cp = slot_codepoint(slot).unwrap();
                let Some(gid) = drawable_glyph(&renderer.font, cp) else {
                    continue;
                };
                let Some(outline) = renderer.font.glyph_outline(gid) else {
                    continue;
                };
                if outline.y_min != 0 || outline.contours.is_empty() {
                    continue;
                }
                standing += 1;
                let ch = char::from_u32(cp).unwrap();
                let mut text = [0u8; 4];
                canvas.clear();
                renderer.draw_text(
                    &mut canvas,
                    x,
                    y,
                    ch.encode_utf8(&mut text),
                    size,
                    Color32::WHITE,
                    Color32::BLACK,
                );
                assert!(
                    canvas.row_has_ink(baseline - 1),
                    "U+{cp:04X} at {size} px leaves the row above its baseline empty"
                );
                for row in baseline..canvas.height {
                    assert!(
                        !canvas.row_has_ink(row),
                        "U+{cp:04X} at {size} px inks row {} below its baseline",
                        row - baseline
                    );
                }
            }
        }
        assert!(standing > 14 * 100, "only {standing} glyphs checked");
    }

    #[test]
    fn a_runs_ink_stays_within_a_quarter_pixel_of_its_pen() {
        let mut renderer = FontRenderer::new(INTER_TTF).unwrap();
        let font = TtfFont::parse(INTER_TTF).unwrap();
        let origin = 8;
        for size in 11u16..=24 {
            let scale = size as f32 / font.units_per_em() as f32;
            let mut canvas = TestCanvas::new(560, 48);
            for text in [
                "l l l l l l l l l l l l l l",
                "s t a r t e d",
                "t h e v o l u m e",
                "A V W a v e s",
            ] {
                // The ideal pen, straight from the font's tables.
                let mut expected = slopos_ostd::KVec::new();
                let mut pen = origin as f32;
                let mut previous = None;
                for ch in text.chars() {
                    let gid = font.glyph_index(ch as u32).unwrap();
                    if let Some(left) = previous {
                        pen += font.kerning(left, gid) as f32 * scale;
                    }
                    if ch != ' ' {
                        canvas.clear();
                        let mut one = [0u8; 4];
                        renderer.draw_text(
                            &mut canvas,
                            origin,
                            8,
                            ch.encode_utf8(&mut one),
                            size,
                            Color32::WHITE,
                            Color32::BLACK,
                        );
                        let alone = centroid(canvas.columns().as_slice()) - origin as f32;
                        expected.push((ch, pen + alone)).unwrap();
                    }
                    pen += font.h_metrics(gid).unwrap().advance_width as f32 * scale;
                    previous = Some(gid);
                }

                canvas.clear();
                renderer.draw_text(
                    &mut canvas,
                    origin,
                    8,
                    text,
                    size,
                    Color32::WHITE,
                    Color32::BLACK,
                );
                let columns = canvas.columns();
                let mut blobs = slopos_ostd::KVec::new();
                let mut start = None;
                for (x, &ink) in columns.iter().enumerate().chain([(columns.len(), &0)]) {
                    match (start, ink != 0) {
                        (None, true) => start = Some(x),
                        (Some(from), false) => {
                            let mut blob = slopos_ostd::KVec::<u32>::zeroed(columns.len()).unwrap();
                            blob[from..x].copy_from_slice(&columns[from..x]);
                            blobs.push(centroid(blob.as_slice())).unwrap();
                            start = None;
                        }
                        _ => {}
                    }
                }
                assert_eq!(blobs.len(), expected.len(), "{text:?} at {size} px");
                for (&drawn, &(ch, ideal)) in blobs.iter().zip(expected.iter()) {
                    assert!(
                        (drawn - ideal).abs() <= 0.25,
                        "{ch:?} in {text:?} at {size} px is drawn at {drawn:.3}, its pen puts it at {ideal:.3}"
                    );
                }
            }
        }
    }

    #[test]
    fn kerned_pairs_measure_narrower_than_their_glyphs() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        for size in 11u16..=24 {
            for (pair, left, right) in [("To", "T", "o"), ("AV", "A", "V")] {
                let together = run_width(&renderer.font, pair, size);
                let apart =
                    run_width(&renderer.font, left, size) + run_width(&renderer.font, right, size);
                assert!(
                    together < apart,
                    "{pair:?} at {size} px is {together} wide, its glyphs {apart}"
                );
            }
        }
        assert!(
            renderer.text_width("To", 24)
                < renderer.text_width("T", 24) + renderer.text_width("o", 24)
        );
    }

    #[test]
    fn fitting_and_measuring_agree_on_a_kerned_run() {
        let renderer = FontRenderer::new(INTER_TTF).unwrap();
        let text = "AVATAR Today";
        for size in 11u16..=24 {
            let width = renderer.text_width(text, size);
            assert_eq!(renderer.prefix_fitting(text, size, width), text.len());
            let short = renderer.prefix_fitting(text, size, width - 1);
            assert!(short < text.len());
            assert!(renderer.text_width(&text[..short], size) <= width - 1);
        }
    }
}
