//! The code surface: a gutter, a viewport of styled lines, a caret and a
//! selection.
//!
//! The widget is given exactly the lines that are visible and their spans, and
//! it reports back in **document coordinates** — a click is a `(line, col)` in
//! the file, not a pixel in this rect. That split is what keeps the buffer, the
//! undo history and the highlighter out of the toolkit: this draws text and
//! resolves geometry, and the application owns the document.
//!
//! Geometry is fixed-cell (see [`crate::text`]): a column is one advance, which
//! is what makes a pixel-to-column mapping a division rather than a measurement
//! loop. Tabs are the one exception and the reason a *display* column exists
//! beside the character column: a tab advances to the next multiple of
//! `tab_width`, so the two indices diverge on any line that contains one.

use std::any::Any;

use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Horizontal padding inside the gutter, each side.
const GUTTER_PAD: i32 = 8;
/// Gap between the gutter and the first text column.
const TEXT_PAD: i32 = 8;
/// Width of the caret.
const CARET_WIDTH: i32 = 2;
/// Width of the overview scrollbar on the right edge.
const SCROLLBAR_WIDTH: i32 = 6;

/// A coloured run within a line, in character indices.
#[derive(Clone, Copy, Debug)]
pub struct StyledSpan {
    pub start: usize,
    pub end: usize,
    pub color: Color32,
}

/// One visible line: its number, its text and the spans that colour it.
#[derive(Clone, Debug)]
pub struct CodeLine {
    /// Document line index (0-based); the gutter shows it 1-based.
    pub number: usize,
    pub text: String,
    pub spans: Vec<StyledSpan>,
    /// Extra background runs — search hits — in character indices.
    pub highlights: Vec<(usize, usize)>,
}

/// What the code surface reports; the application interprets it against the
/// document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodeInput {
    Click {
        line: usize,
        col: usize,
        /// Shift was held: extend the selection rather than replacing it.
        extend: bool,
    },
    /// The pointer moved while a selection drag was live.
    Drag {
        line: usize,
        col: usize,
    },
    /// The drag ended.
    Release,
    Scroll {
        delta_lines: i32,
    },
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    Text {
        character: char,
    },
}

/// Lines that fit in `height` pixels. The application sizes the document's
/// viewport with this, so the rows it sends are exactly the rows drawn.
pub fn visible_line_count(height: i32, line_height: i32) -> usize {
    if height <= 0 || line_height <= 0 {
        return 0;
    }
    (height / line_height).max(0) as usize
}

/// Width of the gutter for a file of `total_lines`, at `cell_width`.
pub fn gutter_width(total_lines: usize, cell_width: i32, show_line_numbers: bool) -> i32 {
    if !show_line_numbers {
        return TEXT_PAD;
    }
    let digits = digit_count(total_lines.max(1));
    GUTTER_PAD * 2 + digits * cell_width
}

fn digit_count(n: usize) -> i32 {
    let mut digits = 1;
    let mut value = n;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

/// Display column of character `col` on `text`, expanding tabs.
pub fn display_col(text: &str, col: usize, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let mut display = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i >= col {
            break;
        }
        if ch == '\t' {
            display += tab - (display % tab);
        } else {
            display += 1;
        }
    }
    // A column past the end of the line keeps counting, so a caret parked past a
    // short line still lands where the goal column says.
    display + col.saturating_sub(text.chars().count())
}

/// Character column containing display column `display` on `text`. The exact
/// inverse of [`display_col`]; `editor-core`'s `buffer::char_col_from_display`
/// is the same function and the two must agree, because the viewport origin
/// crosses between them.
pub fn char_col_from_display(text: &str, display: usize, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let mut current = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let width = if ch == '\t' { tab - (current % tab) } else { 1 };
        if display < current + width {
            return i;
        }
        current += width;
    }
    let len = text.chars().count();
    len + display.saturating_sub(current)
}

