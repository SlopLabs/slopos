//! Pre-rasterized fixed-width glyph atlas for fast terminal/console rendering.

use slopos_ostd::KVec;

use slopos_abi::damage::DamageRect;
use slopos_abi::draw::{Canvas, Color32};

use crate::{FontRenderer, FontSource};

use crate::{ASCII_FIRST, ASCII_LAST, GLYPH_COUNT, boxdraw, glyph_slot, slot_codepoint};

/// Bytes one coverage chunk aims for: the whole set is past the 1 MiB a single
/// kernel allocation may be once the cell reaches 32x32.
pub const CHUNK_TARGET_BYTES: usize = 256 * 1024;

/// Pre-rasterized fixed-width glyph atlas: every glyph-set codepoint (see
/// [`crate::glyph_slot`]) gets a uniform cell, one coverage byte per pixel.
pub struct GlyphAtlas {
    cell_w: u16,
    cell_h: u16,
    slots_per_chunk: usize,
    /// [`GLYPH_COUNT`] cells of `cell_w × cell_h` bytes, in chunks of
    /// `slots_per_chunk` cells.
    chunks: KVec<KVec<u8>>,
    /// Rendered for codepoints outside the glyph set, and for set codepoints
    /// the font itself has no glyph for.
    replacement: KVec<u8>,
    source: FontSource,
}

/// Writes the synthesized notdef — a filled diamond — over the whole cell.
fn fill_replacement(cell: &mut [u8], cell_w: usize, cell_h: usize) {
    let mx = cell_w / 2;
    let my = cell_h / 2;
    let rx = (cell_w / 3).max(2);
    let ry = (cell_h / 3).max(2);
    for y in 0..cell_h {
        for x in 0..cell_w {
            let dx = if x >= mx { x - mx } else { mx - x };
            let dy = if y >= my { y - my } else { my - y };
            cell[y * cell_w + x] = if dx * ry + dy * rx <= rx * ry { 200 } else { 0 };
        }
    }
}

/// Builds a [`GlyphAtlas`] one chunk at a time, so no caller — least of all
/// the `font_set` syscall — materialises the whole coverage set at once.
pub struct AtlasBuilder {
    cell_w: u16,
    cell_h: u16,
    slots_per_chunk: usize,
    chunks: KVec<KVec<u8>>,
    replacement: KVec<u8>,
}

impl AtlasBuilder {
    pub fn new(cell_w: u16, cell_h: u16) -> Option<Self> {
        if cell_w == 0 || cell_h == 0 {
            return None;
        }
        let stride = (cell_w as usize).checked_mul(cell_h as usize)?;
        let slots_per_chunk = (CHUNK_TARGET_BYTES / stride).max(1);
        let mut chunks = KVec::with_capacity(GLYPH_COUNT.div_ceil(slots_per_chunk)).ok()?;
        let mut placed = 0usize;
        while placed < GLYPH_COUNT {
            let slots = slots_per_chunk.min(GLYPH_COUNT - placed);
            let chunk = KVec::<u8>::zeroed(slots.checked_mul(stride)?).ok()?;
            chunks.push(chunk).ok()?;
            placed += slots;
        }
        let replacement = KVec::<u8>::zeroed(stride).ok()?;
        Some(Self {
            cell_w,
            cell_h,
            slots_per_chunk,
            chunks,
            replacement,
        })
    }

    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    pub fn chunk_mut(&mut self, index: usize) -> Option<&mut [u8]> {
        self.chunks
            .as_mut_slice()
            .get_mut(index)
            .map(|chunk| chunk.as_mut_slice())
    }

    pub fn replacement_mut(&mut self) -> &mut [u8] {
        self.replacement.as_mut_slice()
    }

    /// One glyph slot's cell, `cell_w × cell_h` bytes.
    pub fn slot_mut(&mut self, slot: usize) -> Option<&mut [u8]> {
        let stride = self.cell_w as usize * self.cell_h as usize;
        let at = (slot % self.slots_per_chunk) * stride;
        let chunk = self
            .chunks
            .as_mut_slice()
            .get_mut(slot / self.slots_per_chunk)?;
        chunk.as_mut_slice().get_mut(at..at + stride)
    }

