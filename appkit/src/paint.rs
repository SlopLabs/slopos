use slopos_abi::draw::{Canvas, Color32};
use slopos_gfx::DrawBuffer;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};

use slopos_theme_core::{Icon, TextRole, Theme, radius};

use super::audit::{Mark, TextRun};
use super::constraints::Rect;
use super::style::StyleSheet;
use super::text::BODY;
use super::traits::WidgetId;

/// Paint target handed to each widget; the framework sets the clip rect before
/// calling `paint()`, and widgets may only paint within it.
pub struct PaintContext<'a, 'b> {
    pub buffer: &'a mut DrawBuffer<'b>,
    /// Window coordinates.
    pub clip: Rect,
    /// The clip less every viewport's: an edge that cuts text short.
    fixed: Rect,
    viewport: Option<Rect>,
    layer: WidgetId,
    /// Accumulated over all ancestor ScrollViews.
    pub scroll_offset_x: i32,
    pub scroll_offset_y: i32,
    pub focus_visible: bool,
    /// Where the pointer is, in window coordinates.
    ///
    /// Hover is derived from this at paint time rather than remembered: the
    /// widget tree is rebuilt on every message, so a stored hover flag is gone
    /// by the next frame — which makes a widget draw one thing and hit-test
    /// another.
    pub pointer: (i32, i32),
    pub style: &'a StyleSheet,
    /// Every line of text drawn, for [`crate::audit`].
    runs: Option<&'a mut Vec<TextRun>>,
}

impl<'a, 'b> PaintContext<'a, 'b> {
    pub fn new(buffer: &'a mut DrawBuffer<'b>, style: &'a StyleSheet) -> Self {
        let w = buffer.width() as i32;
        let h = buffer.height() as i32;
        Self {
            buffer,
            clip: Rect::new(0, 0, w, h),
            fixed: Rect::new(0, 0, w, h),
            viewport: None,
            layer: WidgetId::NONE,
            scroll_offset_x: 0,
            scroll_offset_y: 0,
            focus_visible: false,
            pointer: (-1, -1),
            style,
            runs: None,
        }
    }