/// Character column for a click `half_display` half-cells from the line's
/// start: past a character's midpoint the caret belongs after it.
///
/// Half cells, because at whole-cell resolution a one-cell-wide character can
/// never round up — the click and the character's own column are the same
/// number — and the caret could then only ever land on a glyph's left edge.
pub fn char_col_at_half(text: &str, half_display: usize, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let mut current = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let width = 2 * if ch == '\t' {
            tab - ((current / 2) % tab)
        } else {
            1
        };
        if half_display < current + width / 2 {
            return i;
        }
        current += width;
    }
    let len = text.chars().count();
    len + half_display.saturating_sub(current) / 2
}

type InputCallback = Box<dyn Fn(CodeInput) -> Box<dyn Any>>;

pub struct CodeViewWidget {
    core: WidgetCore,
    lines: Vec<CodeLine>,
    first_line: usize,
    total_lines: usize,
    first_col: usize,
    tab_width: usize,
    cursor: Option<(usize, usize)>,
    selection: Option<((usize, usize), (usize, usize))>,
    show_line_numbers: bool,
    focused: bool,
    /// Whether a selection drag is in progress. Given rather than remembered:
    /// the widget tree is rebuilt on every message, so a flag the widget set on
    /// the press would be gone before the first move arrived.
    selecting: bool,
    on_input: Option<InputCallback>,
    /// Cell metrics from the last paint/measure; hit testing must use what paint
    /// used or a click lands on the wrong column.
    cell_w: i32,
    line_h: i32,
}

impl CodeViewWidget {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lines: Vec<CodeLine>,
        first_line: usize,
        total_lines: usize,
        first_col: usize,
        tab_width: usize,
        cursor: Option<(usize, usize)>,
        selection: Option<((usize, usize), (usize, usize))>,
        show_line_numbers: bool,
        focused: bool,
        selecting: bool,
        on_input: Option<InputCallback>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            lines,
            first_line,
            total_lines,
            first_col,
            tab_width: tab_width.max(1),
            cursor,
            selection,
            show_line_numbers,
            focused,
            selecting,
            on_input,
            cell_w: crate::text::cell_width(),
            line_h: crate::text::cell_height(),
        }
    }

    fn gutter(&self) -> i32 {
        gutter_width(self.total_lines, self.cell_w, self.show_line_numbers)
    }

    fn text_origin_x(&self) -> i32 {
        self.layout_rect().x + self.gutter() + TEXT_PAD
    }

    fn emit(&self, input: CodeInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }

    /// Document position under a pixel, clamped into the visible rows.
    fn position_at(&self, x: i32, y: i32) -> (usize, usize) {
        let rect = self.layout_rect();
        let row = if self.line_h > 0 {
            ((y - rect.y) / self.line_h).max(0) as usize
        } else {
            0
        };
        let line = (self.first_line + row).min(self.total_lines.saturating_sub(1));
        let text = self.lines.get(row).map(|l| l.text.as_str()).unwrap_or("");
        let local_x = x - self.text_origin_x();
        if local_x < 0 {
            // The gutter: a click on a line number means the start of the
            // line, not the first column that happens to be scrolled into
            // view.
            return (line, 0);
        }
        let half = if self.cell_w > 0 {
            (local_x * 2).div_euclid(self.cell_w) as usize
        } else {
            0
        };
        (
            line,
            char_col_at_half(text, half + self.first_col * 2, self.tab_width),
        )
    }

    /// Selection range on one line as display columns, or `None`.
    fn selection_on_line(&self, line: usize, text: &str) -> Option<(usize, usize)> {
        let ((sl, sc), (el, ec)) = self.selection?;
        if line < sl || line > el {
            return None;
        }
        let start = if line == sl {
            display_col(text, sc, self.tab_width)
        } else {
            0
        };
        let end = if line == el {
            display_col(text, ec, self.tab_width)
        } else {
            // A selection running through this line covers its terminator too,
            // which is the one cell past its last column.
            display_col(text, text.chars().count(), self.tab_width) + 1
        };
        Some((start, end.max(start)))
    }

    fn paint_line_text(&self, ctx: &mut PaintContext, line: &CodeLine, y: i32, clip: Rect) {
        let default_fg = ctx.style.code_fg;
        let mut display = 0usize;
        let right_edge = clip.x + clip.width;

        // Runs are grouped by colour so a line is a handful of string draws
        // rather than one per character. A tab ends a run because it advances
        // without drawing.
        let mut run = String::new();
        let mut run_x = self.text_origin_x();
        let mut run_color = default_fg;

        for (index, ch) in line.text.chars().enumerate() {
            let width = if ch == '\t' {
                self.tab_width - (display % self.tab_width)
            } else {
                1
            };
            let color = span_color(&line.spans, index).unwrap_or(default_fg);

            if display + width > self.first_col {
                let cell_x = self.text_origin_x()
                    + (display.saturating_sub(self.first_col) as i32) * self.cell_w;
                if cell_x >= right_edge {
                    break;
                }
                if ch == '\t' {
                    if !run.is_empty() {
                        ctx.draw_mono_text(run_x, y, &run, run_color, Color32::TRANSPARENT);
                        run.clear();
                    }
                } else {
                    if run.is_empty() {
                        run_x = cell_x;
                        run_color = color;
                    } else if color != run_color {
                        ctx.draw_mono_text(run_x, y, &run, run_color, Color32::TRANSPARENT);
                        run.clear();
                        run_x = cell_x;
                        run_color = color;
                    }
                    run.push(ch);
                }
            }
            display += width;
        }
        if !run.is_empty() {
            ctx.draw_mono_text(run_x, y, &run, run_color, Color32::TRANSPARENT);
        }
    }

    fn paint_scrollbar(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let visible = visible_line_count(rect.height, self.line_h).max(1);
        if self.total_lines <= visible {
            return;
        }
        let track_h = rect.height;
        let thumb_h = ((visible as i64 * track_h as i64) / self.total_lines as i64)
            .max(24)
            .min(track_h as i64) as i32;
        let span = (self.total_lines - visible).max(1);
        let offset =
            ((self.first_line.min(span) as i64) * (track_h - thumb_h) as i64 / span as i64) as i32;
        let x = rect.x + rect.width - SCROLLBAR_WIDTH;
        ctx.fill_rect_blended(
            x,
            rect.y + offset,
            SCROLLBAR_WIDTH,
            thumb_h,
            Color32::new(0xc8, 0xcc, 0xd4, 0x4c),
        );
    }
}