    pub fn finish(self, source: FontSource) -> GlyphAtlas {
        GlyphAtlas {
            cell_w: self.cell_w,
            cell_h: self.cell_h,
            slots_per_chunk: self.slots_per_chunk,
            chunks: self.chunks,
            replacement: self.replacement,
            source,
        }
    }
}

impl GlyphAtlas {
    /// Create a new atlas by pre-rasterizing the whole glyph set at `size_px`.
    pub fn new(font_data: &[u8], size_px: u16) -> Option<Self> {
        let renderer = FontRenderer::new(font_data)?;
        let upem = renderer.font.units_per_em() as f32;
        if upem == 0.0 {
            return None;
        }
        let scale = size_px as f32 / upem;
        let hhea = renderer.font.hhea();
        let ascender = libm::ceilf(hhea.ascender as f32 * scale) as i32;
        let descender = libm::floorf(hhea.descender as f32 * scale) as i32;
        let line_gap = libm::roundf(hhea.line_gap as f32 * scale) as i32;
        let cell_h = (ascender - descender + line_gap / 2) as u16;

        // Deliberately ASCII-only: extending the glyph set must not move the
        // cell geometry the terminal grid is sized from; wider glyphs are clipped.
        let mut max_advance: u16 = 0;
        for cp in ASCII_FIRST..=ASCII_LAST {
            if let Some(gid) = renderer.font.glyph_index(cp) {
                if let Some(hm) = renderer.font.h_metrics(gid) {
                    let adv = libm::ceilf(hm.advance_width as f32 * scale) as u16;
                    if adv > max_advance {
                        max_advance = adv;
                    }
                }
            }
        }
        if max_advance == 0 || cell_h == 0 {
            return None;
        }
        let cell_w = max_advance;

        let cw = cell_w as usize;
        let ch = cell_h as usize;
        let mut builder = AtlasBuilder::new(cell_w, cell_h)?;
        fill_replacement(builder.replacement_mut(), cw, ch);

        for idx in 0..GLYPH_COUNT {
            let Some(cp) = slot_codepoint(idx) else {
                continue;
            };
            let cell = builder.slot_mut(idx)?;

            if boxdraw::draw(cp, cell, cw, ch) {
                continue;
            }
            // Keyed on the cmap, not on empty coverage: a space legitimately
            // rasterizes to nothing. An uncovered codepoint answers glyph 0.
            if !matches!(renderer.font.glyph_index(cp), Some(gid) if gid != 0) {
                fill_replacement(cell, cw, ch);
                continue;
            }

            if let Some(rg) = renderer.rasterize_glyph(cp, size_px, scale, ascender) {
                let glyph_advance = rg.advance as i32;
                let x_center = (cell_w as i32 - glyph_advance) / 2;
                let gx_start = x_center + rg.bearing_x as i32;
                let gy_start = ascender - rg.bearing_y as i32;

                for gy in 0..rg.height as usize {
                    for gx in 0..rg.width as usize {
                        let dx = gx_start + gx as i32;
                        let dy = gy_start + gy as i32;
                        if dx >= 0 && (dx as usize) < cw && dy >= 0 && (dy as usize) < ch {
                            let src = gy * rg.width as usize + gx;
                            let dst = dy as usize * cw + dx as usize;
                            if src < rg.coverage.len() {
                                cell[dst] = rg.coverage[src];
                            }
                        }
                    }
                }
            }
        }

        Some(builder.finish(FontSource::Embedded))
    }

    pub fn new_with_source(font_data: &[u8], size_px: u16, source: FontSource) -> Option<Self> {
        let mut atlas = Self::new(font_data, size_px)?;
        atlas.source = source;
        Some(atlas)
    }

    pub fn source(&self) -> FontSource {
        self.source
    }

    #[inline]
    pub fn cell_width(&self) -> i32 {
        self.cell_w as i32
    }

    #[inline]
    pub fn cell_height(&self) -> i32 {
        self.cell_h as i32
    }

    /// Coverage bytes for the whole glyph set: what the chunks concatenated
    /// add up to, and the coverage half of the `font_set` wire format.
    #[inline]
    pub fn coverage_len(&self) -> usize {
        GLYPH_COUNT * self.cell_w as usize * self.cell_h as usize
    }

