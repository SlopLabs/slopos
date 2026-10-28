//! The code surface: a gutter, the lines in view, a caret and a selection,
//! reporting in document coordinates so the buffer stays the app's.

use std::any::Any;

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, State, TextRole, space, with_alpha};

use super::scrollbar::Bar;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, Modifiers, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::style::StyleSheet;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

const GUTTER_PAD: i32 = space::SM;
const TEXT_PAD: i32 = space::SM;

/// A coloured run within a line, in character indices.
#[derive(Clone, Copy, Debug)]
pub struct StyledSpan {
    pub start: usize,
    pub end: usize,
    pub color: Color32,
}

#[derive(Clone, Debug)]
pub struct CodeLine {
    /// From 0; the gutter counts from 1.
    pub number: usize,
    pub text: String,
    pub spans: Vec<StyledSpan>,
    /// Search hits, in character indices.
    pub highlights: Vec<(usize, usize)>,
}

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
    /// The scrollbar was pressed or dragged.
    ScrollTo {
        first_line: usize,
    },
    Key {
        key: Key,
        modifiers: Modifiers,
    },
    Text {
        character: char,
    },
}

/// Lines that fit in `height` pixels, which the app sends.
pub fn visible_line_count(height: i32, line_height: i32) -> usize {
    if line_height <= 0 {
        return 0;
    }
    (height / line_height).max(0) as usize
}

/// What of the width beside the gutter is not text, which the app takes off
/// as the widget does: the gap after the gutter and the scrollbar.
pub fn text_area_reserved() -> i32 {
    TEXT_PAD + StyleSheet::current().scrollbar_width
}

