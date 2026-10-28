use std::borrow::Cow;
use std::cell::Cell;

use slopos_theme_core::{TextRole, Tone};

use crate::components::{Text, Wrap};
use crate::constraints::{BoxConstraints, Rect, Size, TextAlignment};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

/// Text in a role and a tone: one line cut short with an ellipsis, or lines
/// wrapped to the width it is given.
pub struct TextWidget {
    core: WidgetCore,
    text: String,
    role: TextRole,
    tone: Tone,
    align: TextAlignment,
    wrap: Wrap,
    max_lines: Option<u32>,
    /// Byte ranges of `text`, one per line drawn, as laid out for the width
    /// last given.
    lines: Vec<(usize, usize)>,
    /// The last paint left some of the text out, which a tooltip shows whole.
    cut: Cell<bool>,
    /// Lines after the last are left out, so it ends in an ellipsis.
    truncated: bool,
}

impl TextWidget {
    pub fn new(spec: &Text) -> Self {
        Self {
            core: WidgetCore::new(),
            text: spec.text.clone(),
            role: spec.role,
            tone: spec.tone,
            align: spec.align,
            wrap: spec.wrap,
            max_lines: spec.max_lines,
            lines: Vec::new(),
            cut: Cell::new(false),
            truncated: false,
        }
    }

    fn line_height(&self) -> i32 {
        self.role.style().line_height
    }

    /// Lays the text out for `width`, keeping at most `keep` lines.
    fn lay_out(&mut self, width: Option<i32>, keep: Option<usize>) {
        let mut lines = break_lines(&self.text, self.role, self.wrap, width);
        let keep = keep
            .into_iter()
            .chain(self.max_lines.map(|m| m as usize))
            .min()
            .map(|k| k.max(1));
        self.truncated = keep.is_some_and(|k| lines.len() > k);
        lines.truncate(keep.unwrap_or(lines.len()));
        self.lines = lines;
    }

    fn block_height(&self) -> i32 {
        self.line_height() * self.lines.len().max(1) as i32
    }

    /// Line `i` as drawn, the last ending in an ellipsis when lines after it
    /// are left out.
    fn shown(&self, i: usize) -> Cow<'_, str> {
        let (s, e) = self.lines[i];
        let line = self.text[s..e].trim_end();
        match self.truncated && i + 1 == self.lines.len() {
            true => Cow::Owned(format!("{line}{}", ui::ELLIPSIS)),
            false => Cow::Borrowed(line),
        }
    }

    fn top(&self, rect: Rect) -> i32 {
        rect.y + (rect.height - self.block_height()).max(0) / 2
    }
}

impl Widget for TextWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let bound = constraints
            .is_width_bounded()
            .then_some(constraints.max_width);
        let wraps = self.wrap != Wrap::None;
        self.lay_out(if wraps { bound } else { None }, None);
        let widest = (0..self.lines.len())
            .map(|i| ui::width(&self.shown(i), self.role))
            .max()
            .unwrap_or(0);
        constraints.constrain(Size::new(widest, self.block_height()))
    }

    fn layout(&mut self, rect: Rect) {
        let wraps = self.wrap != Wrap::None;
        let room = (rect.height / self.line_height()) as usize;
        self.lay_out(Some(rect.width).filter(|_| wraps), Some(room));
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let fg = ctx.theme().ink(self.tone);
        let lh = self.line_height();
        let top = self.top(rect);
        let mut cut = self.truncated;
        for i in 0..self.lines.len() {
            let line = self.shown(i);
            let w = ui::width(&line, self.role).min(rect.width);
            let x = match self.align {
                TextAlignment::Start => rect.x,
                TextAlignment::Center => rect.x + (rect.width - w) / 2,
                TextAlignment::End => rect.x + rect.width - w,
            };
            let y = ui::centred_top(top + i as i32 * lh, lh, self.role);
            cut |= ctx.draw_text_fitted(x, y, &line, self.role, rect.width, fg);
        }
        self.cut.set(cut);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
    }

    /// In the box `measure` answered, which `paint` centres the lines in.
    fn baseline(&self) -> Option<i32> {
        let above = (self.measured_size().height - self.block_height()).max(0) / 2;
        Some(above + ui::baseline_in(self.line_height(), self.role))
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.cut.get().then_some(self.text.as_str())
    }
}

/// Byte ranges of `text`'s lines in `role` for `width`, `None` meaning
/// unbounded: one per `\n`, and more where `wrap` breaks one.
pub(crate) fn break_lines(
    text: &str,
    role: TextRole,
    wrap: Wrap,
    width: Option<i32>,
) -> Vec<(usize, usize)> {
    let mut lines = Vec::new();
    let mut start = 0;
    for hard in text.split('\n') {
        let end = start + hard.len();
        match (wrap, width) {
            (Wrap::None, _) | (_, None) => lines.push((start, end)),
            (Wrap::Words, Some(w)) => break_words(text, role, start, end, w, &mut lines),
            (Wrap::Chars, Some(w)) => break_chars(text, role, start, end, w, &mut lines),
        }
        start = end + 1;
    }
    lines
}

fn break_words(
    text: &str,
    role: TextRole,
    start: usize,
    end: usize,
    width: i32,
    out: &mut Vec<(usize, usize)>,
) {
    let fits = |(s, e): (usize, usize)| ui::width(&text[s..e], role) <= width;
    let line = &text[start..end];
    let mut words = line
        .split_whitespace()
        .map(|w| {
            let at = start + (w.as_ptr() as usize - line.as_ptr() as usize);
            (at, at + w.len())
        })
        .peekable();
    if words.peek().is_none() {
        out.push((start, start));
        return;
    }
    let mut current: Option<(usize, usize)> = None;
    for word in words {
        match current.map(|(s, _)| (s, word.1)) {
            Some(joined) if fits(joined) => current = Some(joined),
            _ => {
                out.extend(current);
                if fits(word) {
                    current = Some(word);
                } else {
                    break_chars(text, role, word.0, word.1, width, out);
                    current = out.pop();
                }
            }
        }
    }
    out.extend(current);
}

fn break_chars(
    text: &str,
    role: TextRole,
    start: usize,
    end: usize,
    width: i32,
    out: &mut Vec<(usize, usize)>,
) {
    let mut at = start;
    loop {
        let rest = &text[at..end];
        let take = match ui::prefix_fitting(rest, role, width) {
            0 => rest.chars().next().map_or(0, char::len_utf8),
            fits => fits,
        };
        if take == 0 || at + take >= end {
            out.push((at, end));
            return;
        }
        out.push((at, at + take));
        at += take;
    }
}