    /// The coverage pieces in slot order.
    pub fn coverage_chunks(&self) -> impl Iterator<Item = &[u8]> + '_ {
        self.chunks.as_slice().iter().map(|chunk| chunk.as_slice())
    }

    #[inline]
    pub fn replacement(&self) -> &[u8] {
        self.replacement.as_slice()
    }

    /// Coverage for a codepoint (cell_w × cell_h bytes); the replacement glyph
    /// when the codepoint is outside the glyph set.
    #[inline]
    pub fn get_coverage(&self, codepoint: u32) -> &[u8] {
        let Some(idx) = glyph_slot(codepoint) else {
            return self.replacement.as_slice();
        };
        let stride = self.cell_w as usize * self.cell_h as usize;
        let at = (idx % self.slots_per_chunk) * stride;
        &self.chunks.as_slice()[idx / self.slots_per_chunk].as_slice()[at..at + stride]
    }

    /// Draw a single character at (x, y). Never reads back from the target, so
    /// it is safe over MMIO. A transparent `bg` (`bg.0 == 0`) leaves uncovered
    /// pixels untouched and blends edge pixels against opaque black.
    pub fn draw_char<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        cp: u32,
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        let coverage = self.get_coverage(cp);
        let has_bg = bg.0 != 0;
        let fmt = target.pixel_format();
        let fg_px = fmt.encode(fg);
        let bg_px = fmt.encode(bg);
        // Opaque black, not transparent black: blending edges against alpha=0
        // leaves a dark fringe.
        let blend_bg = if has_bg { bg } else { Color32::BLACK };

        let buf_w = target.width() as i32;
        let buf_h = target.height() as i32;

        for row in 0..ch {
            let py = y + row;
            if py < 0 || py >= buf_h {
                continue;
            }
            for col in 0..cw {
                let px = x + col;
                if px < 0 || px >= buf_w {
                    continue;
                }
                let cov = coverage[(row * cw + col) as usize];
                if cov == 0 {
                    if has_bg {
                        target.put_pixel(px, py, bg_px);
                    }
                } else if cov == 255 {
                    target.put_pixel(px, py, fg_px);
                } else {
                    let blended = blend_color32(cov, fg, blend_bg);
                    target.put_pixel(px, py, fmt.encode(blended));
                }
            }
        }

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + cw - 1).min(buf_w - 1);
        let y1 = (y + ch - 1).min(buf_h - 1);
        if x0 <= x1 && y0 <= y1 {
            let d = DamageRect { x0, y0, x1, y1 };
            target.report_damage(d);
            Some(d)
        } else {
            None
        }
    }

    /// Draw a null-terminated byte string.
    pub fn draw_bytes<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &[u8],
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        let w = target.width() as i32;
        let h = target.height() as i32;
        let mut cx = x;
        let mut cy = y;
        let mut damage: Option<DamageRect> = None;

        for &byte in text {
            match byte {
                0 => break,
                b'\n' => {
                    cx = x;
                    cy += ch;
                }
                b'\r' => cx = x,
                b'\t' => {
                    let tab = 4 * cw;
                    cx = ((cx - x + tab) / tab) * tab + x;
                }
                _ => {
                    if let Some(d) = self.draw_char(target, cx, cy, byte as u32, fg, bg) {
                        damage = Some(match damage {
                            Some(prev) => prev.union(&d),
                            None => d,
                        });
                    }
                    cx += cw;
                    if cx + cw > w {
                        cx = x;
                        cy += ch;
                    }
                }
            }
            if cy >= h {
                break;
            }
        }
        if let Some(d) = damage {
            target.report_damage(d);
        }
        damage
    }

    /// Draw a UTF-8 string; a multi-byte character occupies one glyph cell.
    pub fn draw_str<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        fg: Color32,
        bg: Color32,
    ) -> Option<DamageRect> {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        let w = target.width() as i32;
        let h = target.height() as i32;
        let mut cx = x;
        let mut cy = y;
        let mut damage: Option<DamageRect> = None;

        for c in text.chars() {
            match c {
                '\0' => break,
                '\n' => {
                    cx = x;
                    cy += ch;
                }
                '\r' => cx = x,
                '\t' => {
                    let tab = 4 * cw;
                    cx = ((cx - x + tab) / tab) * tab + x;
                }
                _ => {
                    if let Some(d) = self.draw_char(target, cx, cy, c as u32, fg, bg) {
                        damage = Some(match damage {
                            Some(prev) => prev.union(&d),
                            None => d,
                        });
                    }
                    cx += cw;
                    if cx + cw > w {
                        cx = x;
                        cy += ch;
                    }
                }
            }
            if cy >= h {
                break;
            }
        }
        if let Some(d) = damage {
            target.report_damage(d);
        }
        damage
    }

    pub fn draw_char_clipped<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        cp: u32,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        if x > clip.x1 || y > clip.y1 || x + cw - 1 < clip.x0 || y + ch - 1 < clip.y0 {
            return;
        }

        let coverage = self.get_coverage(cp);
        let has_bg = bg.0 != 0;
        let fmt = target.pixel_format();
        let fg_px = fmt.encode(fg);
        let bg_px = fmt.encode(bg);
        let blend_bg = if has_bg { bg } else { Color32::BLACK };

        for row in 0..ch {
            let py = y + row;
            if py < clip.y0 || py > clip.y1 {
                continue;
            }
            for col in 0..cw {
                let px = x + col;
                if px < clip.x0 || px > clip.x1 {
                    continue;
                }
                let cov = coverage[(row * cw + col) as usize];
                if cov == 0 {
                    if has_bg {
                        target.put_pixel(px, py, bg_px);
                    }
                } else if cov == 255 {
                    target.put_pixel(px, py, fg_px);
                } else {
                    let blended = blend_color32(cov, fg, blend_bg);
                    target.put_pixel(px, py, fmt.encode(blended));
                }
            }
        }
    }

    pub fn draw_str_clipped<T: Canvas>(
        &self,
        target: &mut T,
        x: i32,
        y: i32,
        text: &str,
        fg: Color32,
        bg: Color32,
        clip: &DamageRect,
    ) {
        let cw = self.cell_w as i32;
        let ch = self.cell_h as i32;
        if y + ch - 1 < clip.y0 || y > clip.y1 {
            return;
        }
        let mut cx = x;
        for c in text.chars() {
            if c == '\0' {
                break;
            }
            if cx > clip.x1 {
                break;
            }
            if cx + cw - 1 >= clip.x0 {
                self.draw_char_clipped(target, cx, y, c as u32, fg, bg, clip);
            }
            cx += cw;
        }
    }

    /// Measure width of a null-terminated byte string.
    pub fn bytes_width(&self, text: &[u8]) -> i32 {
        let cw = self.cell_w as i32;
        let mut width = 0i32;
        for &ch in text {
            match ch {
                0 | b'\n' => break,
                b'\t' => {
                    let tab = 4 * cw;
                    width = ((width + tab - 1) / tab) * tab;
                }
                _ => width += cw,
            }
        }
        width
    }

    /// Measure width of a UTF-8 string (character-decoded).
    pub fn str_width(&self, text: &str) -> i32 {
        let cw = self.cell_w as i32;
        let mut width = 0i32;
        for c in text.chars() {
            match c {
                '\0' | '\n' => break,
                '\t' => {
                    let tab = 4 * cw;
                    width = ((width + tab - 1) / tab) * tab;
                }
                _ => width += cw,
            }
        }
        width
    }

    /// Count lines in a null-terminated byte string.
    pub fn bytes_lines(&self, text: &[u8]) -> i32 {
        let mut lines = 1i32;
        for &ch in text {
            if ch == 0 {
                break;
            }
            if ch == b'\n' {
                lines += 1;
            }
        }
        lines
    }
}

