use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size, TextAlignment};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct LabelWidget {
    core: WidgetCore,
    text: String,
    alignment: TextAlignment,
    wrap: bool,
    max_lines: Option<u32>,
    color: Option<Color32>,
    baseline: i32,
}

impl LabelWidget {
    pub fn new(text: String, alignment: TextAlignment, wrap: bool, max_lines: Option<u32>) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            alignment,
            wrap,
            max_lines,
            color: None,
            baseline: 0,
        }
    }

    pub fn with_color(mut self, color: Color32) -> Self {
        self.color = Some(color);
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
    }

    fn wrap_lines(&self, text: &str, max_width: i32, ctx: &PaintContext) -> Vec<String> {
        let mut lines = Vec::new();
        for raw_line in text.split('\n') {
            if raw_line.is_empty() {
                lines.push(String::new());
                continue;
            }
            let mut current = String::new();
            for word in raw_line.split_whitespace() {
                if current.is_empty() {
                    // First word on the line — always accept it even if it overflows.
                    current.push_str(word);
                } else {
                    let candidate = format!("{} {}", current, word);
                    if ctx.text_width(&candidate) <= max_width {
                        current = candidate;
                    } else {
                        lines.push(current);
                        current = word.to_string();
                    }
                }
            }
            lines.push(current);
        }
        if let Some(max) = self.max_lines {
            lines.truncate(max as usize);
        }
        lines
    }
}

impl Widget for LabelWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let line_height = ctx.style.line_height;
        self.baseline = ctx.baseline_in(line_height);

        if !self.wrap {
            let text_w = ctx.text_width(&self.text);
            let mut lines = 1i32;
            if self.text.contains('\n') {
                lines = self.text.split('\n').count() as i32;
            }
            if let Some(max) = self.max_lines {
                lines = lines.min(max as i32);
            }
            let size = Size::new(text_w, line_height * lines);
            constraints.constrain(size)
        } else {
            // Wrapping needs a real width to wrap against; an unbounded parent
            // (a scroll view's cross axis) gets the natural single-line width.
            let avail_w = if constraints.is_width_bounded() {
                constraints.max_width
            } else {
                return constraints.constrain(Size::new(ctx.text_width(&self.text), line_height));
            };
            let mut line_count = 0u32;
            for raw_line in self.text.split('\n') {
                if raw_line.is_empty() {
                    line_count += 1;
                    continue;
                }
                let mut current_w = 0i32;
                let mut on_line = false;
                for word in raw_line.split_whitespace() {
                    let word_w = ctx.text_width(word);
                    if !on_line {
                        current_w = word_w;
                        on_line = true;
                    } else {
                        let space_w = ctx.text_width(" ");
                        if current_w + space_w + word_w <= avail_w {
                            current_w += space_w + word_w;
                        } else {
                            line_count += 1;
                            current_w = word_w;
                        }
                    }
                }
                if on_line {
                    line_count += 1;
                }
            }
            if line_count == 0 {
                line_count = 1;
            }
            if let Some(max) = self.max_lines {
                line_count = line_count.min(max);
            }
            let size = Size::new(avail_w, line_height * line_count as i32);
            constraints.constrain(size)
        }
    }

    /// Each line in a line box of `style.line_height` from the top, its text
    /// on that box's baseline: the row a [`StyledLabelWidget`] of one line
    /// puts its text on, so the two line up side by side.
    ///
    /// [`StyledLabelWidget`]: super::styled_label::StyledLabelWidget
    fn paint(&self, ctx: &mut PaintContext) {
        let fg = self.color.unwrap_or(ctx.style.text_primary);
        let line_height = ctx.style.line_height;
        let rect = self.layout_rect();
        let first = rect.y + ctx.baseline_in(line_height);
        let draw = |ctx: &mut PaintContext, i: usize, line: &str| {
            let tw = ctx.text_width(line);
            let x = match self.alignment {
                TextAlignment::Start => rect.x,
                TextAlignment::Center => rect.x + (rect.width - tw) / 2,
                TextAlignment::End => rect.x + rect.width - tw,
            };
            ctx.draw_text_on_baseline(x, first + i as i32 * line_height, line, fg);
        };

        if self.wrap {
            for (i, line) in self
                .wrap_lines(&self.text, rect.width, ctx)
                .iter()
                .enumerate()
            {
                draw(ctx, i, line);
            }
        } else {
            let max = self.max_lines.map_or(usize::MAX, |m| m as usize);
            for (i, line) in self.text.split('\n').take(max).enumerate() {
                draw(ctx, i, line);
            }
        }
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn baseline(&self) -> Option<i32> {
        Some(self.baseline)
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