fn span_color(spans: &[StyledSpan], index: usize) -> Option<Color32> {
    spans
        .iter()
        .find(|s| index >= s.start && index < s.end)
        .map(|s| s.color)
}

impl Widget for CodeViewWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        self.cell_w = crate::text::cell_width();
        self.line_h = crate::text::cell_height();
        constraints.constrain(constraints.max_size())
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, style.code_bg);

        let gutter = self.gutter();
        let content = Rect::new(
            rect.x + gutter,
            rect.y,
            (rect.width - gutter).max(0),
            rect.height,
        );
        let cursor_line = self.cursor.map(|(l, _)| l);

        for (row, line) in self.lines.iter().enumerate() {
            let y = rect.y + row as i32 * self.line_h;
            if y >= rect.y + rect.height {
                break;
            }
            let is_cursor_line = cursor_line == Some(line.number);

            if is_cursor_line && self.selection.is_none() {
                ctx.fill_rect(rect.x, y, rect.width, self.line_h, style.line_highlight);
            }

            if let Some((start, end)) = self.selection_on_line(line.number, &line.text) {
                let x0 = self.text_origin_x()
                    + (start.saturating_sub(self.first_col) as i32) * self.cell_w;
                let x1 = self.text_origin_x()
                    + (end.saturating_sub(self.first_col) as i32) * self.cell_w;
                if end > self.first_col && x1 > x0 {
                    ctx.with_clip(content, |ctx| {
                        ctx.fill_rect_blended(x0, y, x1 - x0, self.line_h, style.selection_bg);
                    });
                }
            }

            for (start, end) in &line.highlights {
                let ds = display_col(&line.text, *start, self.tab_width);
                let de = display_col(&line.text, *end, self.tab_width);
                let x0 =
                    self.text_origin_x() + (ds.saturating_sub(self.first_col) as i32) * self.cell_w;
                let x1 =
                    self.text_origin_x() + (de.saturating_sub(self.first_col) as i32) * self.cell_w;
                if de > self.first_col && x1 > x0 {
                    ctx.with_clip(content, |ctx| {
                        ctx.fill_rect_blended(x0, y, x1 - x0, self.line_h, style.match_highlight);
                    });
                }
            }

            if self.show_line_numbers {
                let number = format!("{}", line.number + 1);
                let width = number.chars().count() as i32 * self.cell_w;
                let nx = rect.x + gutter - GUTTER_PAD - width;
                let color = if is_cursor_line {
                    style.gutter_fg_active
                } else {
                    style.gutter_fg
                };
                ctx.draw_mono_text(nx, y, &number, color, Color32::TRANSPARENT);
            }

            ctx.with_clip(content, |ctx| {
                self.paint_line_text(ctx, line, y, content);
            });
        }

        if let Some((line, col)) = self.cursor {
            if line >= self.first_line {
                let row = line - self.first_line;
                if row < self.lines.len() || self.lines.is_empty() {
                    let text = self.lines.get(row).map(|l| l.text.as_str()).unwrap_or("");
                    let display = display_col(text, col, self.tab_width);
                    if display >= self.first_col {
                        let x =
                            self.text_origin_x() + (display - self.first_col) as i32 * self.cell_w;
                        let y = rect.y + row as i32 * self.line_h;
                        // A blurred surface shows where the caret is without
                        // claiming the keyboard: hollow rather than solid.
                        if self.focused {
                            ctx.with_clip(content, |ctx| {
                                ctx.fill_rect(x, y, CARET_WIDTH, self.line_h, style.cursor_color);
                            });
                        } else {
                            ctx.with_clip(content, |ctx| {
                                ctx.draw_rect(x, y, self.cell_w, self.line_h, style.gutter_fg);
                            });
                        }
                    }
                }
            }
        }

        self.paint_scrollbar(ctx);
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::PointerDown {
                x,
                y,
                button,
                modifiers,
            } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                if *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                let (line, col) = self.position_at(*x, *y);
                self.emit(
                    CodeInput::Click {
                        line,
                        col,
                        extend: modifiers.shift,
                    },
                    sink,
                );
                EventResponse::CapturePointer
            }

            WidgetEvent::PointerMove { x, y } => {
                if !self.selecting {
                    return EventResponse::Ignored;
                }
                let (line, col) = self.position_at(*x, *y);
                self.emit(CodeInput::Drag { line, col }, sink)
            }

            WidgetEvent::PointerUp {
                button: PointerButton::Left,
                ..
            } => {
                // Unconditional: a press and its release can arrive in one
                // event batch, with no rebuild between them, so this widget's
                // `selecting` is still the value from before the press. The
                // application ignores a release it did not start.
                self.emit(CodeInput::Release, sink);
                EventResponse::ReleasePointer
            }

            WidgetEvent::Scroll { delta_y, .. } => {
                if self.line_h <= 0 {
                    return EventResponse::Ignored;
                }
                let lines = -delta_y / self.line_h.max(1);
                let lines = if lines == 0 {
                    if *delta_y > 0 { -1 } else { 1 }
                } else {
                    lines
                };
                self.emit(CodeInput::Scroll { delta_lines: lines }, sink)
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                if !self.focused {
                    return EventResponse::Ignored;
                }
                self.emit(
                    CodeInput::Key {
                        key: *key,
                        modifiers: *modifiers,
                    },
                    sink,
                )
            }

            WidgetEvent::TextInput { character } => {
                if !self.focused {
                    return EventResponse::Ignored;
                }
                self.emit(
                    CodeInput::Text {
                        character: *character,
                    },
                    sink,
                )
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::TextField
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::ClickFocus
    }
}