/// `(num + 128) / 255`, without a divide.
///
/// Exact for `num <= 255 * 255`, the whole range a channel blend can produce
/// (`a + inv == 255`, both components `u8`). The kernel builds at opt-level 0,
/// where each `/ 255` is a real `divl` on the per-pixel path.
#[inline]
fn blend_div255(num: u32) -> u32 {
    let x = num + 128;
    (x + 1 + (x >> 8)) >> 8
}

/// Blend fg and bg Color32 values by coverage (0-255).
#[inline]
pub fn blend_color32(cov: u8, fg: Color32, bg: Color32) -> Color32 {
    let a = cov as u32;
    let inv = 255 - a;
    let r = blend_div255(fg.red() as u32 * a + bg.red() as u32 * inv);
    let g = blend_div255(fg.green() as u32 * a + bg.green() as u32 * inv);
    let b = blend_div255(fg.blue() as u32 * a + bg.blue() as u32 * inv);
    let al = blend_div255(fg.alpha() as u32 * a + bg.alpha() as u32 * inv);
    Color32::new(r as u8, g as u8, b as u8, al as u8)
}

/// Blend fg and bg raw `0x00RRGGBB` values by coverage (0-255).
#[inline]
pub fn blend_coverage_u32(cov: u8, fg: u32, bg: u32) -> u32 {
    if cov == 255 {
        return fg;
    }
    if cov == 0 {
        return bg;
    }
    let a = cov as u32;
    let inv = 255 - a;
    let r = blend_div255(((fg >> 16) & 0xFF) * a + ((bg >> 16) & 0xFF) * inv);
    let g = blend_div255(((fg >> 8) & 0xFF) * a + ((bg >> 8) & 0xFF) * inv);
    let b = blend_div255((fg & 0xFF) * a + (bg & 0xFF) * inv);
    (r << 16) | (g << 8) | b
}