pub fn gutter_width(total_lines: usize, cell_width: i32, show_line_numbers: bool) -> i32 {
    if !show_line_numbers {
        return TEXT_PAD;
    }
    let digits = total_lines.max(1).ilog10() as i32 + 1;
    GUTTER_PAD * 2 + digits * cell_width
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

/// The character column holding display column `display`, the inverse of
/// [`display_col`], which editor-core's `char_col_from_display` must match.
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

/// The character column a click `half_display` half-cells in lands on: after
/// a character past its middle, which whole cells could never tell.
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
    /// Given by the app: a rebuild comes between a press and its moves.
    selecting: bool,
    scroll_dragging: bool,
    on_input: Option<InputCallback>,
    cell_w: i32,
    line_h: i32,
    bar: i32,
    thumb_min: i32,
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
        scroll_dragging: bool,
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
            scroll_dragging,
            on_input,
            cell_w: crate::text::cell_width(),
            line_h: crate::text::cell_height(),
            bar: 0,
            thumb_min: 0,
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

    /// Clamped into the rows in view.
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
            // On a line's number, the start of the line, scrolled or not.
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

    /// In display columns.
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

    /// Washes display columns `start..end` of the row at `y`.
    fn wash(
        &self,
        ctx: &mut PaintContext,
        content: Rect,
        y: i32,
        span: (usize, usize),
        ink: Color32,
    ) {
        let x = |col: usize| {
            self.text_origin_x() + col.saturating_sub(self.first_col) as i32 * self.cell_w
        };
        let (x0, x1) = (x(span.0), x(span.1));
        if span.1 > self.first_col && x1 > x0 {
            ctx.with_viewport(content, |ctx| {
                ctx.fill_rect_blended(x0, y, x1 - x0, self.line_h, ink);
            });
        }
    }

    fn paint_line_text(&self, ctx: &mut PaintContext, line: &CodeLine, y: i32, clip: Rect) {
        let default_fg = ctx.theme().text_secondary;
        let mut display = 0usize;
        let right_edge = clip.x + clip.width;

        // A run of one colour is one draw; a tab ends one, drawing nothing.
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
                        ctx.draw_text(run_x, y, &run, TextRole::Mono, run_color);
                        run.clear();
                    }
                } else {
                    if run.is_empty() {
                        run_x = cell_x;
                        run_color = color;
                    } else if color != run_color {
                        ctx.draw_text(run_x, y, &run, TextRole::Mono, run_color);
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
            ctx.draw_text(run_x, y, &run, TextRole::Mono, run_color);
        }
    }

    fn scrollbar(&self) -> Option<Bar> {
        let rect = self.layout_rect();
        let shown = visible_line_count(rect.height, self.line_h).max(1) as i64;
        let (total, first) = (self.total_lines as i64, self.first_line as i64);
        Bar::needed(rect, self.bar, self.thumb_min, shown, total, first)
    }

    fn first_line_at(&self, y: i32) -> usize {
        self.scrollbar().map_or(0, |bar| bar.offset_at(y)) as usize
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

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.cell_w = crate::text::cell_width();
        self.line_h = crate::text::cell_height();
        self.bar = ctx.style.scrollbar_width;
        self.thumb_min = ctx.style.scrollbar_thumb_min;
        constraints.constrain(constraints.max_size())
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, theme.surface);

        let current_line = theme.state(ColorRole::Surface, State::Hover);
        let found = with_alpha(theme.warning, 0x4d);
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
                ctx.fill_rect(rect.x, y, rect.width, self.line_h, current_line);
            }

            if let Some(span) = self.selection_on_line(line.number, &line.text) {
                self.wash(ctx, content, y, span, theme.selection);
            }
            for &(start, end) in &line.highlights {
                let display = |col| display_col(&line.text, col, self.tab_width);
                self.wash(ctx, content, y, (display(start), display(end)), found);
            }

            if self.show_line_numbers {
                let number = format!("{}", line.number + 1);
                let width = number.chars().count() as i32 * self.cell_w;
                let nx = rect.x + gutter - GUTTER_PAD - width;
                let color = if is_cursor_line {
                    theme.text
                } else {
                    theme.text_disabled
                };
                ctx.draw_text(nx, y, &number, TextRole::Mono, color);
            }

            ctx.with_viewport(content, |ctx| {
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
                        // Hollow without focus: where it is, not claiming keys.
                        if self.focused {
                            ctx.with_viewport(content, |ctx| {
                                ctx.fill_rect(x, y, super::CARET, self.line_h, theme.accent);
                            });
                        } else {
                            ctx.with_viewport(content, |ctx| {
                                ctx.draw_rect(x, y, self.cell_w, self.line_h, theme.text_disabled);
                            });
                        }
                    }
                }
            }
        }

        if let Some(bar) = self.scrollbar() {
            bar.paint(ctx, self.id(), self.scroll_dragging);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
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
                if self.scrollbar().is_some_and(|bar| bar.grabs(*x, *y)) {
                    let first_line = self.first_line_at(*y);
                    self.emit(CodeInput::ScrollTo { first_line }, sink);
                    return EventResponse::Consumed;
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
                EventResponse::Consumed
            }

            WidgetEvent::PointerMove { x, y } => {
                if self.scroll_dragging {
                    let first_line = self.first_line_at(*y);
                    return self.emit(CodeInput::ScrollTo { first_line }, sink);
                }
                if !self.selecting {
                    return EventResponse::Ignored;
                }
                let (line, col) = self.position_at(*x, *y);
                self.emit(CodeInput::Drag { line, col }, sink)
            }

            // Sent whether or not a drag is live: a press and its release can
            // come in one batch, before the rebuild that says one began.
            WidgetEvent::PointerUp {
                button: PointerButton::Left,
                ..
            } => {
                self.emit(CodeInput::Release, sink);
                EventResponse::Consumed
            }

            WidgetEvent::Scroll { delta_y, .. } if self.line_h > 0 && *delta_y != 0 => {
                let lines = match delta_y / self.line_h {
                    0 => delta_y.signum(),
                    lines => lines,
                };
                self.emit(CodeInput::Scroll { delta_lines: lines }, sink)
            }

            WidgetEvent::KeyDown { key, modifiers, .. } if self.focused => self.emit(
                CodeInput::Key {
                    key: *key,
                    modifiers: *modifiers,
                },
                sink,
            ),

            WidgetEvent::TextInput { character } if self.focused => self.emit(
                CodeInput::Text {
                    character: *character,
                },
                sink,
            ),

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::TextField
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.scrollbar().and_then(|bar| bar.hover(x, y))
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::ClickFocus
    }

    fn declares_focus(&self) -> bool {
        self.focused
    }
}
