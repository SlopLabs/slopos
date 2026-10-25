use slopos_abi::draw::Color32;
use slopos_gfx::DrawBuffer;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};

use slopos_theme_core::{TextRole, Theme};

use super::constraints::Rect;
use super::style::StyleSheet;
use super::text::BODY;

/// Paint target handed to each widget; the framework sets the clip rect before
/// calling `paint()`, and widgets may only paint within it.
pub struct PaintContext<'a> {
    pub buffer: &'a mut DrawBuffer<'a>,
    /// Window coordinates.
    pub clip: Rect,
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
}

impl<'a> PaintContext<'a> {
    pub fn new(buffer: &'a mut DrawBuffer<'a>, style: &'a StyleSheet) -> Self {
        let w = buffer.width() as i32;
        let h = buffer.height() as i32;
        Self {
            buffer,
            clip: Rect::new(0, 0, w, h),
            scroll_offset_x: 0,
            scroll_offset_y: 0,
            focus_visible: false,
            pointer: (-1, -1),
            style,
        }
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
        // Unclipped here: gfx's rounded rect does its own bounds checking.
        slopos_gfx::canvas_ops::rounded_rect_filled(self.buffer, x, y, w, h, radius, color);
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
        slopos_gfx::canvas_ops::rounded_rect(self.buffer, x, y, w, h, radius, color);
    }

    /// Body text with its line box's top-left at `(x, y)`.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
        let dr = self.clip.to_damage_rect();
        crate::text::ui::draw_clipped(self.buffer, x, y, text, BODY.size, BODY.weight, fg, bg, &dr);
    }

    pub fn draw_text_transparent(&mut self, x: i32, y: i32, text: &str, fg: Color32) {
        self.draw_text(x, y, text, fg, Color32::TRANSPARENT);
    }

    /// Body text whose baseline lands on row `baseline`, composited over what
    /// is there.
    pub fn draw_text_on_baseline(&mut self, x: i32, baseline: i32, text: &str, fg: Color32) {
        let top = baseline - crate::text::ui::ascent(BODY.size);
        self.draw_text(x, top, text, fg, Color32::TRANSPARENT);
    }

    /// Text in `role`, composited over what is there.
    pub fn draw_text_in(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32) {
        let dr = self.clip.to_damage_rect();
        let style = role.style();
        crate::text::ui::draw_clipped(
            self.buffer,
            x,
            y,
            text,
            style.size,
            style.weight,
            fg,
            Color32::TRANSPARENT,
            &dr,
        );
    }

    /// Fixed-cell text, for content whose columns must line up (code, hex,
    /// tabular numbers).
    pub fn draw_mono_text(&mut self, x: i32, y: i32, text: &str, fg: Color32, bg: Color32) {
        let dr = self.clip.to_damage_rect();
        crate::text::draw_str_clipped(self.buffer, x, y, text, fg, bg, &dr);
    }

    pub fn text_width(&self, text: &str) -> i32 {
        crate::text::ui::width(text, BODY.size)
    }

    pub fn text_width_in(&self, text: &str, role: TextRole) -> i32 {
        let style = role.style();
        crate::text::ui::width_weighted(text, style.size, style.weight)
    }

    /// The font's own line height for body text, less than [`BODY`]'s box.
    pub fn text_height(&self) -> i32 {
        crate::text::ui::line_height(BODY.size)
    }

    /// See [`MeasureCtx::baseline_in`](crate::traits::MeasureCtx::baseline_in).
    pub fn baseline_in(&self, height: i32) -> i32 {
        crate::text::ui::baseline_in(height, BODY.size)
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

    /// Draw the focus ring just outside `rect`; a no-op unless focus is visible.
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        if !self.focus_visible {
            return;
        }
        let offset = self.style.focus_ring_offset;
        let width = self.style.focus_ring_width;
        let color = self.style.theme.focus;
        let x = rect.x - offset - width;
        let y = rect.y - offset - width;
        let w = rect.width + (offset + width) * 2;
        let inner_x = rect.x - offset;
        let inner_y = rect.y - offset;
        let inner_w = rect.width + offset * 2;
        let inner_h = rect.height + offset * 2;

        self.fill_rect(x, y, w, width, color);
        self.fill_rect(x, inner_y + inner_h, w, width, color);
        self.fill_rect(x, inner_y, width, inner_h, color);
        self.fill_rect(inner_x + inner_w, inner_y, width, inner_h, color);
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
    pub fn with_clip<F: FnOnce(&mut PaintContext<'_>)>(&mut self, new_clip: Rect, f: F) {
        let old_clip = self.clip;
        self.clip = self.clip.intersect(&new_clip).unwrap_or(Rect::ZERO);
        f(self);
        self.clip = old_clip;
    }

    pub fn with_scroll_offset<F: FnOnce(&mut PaintContext<'_>)>(&mut self, dx: i32, dy: i32, f: F) {
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