#[cfg(feature = "kernel")]
mod global_atlas {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};
    use slopos_ostd::KBox;
    use slopos_ostd::sync::{RcuCell, RcuCellGuard};

    pub type AtlasGuard = RcuCellGuard<GlyphAtlas>;

    static GLOBAL_ATLAS: RcuCell<GlyphAtlas> = RcuCell::empty();

    /// Bumped by every `replace_global`; compared instead of pointer identity,
    /// which a recycled heap address would make ABA-unsafe.
    static ATLAS_GENERATION: AtomicU64 = AtomicU64::new(0);

    static FONT_CHANGE_CALLBACK: slopos_ostd::sync::SpinLock<Option<fn()>> =
        slopos_ostd::sync::SpinLock::new(
            None,
            slopos_ostd::lock_class!(
                "FONT_CHANGE_CALLBACK",
                slopos_ostd::sync::LOCK_LEVEL_RESOURCE
            ),
        );

    pub fn register_font_change_callback(cb: fn()) {
        *FONT_CHANGE_CALLBACK.lock() = Some(cb);
    }

    pub fn invoke_font_change_callback() {
        let cb = *FONT_CHANGE_CALLBACK.lock();
        if let Some(f) = cb {
            f();
        }
    }

    /// Snapshot and re-check to detect a replacement across a lock drop.
    #[inline]
    pub fn atlas_generation() -> u64 {
        ATLAS_GENERATION.load(Ordering::Acquire)
    }

    pub fn init_global(font_data: &[u8], size_px: u16) -> bool {
        if let Some(atlas) = GlyphAtlas::new(font_data, size_px) {
            replace_global(atlas);
            true
        } else {
            false
        }
    }

    pub fn init_global_bitmap() -> bool {
        use crate::bitmap;
        match bitmap::bitmap_to_coverage(
            &bitmap::VGA_FONT_8X16,
            bitmap::BITMAP_FONT_WIDTH,
            bitmap::BITMAP_FONT_HEIGHT,
            bitmap::BITMAP_FONT_GLYPH_COUNT,
        ) {
            Some(builder) => replace_global(builder.finish(FontSource::BitmapFallback)),
            None => false,
        }
    }

    /// Atomically replace the global atlas; `false` if allocating the new box
    /// failed.
    pub fn replace_global(new_atlas: GlyphAtlas) -> bool {
        let new_box = match KBox::try_new(new_atlas) {
            Ok(b) => b,
            Err(_) => return false,
        };
        let _ = GLOBAL_ATLAS.replace(new_box);
        ATLAS_GENERATION.fetch_add(1, Ordering::Release);
        true
    }

    /// The RCU read-side critical section lasts exactly as long as the
    /// returned guard, so drop it promptly.
    pub fn global() -> Option<AtlasGuard> {
        GLOBAL_ATLAS.load()
    }
}

#[cfg(feature = "kernel")]
pub use global_atlas::*;

#[cfg(test)]
mod tests {
    use super::GlyphAtlas;
    use crate::FontSource;

    #[test]
    fn blend_div255_matches_the_divide_it_replaces() {
        // Every numerator a channel blend can produce, exhaustively.
        for num in 0..=(255u32 * 255 + 128) {
            assert_eq!(
                super::blend_div255(num),
                (num + 128) / 255,
                "div255_round diverged at {num}"
            );
        }
    }

