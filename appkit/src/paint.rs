use slopos_abi::draw::{Canvas, Color32};
use slopos_gfx::DrawBuffer;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_gfx::rounded::Rounded;
use slopos_theme_core::{Icon, TextRole, Theme, radius, stroke};

use super::audit::{Mark, TextRun};
use super::constraints::Rect;
use super::focus::RING_REACH;
use super::style::StyleSheet;
use super::text::ui;
use super::traits::WidgetId;

/// What a widget paints into; it paints only within `clip`, in window
/// coordinates.
pub struct PaintContext<'a, 'b> {
    pub buffer: &'a mut DrawBuffer<'b>,
    pub clip: Rect,
    /// The clip less every viewport's: an edge that cuts text short.
    fixed: Rect,
    viewport: Option<Rect>,
    /// What every viewport around shows, whatever part of it is repainted.
    shown: Rect,
    layer: WidgetId,
    pub focus_visible: bool,
    /// The widget and part the pointer lights, which every widget paints its
    /// hover from: it is rebuilt between messages and keeps none.
    pub hovered: Option<(WidgetId, usize)>,
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
            shown: Rect::new(0, 0, w, h),
            layer: WidgetId::NONE,
            focus_visible: false,
            hovered: None,
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

    /// A 1 px outline.
    pub fn draw_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color32) {
        self.fill_rect(x, y, w, 1, color);
        self.fill_rect(x, y + h - 1, w, 1, color);
        self.fill_rect(x, y + 1, 1, h - 2, color);
        self.fill_rect(x + w - 1, y + 1, 1, h - 2, color);
    }

    /// Blended, so a translucent `color` washes over what is there.
    pub fn fill_rounded_rect(&mut self, rect: Rect, radius: i32, color: Color32) {
        let clip = self.clip.to_damage_rect();
        let shape = Rounded::new(rect.x, rect.y, rect.width, rect.height, radius);
        shape.draw(self.buffer, &clip, color);
    }

    /// An outline `width` thick inside `rect`, with rounded corners.
    pub fn draw_rounded_rect(&mut self, rect: Rect, radius: i32, width: i32, color: Color32) {
        let clip = self.clip.to_damage_rect();
        let outline = Rounded::new(rect.x, rect.y, rect.width, rect.height, radius);
        outline.stroke(self.buffer, width, &clip, color);
    }

    pub fn draw_top_rim(&mut self, rect: Rect, radius: i32) {
        let clip = self.clip.to_damage_rect();
        let shape = Rounded::new(rect.x, rect.y, rect.width, rect.height, radius);
        let fade = (rect.height / 2).min(2 * radius).max(1);
        shape.top_rim(self.buffer, &clip, self.style.theme.rim_light, fade);
    }

    pub fn fill_circle(&mut self, x: i32, y: i32, diameter: i32, color: Color32) {
        let clip = self.clip.to_damage_rect();
        Rounded::circle(x, y, diameter).draw(self.buffer, &clip, color);
    }

    /// The theme's raised shadow under the `rect` rounded at `radius`, which
    /// something opaque then covers.
    pub fn draw_shadow(&mut self, rect: Rect, radius: i32) {
        let shadow = self.theme().shadow_raised;
        let caster = Rounded::new(
            rect.x,
            rect.y + shadow.offset_y,
            rect.width,
            rect.height,
            radius,
        );
        let clip = self.clip.to_damage_rect();
        let under = Rounded::new(rect.x, rect.y, rect.width, rect.height, radius).bands()[1];
        caster.draw_shadow(self.buffer, shadow.spread, &clip, &under, shadow.color);
    }

    /// `text` in `role` with its line's top-left at `(x, y)`.
    pub fn draw_text(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32) {
        self.text_line(x, y, text, role, fg, Mark::Whole);
    }

    /// `text` in `role` cut short to `max_w` with an ellipsis; answers whether
    /// it was cut, which a tooltip then shows whole.
    pub fn draw_text_fitted(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        max_w: i32,
        fg: Color32,
    ) -> bool {
        if self.runs.is_some() {
            match ui::elide_end(text, role, max_w) {
                (kept, false) if kept.len() == text.len() => {
                    self.note_line(x, y, text, role, fg, Mark::Whole)
                }
                (kept, true) => {
                    let after = x + ui::width(kept, role);
                    self.note_line(x, y, kept, role, fg, Mark::Elided);
                    self.note_line(after, y, ui::ELLIPSIS, role, fg, Mark::Elided);
                }
                _ => self.note_line(x, y, text, role, fg, Mark::Dropped),
            }
        }
        let dr = self.clip.to_damage_rect();
        let bg = Color32::TRANSPARENT;
        ui::draw_elided(self.buffer, x, y, text, role, max_w, fg, bg, &dr)
    }

    /// [`Self::draw_text_fitted`] cut from the start, keeping the end, as a
    /// file name's end tells it apart.
    pub fn draw_text_tail(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        role: TextRole,
        max_w: i32,
        fg: Color32,
    ) -> bool {
        match ui::elide_start(text, role, max_w) {
            (kept, false) if kept.len() == text.len() => {
                self.text_line(x, y, text, role, fg, Mark::Whole);
                false
            }
            (kept, true) => {
                let after = x + ui::width(ui::ELLIPSIS, role);
                self.text_line(x, y, ui::ELLIPSIS, role, fg, Mark::Elided);
                self.text_line(after, y, kept, role, fg, Mark::Elided);
                true
            }
            _ => {
                self.note_line(x, y, text, role, fg, Mark::Dropped);
                true
            }
        }
    }

    fn text_line(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32, mark: Mark) {
        self.note_line(x, y, text, role, fg, mark);
        let dr = self.clip.to_damage_rect();
        ui::draw(self.buffer, x, y, text, role, fg, Color32::TRANSPARENT, &dr);
    }

    /// Notes a line for the audit, with the colour most of its box holds.
    fn note_line(&mut self, x: i32, y: i32, text: &str, role: TextRole, fg: Color32, mark: Mark) {
        if self.runs.is_none() || text.trim().is_empty() {
            return;
        }
        let height = ui::font_height(role);
        let width = ui::width(text, role);
        let under = self.sample_under(Rect::new(x, y, width, height));
        self.note(TextRun {
            x,
            top: y,
            baseline: y + ui::ascent(role),
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

    /// `icon` in the `size × size` box at `(x, y)`.
    pub fn draw_icon(&mut self, icon: Icon, x: i32, y: i32, size: i32, color: Color32) {
        let dr = self.clip.to_damage_rect();
        super::text::icons::draw_clipped(self.buffer, x, y, icon, size, color, &dr);
    }

    pub fn theme(&self) -> &'static Theme {
        self.style.theme
    }

    /// The part of widget `id` the pointer lights, as its
    /// [`Widget::hover_part`](crate::traits::Widget::hover_part) answered.
    pub fn lit(&self, id: WidgetId) -> Option<usize> {
        self.hovered
            .filter(|&(at, _)| at == id)
            .map(|(_, part)| part)
    }

    /// A rounded ring just outside `rect`, while focus is visible.
    pub fn draw_focus_ring(&mut self, rect: Rect) {
        if !self.focus_visible {
            return;
        }
        let ring = rect.inflate(RING_REACH);
        let clip = self.clip.to_damage_rect();
        let outline = Rounded::new(
            ring.x,
            ring.y,
            ring.width,
            ring.height,
            radius::CONTROL + RING_REACH,
        );
        outline.stroke(self.buffer, stroke::FOCUS, &clip, self.style.theme.focus);
    }

    /// [`Self::draw_focus_ring`] inside `rect`, for a view whose edges meet
    /// its neighbours' or the window's.
    pub fn draw_inner_focus_ring(&mut self, rect: Rect) {
        self.draw_focus_ring(rect.inflate(-RING_REACH));
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

    /// Runs `f` with the clip narrowed to `new_clip`.
    pub fn with_clip<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, new_clip: Rect, f: F) {
        let (old_clip, old_fixed) = (self.clip, self.fixed);
        self.clip = self.clip.intersect(&new_clip).unwrap_or(Rect::ZERO);
        self.fixed = self.fixed.intersect(&new_clip).unwrap_or(Rect::ZERO);
        f(self);
        (self.clip, self.fixed) = (old_clip, old_fixed);
    }

    /// Runs `f` clipped to `rect`, a view onto content that goes on past it,
    /// as a scroll view's or a field's: what its edge cuts is not cut short.
    pub fn with_viewport<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, rect: Rect, f: F) {
        let (old_clip, old_fixed, old_viewport) = (self.clip, self.fixed, self.viewport);
        let old_shown = self.shown;
        self.clip = self.clip.intersect(&rect).unwrap_or(Rect::ZERO);
        self.fixed = Rect::new(i32::MIN / 4, i32::MIN / 4, i32::MAX / 2, i32::MAX / 2);
        self.viewport = Some(rect);
        self.shown = self.shown.intersect(&rect).unwrap_or(Rect::ZERO);
        f(self);
        (self.clip, self.fixed, self.viewport) = (old_clip, old_fixed, old_viewport);
        self.shown = old_shown;
    }

    /// Whether every viewport around what is painting shows all of `rect`.
    pub(crate) fn shows(&self, rect: Rect) -> bool {
        self.shown.intersect(&rect) == Some(rect)
    }

    /// Runs `f` as the layer `root` paints over what was painted before,
    /// which its text may lie over.
    pub fn with_layer<F: FnOnce(&mut PaintContext<'_, '_>)>(&mut self, root: WidgetId, f: F) {
        let old = self.layer;
        self.layer = root;
        f(self);
        self.layer = old;
    }
}