    /// A context that also notes every line of text drawn into `runs`.
    pub fn recording(
        buffer: &'a mut DrawBuffer<'b>,
        style: &'a StyleSheet,
        runs: &'a mut Vec<TextRun>,
    ) -> Self {
        let mut ctx = Self::new(buffer, style);
        ctx.runs = Some(runs);
        ctx
    }

    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        let rect = Rect::new(x, y, w, h);
        if let Some(clipped) = rect.intersect(&self.clip) {
            let dr = clipped.to_damage_rect();
            slopos_gfx::canvas_ops::fill_rect_clipped(self.buffer, x, y, w, h, color, &dr);
        }
    }

    pub fn fill_rect_blended(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        let rect = Rect::new(x, y, w, h);
        if let Some(clipped) = rect.intersect(&self.clip) {
            let dr = clipped.to_damage_rect();
            slopos_gfx::blend::fill_rect_blended_clipped(self.buffer, x, y, w, h, color, &dr);
        }
    }

    /// Draw a 1px outline rectangle.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        self.fill_rect(x, y, w, 1, color);
        self.fill_rect(x, y + h - 1, w, 1, color);
        self.fill_rect(x, y + 1, 1, h - 2, color);
        self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
    }

    pub fn fill_rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        color: Color32,
    ) {
        let clip = self.clip.to_damage_rect();
        self.buffer.with_scissor(clip, |b| {
            slopos_gfx::canvas_ops::rounded_rect_filled(b, x, y, w, h, radius, color);
        });
    }

    pub fn draw_rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        radius: i32,
        color: Color32,
    ) {
        let clip = self.clip.to_damage_rect();
        self.buffer.with_scissor(clip, |b| {
            slopos_gfx::canvas_ops::rounded_rect(b, x, y, w, h, radius, color);
        });
    }

    /// `wash`, translucent, laid over what `rect` holds, with rounded corners.
    pub fn fill_rounded_wash(&mut self, rect: Rect, radius: i32, wash: Color32) {
        let under = self.sample_under(rect);
        let fill = slopos_theme_core::contrast::over(wash, under);
        self.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, radius, fill);
    }

    /// Body text with its line box's top-left at `(x, y)`.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
        self.text_line(x, y, text, TextRole::Body, fg, bg, false);
    }

    pub fn draw_text_transparent(&mut self, x: i32, y: i32, text: &str, fg: Color32) {
        self.draw_text(x, y, text, fg, Color32::TRANSPARENT);
    }

    /// Text in `role`, composited over what is there.
    pub fn draw_text_in(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32) {
        self.text_line(x, y, text, role, fg, Color32::TRANSPARENT, false);
    }

    /// Text in `role` cut short to `max_w` with an ellipsis; answers whether
    /// it was cut.
    pub fn draw_text_fitted(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        max_w: i32,
        fg: Color32,
    ) -> bool {
        match crate::text::ui::role_elide_end(text, role, max_w) {
            (kept, false) if kept.len() == text.len() => {
                self.text_line(x, y, text, role, fg, Color32::TRANSPARENT, false);
                false
            }
            (kept, true) => {
                let after = x + crate::text::ui::role_width(kept, role);
                self.text_line(x, y, kept, role, fg, Color32::TRANSPARENT, true);
                let ellipsis = crate::text::ui::ELLIPSIS;
                self.text_line(after, y, ellipsis, role, fg, Color32::TRANSPARENT, true);
                true
            }
            _ => {
                self.note_line(x, y, text, role, fg, Mark::Dropped);
                true
            }
        }
    }

    /// Fixed-cell text, for content whose columns must line up (code, hex,
    /// tabular numbers).
    pub fn draw_mono_text(&mut self, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
        if self.runs.is_some() {
            let (width, height) = (crate::text::string_width(text), crate::text::cell_height());
            let under = self.sample_under(Rect::new(x, y, width, height));
            self.note(TextRun {
                x,
                top: y,
                baseline: y + height * 4 / 5,
                width,
                height,
                text: String::from(text),
                fg,
                bg: under,
                clip: self.clip,
                fixed: self.fixed,
                viewport: self.viewport,
                layer: self.layer,
                mark: Mark::Whole,
            });
        }
        let dr = self.clip.to_damage_rect();
        crate::text::draw_str_clipped(self.buffer, x, y, text, fg, bg, &dr);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_line(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        fg: Color32,
        bg: Color32,
        elided: bool,
    ) {
        let mark = if elided { Mark::Elided } else { Mark::Whole };
        self.note_line(x, y, text, role, fg, mark);
        let dr = self.clip.to_damage_rect();
        crate::text::ui::draw_role(self.buffer, x, y, text, role, fg, bg, &dr);
    }

    /// Notes a line for the audit, with the colour most of its box holds.
    fn note_line(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32, mark: Mark) {
        if self.runs.is_none() || text.trim().is_empty() {
            return;
        }
        let height = crate::text::ui::role_line_height(role);
        let width = crate::text::ui::role_width(text, role);
        let under = self.sample_under(Rect::new(x, y, width, height));
        self.note(TextRun {
            x,
            top: y,
            baseline: y + crate::text::ui::role_ascent(role),
            width,
            height,
            text: String::from(text),
            fg,
            bg: under,
            clip: self.clip,
            fixed: self.fixed,
            viewport: self.viewport,
            layer: self.layer,
            mark,
        });
    }

    fn note(&mut self, run: TextRun) {
        if let Some(runs) = self.runs.as_deref_mut() {
            runs.push(run);
        }
    }

    /// The colour most of `rect`'s visible pixels hold.
    fn sample_under(&self, rect: Rect) -> Color32 {
        let Some(rect) = rect.intersect(&self.clip) else {
            return Color32::TRANSPARENT;
        };
        let format = self.buffer.pixel_format();
        let mut seen: Vec<(u32, u32)> = Vec::new();
        for row in 0..4 {
            for col in 0..8 {
                let px = rect.x + col * rect.width / 8;
                let py = rect.y + row * rect.height / 4;
                let raw = self.buffer.read_pixel(px, py);
                match seen.iter_mut().find(|(c, _)| *c == raw) {
                    Some((_, n)) => *n += 1,
                    None => seen.push((raw, 1)),
                }
            }
        }
        seen.iter()
            .max_by_key(|(_, n)| *n)
            .map_or(Color32::TRANSPARENT, |(raw, _)| format.decode(*raw))
    }

    pub fn text_width(&self, text: &str) -> i32 {
        crate::text::ui::width(text, BODY.size)
    }

    /// The font's own line height for body text, less than [`BODY`]'s box.
    pub fn text_height(&self) -> i32 {
        crate::text::ui::line_height(BODY.size)
    }

    /// See [`MeasureCtx::baseline_in`](crate::traits::MeasureCtx::baseline_in).
    pub fn baseline_in(&self, height: i32) -> i32 {
        crate::text::ui::baseline_in(height, BODY.size)
    }

    /// `icon` in the `size × size` box at `(x, y)`.
    pub fn draw_icon(&mut self, icon: Icon, x: i32, y: i32, size: i32, color: Color32) {
        let dr = self.clip.to_damage_rect();
        crate::text::icons::draw_clipped(self.buffer, x, y, icon, size, color, &dr);
    }

    pub fn theme(&self) -> &'static Theme {
        self.style.theme
    }

    pub fn mono_cell_width(&self) -> i32 {
        crate::text::cell_width()
    }

    pub fn mono_cell_height(&self) -> i32 {
        crate::text::cell_height()
    }

    /// A rounded ring just outside `rect`; a no-op unless focus is visible.
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        if !self.focus_visible {
            return;
        }
        let color = self.style.theme.focus;
        let offset = self.style.focus_ring_offset;
        for i in 0..self.style.focus_ring_width {
            let out = offset + i + 1;
            self.draw_rounded_rect(
                rect.x - out,
                rect.y - out,
                rect.width + out * 2,
                rect.height + out * 2,
                radius::CONTROL + out,
                color,
            );
        }
    }

    /// [`Self::draw_focus_ring`] inside `rect`, for a view whose edges meet
    /// its neighbours' or the window's.
    pub fn draw_inner_focus_ring(&mut self, rect: Rect) {
        let reach = self.style.focus_ring_offset + self.style.focus_ring_width;
        self.draw_focus_ring(rect.inflate(-reach));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_image(
        &mut self,
        bitmap: BitmapRef<'_>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        fit: ImageFit,
        sampling: ImageSampling,
    ) {
        let dr = self.clip.to_damage_rect();
        slopos_gfx::image::draw_image_clipped(self.buffer, bitmap, x, y, w, h, fit, sampling, &dr);
    }

    /// Run `f` with the clip rect intersected against `new_clip`.
    pub fn with_clip<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, new_clip: Rect, f: F) {
        let (old_clip, old_fixed) = (self.clip, self.fixed);
        self.clip = self.clip.intersect(&new_clip).unwrap_or(Rect::ZERO);
        self.fixed = self.fixed.intersect(&new_clip).unwrap_or(Rect::ZERO);
        f(self);
        (self.clip, self.fixed) = (old_clip, old_fixed);
    }

    /// Run `f` clipped to `rect`, a view onto content that goes on past it,
    /// as a scroll view's or a field's: what its edge cuts is not cut short.
    pub fn with_viewport<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, rect: Rect, f: F) {
        let (old_clip, old_fixed, old_viewport) = (self.clip, self.fixed, self.viewport);
        self.clip = self.clip.intersect(&rect).unwrap_or(Rect::ZERO);
        self.fixed = Rect::new(i32::MIN / 4, i32::MIN / 4, i32::MAX / 2, i32::MAX / 2);
        self.viewport = Some(rect);
        f(self);
        (self.clip, self.fixed, self.viewport) = (old_clip, old_fixed, old_viewport);
    }

    /// Run `f` as the layer `root` paints over what was painted before,
    /// which its text may lie over.
    pub fn with_layer<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, root: WidgetId, f: F) {
        let old = self.layer;
        self.layer = root;
        f(self);
        self.layer = old;
    }

    pub fn with_scroll_offset<F: FnOnce(&mut PaintContext<'_, '_>)>(
        &mut self,
        dx: i32,
        dy: i32,
        f: F,
    ) {
        let old_x = self.scroll_offset_x;
        let old_y = self.scroll_offset_y;
        self.scroll_offset_x += dx;
        self.scroll_offset_y += dy;
        f(self);
        self.scroll_offset_x = old_x;
        self.scroll_offset_y = old_y;
    }

    pub fn is_focus_visible(&self) -> bool {
        self.focus_visible
    }
}