    #[test]
    fn blend_coverage_u32_is_exact_at_the_endpoints() {
        assert_eq!(
            super::blend_coverage_u32(0, 0x00FF_FFFF, 0x0012_3456),
            0x0012_3456
        );
        assert_eq!(
            super::blend_coverage_u32(255, 0x00FF_FFFF, 0x0012_3456),
            0x00FF_FFFF
        );
        assert_eq!(super::blend_coverage_u32(128, 0x0000_0000, 0x0000_0000), 0);
        assert_eq!(
            super::blend_coverage_u32(128, 0x00FF_FFFF, 0x00FF_FFFF),
            0x00FF_FFFF
        );
    }

    #[test]
    fn builder_refuses_an_out_of_range_chunk() {
        let mut builder = super::AtlasBuilder::new(8, 16).expect("builder must build");
        let count = builder.chunk_count();
        assert!(count >= 1);
        assert!(builder.chunk_mut(count - 1).is_some());
        assert!(builder.chunk_mut(count).is_none());
    }

    /// The last chunk is the short one, so an off-by-one in either the chunk
    /// index or the within-chunk offset shows up here and nowhere else.
    #[test]
    fn a_slot_written_through_the_last_chunk_reads_back() {
        let cell_w = 32u16;
        let cell_h = 32u16;
        let stride = cell_w as usize * cell_h as usize;
        let mut builder = super::AtlasBuilder::new(cell_w, cell_h).expect("builder must build");
        let last = builder.chunk_count() - 1;
        let chunk = builder.chunk_mut(last).expect("last chunk");
        let cells_in_last = chunk.len() / stride;
        chunk[(cells_in_last - 1) * stride] = 0xA5;
        builder.replacement_mut()[0] = 0x5A;

        let atlas = builder.finish(FontSource::Syscall);
        assert_eq!(atlas.source(), FontSource::Syscall);
        assert_eq!(atlas.cell_width(), 32);
        assert_eq!(atlas.cell_height(), 32);

        let last_cp = crate::slot_codepoint(crate::GLYPH_COUNT - 1).expect("last slot");
        assert_eq!(atlas.get_coverage(last_cp)[0], 0xA5);
        assert_eq!(atlas.get_coverage(0x1F)[0], 0x5A);
    }

    /// A cell large enough that the set needs several chunks, so the two slots
    /// either side of a chunk edge exercise the chunk arithmetic.
    #[test]
    fn slots_across_a_chunk_boundary_read_back() {
        let cell_w = 32u16;
        let cell_h = 32u16;
        let stride = cell_w as usize * cell_h as usize;
        let slots_per_chunk = super::CHUNK_TARGET_BYTES / stride;
        let mut builder = super::AtlasBuilder::new(cell_w, cell_h).expect("builder must build");
        assert!(
            builder.chunk_count() > 1,
            "a {cell_w}x{cell_h} cell must need more than one chunk"
        );

        builder
            .slot_mut(slots_per_chunk - 1)
            .expect("last slot of the first chunk")
            .fill(0x11);
        builder
            .slot_mut(slots_per_chunk)
            .expect("first slot of the second chunk")
            .fill(0x22);

        let atlas = builder.finish(FontSource::Syscall);
        let before = crate::slot_codepoint(slots_per_chunk - 1).expect("slot exists");
        let after = crate::slot_codepoint(slots_per_chunk).expect("slot exists");
        assert!(atlas.get_coverage(before).iter().all(|&b| b == 0x11));
        assert!(atlas.get_coverage(after).iter().all(|&b| b == 0x22));
    }

    #[test]
    fn no_chunk_exceeds_the_target_size() {
        for (cell_w, cell_h) in [(8u16, 16u16), (32, 32), (11, 23)] {
            let atlas = super::AtlasBuilder::new(cell_w, cell_h)
                .expect("builder must build")
                .finish(FontSource::Syscall);
            for chunk in atlas.coverage_chunks() {
                assert!(
                    chunk.len() <= super::CHUNK_TARGET_BYTES,
                    "{cell_w}x{cell_h} produced a {}-byte chunk",
                    chunk.len()
                );
            }
        }
    }

    #[test]
    fn coverage_chunks_cover_exactly_the_coverage_length() {
        let atlas = super::AtlasBuilder::new(8, 16)
            .expect("builder must build")
            .finish(FontSource::Syscall);
        let total: usize = atlas.coverage_chunks().map(|chunk| chunk.len()).sum();
        assert_eq!(total, atlas.coverage_len());
        assert_eq!(atlas.coverage_len(), crate::GLYPH_COUNT * 8 * 16);
    }

    #[cfg(feature = "kernel")]
    #[test]
    fn init_global_bitmap_succeeds() {
        use super::global_atlas::{global, init_global_bitmap};
        assert!(init_global_bitmap());
        let atlas = global().expect("atlas must be set");
        assert_eq!(atlas.cell_width(), 8);
        assert_eq!(atlas.cell_height(), 16);
    }

    #[test]
    fn glyph_slot_mapping_round_trips() {
        use crate::{GLYPH_COUNT, GLYPH_RANGES, glyph_slot, slot_codepoint};
        for slot in 0..GLYPH_COUNT {
            let cp = slot_codepoint(slot).expect("every slot names a codepoint");
            assert_eq!(glyph_slot(cp), Some(slot));
        }
        assert_eq!(slot_codepoint(GLYPH_COUNT), None);

        for (lo, hi) in GLYPH_RANGES {
            assert!(glyph_slot(lo).is_some(), "U+{lo:04X} must be in the set");
            assert!(glyph_slot(hi).is_some(), "U+{hi:04X} must be in the set");
        }

        for gap in [
            0x1F, 0x7F, 0x9F, 0x180, 0x2C5, 0x2DE, 0x36F, 0x500, 0x200F, 0x203F, 0x209F, 0x20C0,
            0x218F, 0x2200, 0x24FF, 0x2600, 0x4E2D,
        ] {
            assert_eq!(glyph_slot(gap), None, "U+{gap:04X} must be outside the set");
        }
        // Box Drawing runs straight into Block Elements: no gap there.
        assert!(glyph_slot(0x257F).is_some());
        assert!(glyph_slot(0x259F).is_some());

        // The keymap-relevant glyphs are all in the set.
        for c in "äöüÄÖÜéèà§°ç£¦¬¢´¨€".chars() {
            assert!(glyph_slot(c as u32).is_some(), "missing {c}");
        }
    }

    const INTER_TTF: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../assets/fonts/Inter-Regular.ttf"
    ));

    #[test]
    fn atlas_rasterizes_latin1_glyphs() {
        let atlas = GlyphAtlas::new(INTER_TTF, 16).expect("atlas must build");
        for c in ['ä', 'é', 'à', '€'] {
            let cov = atlas.get_coverage(c as u32);
            assert!(
                cov.iter().any(|&b| b != 0),
                "{c} should have nonzero coverage"
            );
            assert_ne!(
                cov,
                atlas.get_coverage(0x4E2D),
                "{c} must not be the replacement"
            );
        }
    }

    /// Inter's cmap covers neither U+0149 nor U+0370, yet the set holds a slot
    /// for both: they must read back as the notdef, not as a blank cell.
    #[test]
    fn a_set_codepoint_the_font_lacks_reads_back_as_the_notdef() {
        let atlas = GlyphAtlas::new(INTER_TTF, 16).expect("atlas must build");
        let notdef = atlas.get_coverage(0x4E2D);
        assert!(notdef.iter().any(|&b| b != 0));
        for cp in [0x0149u32, 0x0370] {
            assert_eq!(
                atlas.get_coverage(cp),
                notdef,
                "U+{cp:04X} must read back as the notdef"
            );
        }
        assert!(
            atlas.get_coverage(0x20).iter().all(|&b| b == 0),
            "a space has an empty outline on purpose and must stay blank"
        );
    }

    /// Inter maps every box-drawing codepoint to `.notdef`, so coverage there
    /// can only be procedural.
    #[test]
    fn box_drawing_comes_from_the_procedural_path() {
        assert!(crate::glyph_slot(0x2500).is_some());
        let atlas = GlyphAtlas::new(INTER_TTF, 16).expect("atlas must build");
        let cov = atlas.get_coverage(0x2500);
        assert!(cov.iter().any(|&b| b != 0), "U+2500 must have coverage");
        assert_ne!(
            cov,
            atlas.get_coverage(0x4E2D),
            "U+2500 must be drawn, not replaced"
        );
    }
}
