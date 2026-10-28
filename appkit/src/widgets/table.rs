use std::any::Any;
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use slopos_theme_core::{Icon, TextRole, space};

use super::RowState;
use super::scrollbar::Bar;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::node::{ContextMenuAt, SortIndicator, TableColumn, TableColumnWidth};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{
    FocusPolicy, Kept, KeptAt, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

const CELL_PAD: i32 = space::SM;
const HEADING: TextRole = TextRole::Body;

/// Body text's widest digit, measured over a run of each, since a lone
/// digit's width is rounded up to a whole pixel.
fn widest_digit() -> &'static str {
    static DIGIT: OnceLock<String> = OnceLock::new();
    DIGIT.get_or_init(|| {
        let run = |digit: &String| ui::width(&digit.repeat(10), TextRole::Body);
        let digits = (0..10).map(|digit: u8| digit.to_string());
        digits.max_by_key(run).unwrap_or_default()
    })
}

fn fitting_width<'a>(heading: &str, values: impl IntoIterator<Item = &'a str>) -> i32 {
    let arrow = HEADING.style().icon + space::XS;
    let shape = |value: &str| value.replace(|c: char| c.is_ascii_digit(), widest_digit());
    let widest = values
        .into_iter()
        .map(|value| ui::width(&shape(value), TextRole::Body))
        .max()
        .unwrap_or(0);
    (ui::width(heading, HEADING) + arrow).max(widest) + 2 * CELL_PAD
}

impl TableColumnWidth {
    /// Fixed as wide as `heading` beside a sort arrow or the widest of
    /// `values` in body text, whichever is wider, each digit of a value
    /// measured as the widest digit so any value of its shape fits.
    pub fn fitting<'a>(heading: &str, values: impl IntoIterator<Item = &'a str>) -> Self {
        Self::Fixed(fitting_width(heading, values))
    }
}

impl TableColumn {
    /// The least width a table of `columns` lays out at: each fixed column
    /// whole, and each flexible one as wide as a name of a dozen letters,
    /// beside its scrollbar.
    pub fn least_width(columns: &[TableColumn]) -> i32 {
        let name = "n".repeat(12);
        let columns: i32 = columns
            .iter()
            .map(|column| match column.width {
                TableColumnWidth::Fixed(px) => px,
                TableColumnWidth::Flex(_) => fitting_width(&column.label, [name.as_str()]),
            })
            .sum();
        columns + crate::style::StyleSheet::current().scrollbar_width
    }
}

type Emit<A> = Box<dyn Fn(A) -> Box<dyn Any>>;

/// Rows of cells under column headings, all one height; only the rows in
/// view are painted.
pub struct TableWidget {
    core: WidgetCore,
    columns: Vec<TableColumn>,
    rows: Vec<Vec<Box<dyn Widget>>>,
    row_height: i32,
    selected: Option<usize>,
    on_select: Option<Emit<usize>>,
    on_activate: Option<Emit<usize>>,
    on_header_click: Option<Emit<usize>>,
    on_context_menu: Option<Emit<ContextMenuAt>>,
    scroll_offset: i32,
    shows: u64,
    col_widths: Vec<i32>,
    headings_cut: Vec<Cell<bool>>,
    focused: bool,
    /// Kept here, unlike a code view's: this widget scrolls itself and emits
    /// nothing, so no rebuild comes between a press and its moves.
    thumb_dragging: bool,
    bar: i32,
    thumb_min: i32,
}

impl TableWidget {
    pub fn new(columns: Vec<TableColumn>, rows: Vec<Vec<Box<dyn Widget>>>) -> Self {
        debug_assert!(
            rows.iter().all(|row| row.len() == columns.len()),
            "a table row has other than one cell per column"
        );
        Self {
            core: WidgetCore::new(),
            col_widths: vec![0; columns.len()],
            headings_cut: columns.iter().map(|_| Cell::new(false)).collect(),
            columns,
            rows,
            row_height: 0,
            selected: None,
            on_select: None,
            on_activate: None,
            on_header_click: None,
            on_context_menu: None,
            scroll_offset: 0,
            shows: 0,
            focused: false,
            thumb_dragging: false,
            bar: 0,
            thumb_min: 0,
        }
    }

    pub fn selected(mut self, selected: Option<usize>) -> Self {
        self.selected = selected;
        self
    }

    pub fn on_select(mut self, f: Option<Emit<usize>>) -> Self {
        self.on_select = f;
        self
    }

    /// Emitted by Enter and by a click on the row already chosen.
    pub fn on_activate(mut self, f: Option<Emit<usize>>) -> Self {
        self.on_activate = f;
        self
    }

    pub fn on_header_click(mut self, f: Option<Emit<usize>>) -> Self {
        self.on_header_click = f;
        self
    }

    pub fn on_context_menu(mut self, f: Option<Emit<ContextMenuAt>>) -> Self {
        self.on_context_menu = f;
        self
    }

    /// What the rows are of; see [`Node::Table`](crate::Node::Table).
    pub fn showing(mut self, shows: u64) -> Self {
        self.shows = shows;
        self
    }

    fn header_height(&self) -> i32 {
        self.row_height
    }

    fn body(&self) -> Rect {
        let rect = self.layout_rect();
        let top = rect.y + self.header_height();
        Rect::new(
            rect.x,
            top,
            rect.width,
            (rect.height - self.header_height()).max(0),
        )
    }

    fn content_height(&self) -> i32 {
        self.rows.len() as i32 * self.row_height
    }

    fn max_scroll_offset(&self) -> i32 {
        (self.content_height() - self.body().height).max(0)
    }

    fn row_top(&self, row: usize) -> i32 {
        self.body().y + row as i32 * self.row_height - self.scroll_offset
    }

    fn row_rect(&self, row: usize) -> Rect {
        let rect = self.layout_rect();
        let width = self.rows_width(rect.width, rect.height);
        Rect::new(rect.x, self.row_top(row), width, self.row_height)
    }

    /// How wide the rows are in a table `width` by `height`: beside the
    /// scrollbar while they overflow, so a row's focus ring shows all round.
    fn rows_width(&self, width: i32, height: i32) -> i32 {
        let body = height - self.header_height();
        let overflows = body > 0 && self.content_height() > body;
        width - if overflows { self.bar } else { 0 }
    }

    /// The row the keyboard is on: the chosen one, else the first.
    fn cursor(&self) -> Option<usize> {
        self.selected.or((!self.rows.is_empty()).then_some(0))
    }

    /// Where focus shows: round the cursor's row while all of it is in view,
    /// else round the whole table.
    fn ringed_row(&self) -> Option<usize> {
        let body = self.body();
        let in_view = |r: Rect| r.y >= body.y && r.y + r.height <= body.y + body.height;
        self.cursor().filter(|&row| in_view(self.row_rect(row)))
    }

    fn resolve_col_widths(&mut self, available: i32) {
        let (fixed, flex) =
            self.columns
                .iter()
                .fold((0, 0u16), |(fixed, flex), col| match col.width {
                    TableColumnWidth::Fixed(px) => (fixed + px, flex),
                    TableColumnWidth::Flex(weight) => (fixed, flex + weight),
                });
        let room = (available - fixed).max(0) as i64;
        self.col_widths = self
            .columns
            .iter()
            .map(|col| match col.width {
                TableColumnWidth::Fixed(px) => px,
                TableColumnWidth::Flex(_) if flex == 0 => 0,
                TableColumnWidth::Flex(weight) => (room * weight as i64 / flex as i64) as i32,
            })
            .collect();
    }

    fn scroll_to_selected(&mut self) {
        let Some(row) = self.selected else {
            return;
        };
        let top = row as i32 * self.row_height;
        let bottom = top + self.row_height;
        let body = self.body().height;
        if top < self.scroll_offset {
            self.scroll_offset = top;
        } else if bottom > self.scroll_offset + body {
            self.scroll_offset = bottom - body;
        }
        self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
    }

    fn column_at_x(&self, x: i32) -> Option<usize> {
        let mut left = self.layout_rect().x;
        self.col_widths.iter().position(|&w| {
            left += w;
            x >= left - w && x < left
        })
    }

    fn visible_range(&self) -> (usize, usize) {
        if self.row_height <= 0 {
            return (0, 0);
        }
        let start = (self.scroll_offset / self.row_height) as usize;
        let end = (self.scroll_offset + self.body().height) / self.row_height + 1;
        (start, (end as usize).min(self.rows.len()))
    }

    /// Rows in view, which is how far PageUp and PageDown move.
    fn page_size(&self) -> usize {
        (self.body().height / self.row_height.max(1)).max(1) as usize
    }

    fn row_at_y(&self, y: i32) -> Option<usize> {
        let body = self.body();
        if self.row_height <= 0 || y < body.y || y >= body.y + body.height {
            return None;
        }
        let index = ((y - body.y + self.scroll_offset) / self.row_height) as usize;
        (index < self.rows.len()).then_some(index)
    }

    /// Emits only a change, so a repeated key or click does not churn the
    /// app's state.
    fn select_row(&mut self, row: usize, sink: &mut MessageSink) {
        let changed = self.selected != Some(row);
        self.selected = Some(row);
        self.scroll_to_selected();
        self.place_cells();
        if let Some(cb) = self.on_select.as_ref().filter(|_| changed) {
            sink.emit_raw(cb(row));
        }
    }

    fn activate(&mut self, row: usize, sink: &mut MessageSink) -> EventResponse {
        match &self.on_activate {
            Some(cb) => {
                sink.emit_raw(cb(row));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }

    /// Where a menu for `row` opens: its bottom-left, kept in the body.
    fn row_anchor(&self, row: usize) -> (i32, i32) {
        let body = self.body();
        let y = self.row_top(row) + self.row_height;
        (body.x, y.clamp(body.y, body.y + body.height))
    }

    fn scrollbar(&self) -> Option<Bar> {
        let body = self.body();
        let total = self.content_height() as i64;
        let (shown, offset) = (body.height as i64, self.scroll_offset as i64);
        Bar::needed(body, self.bar, self.thumb_min, shown, total, offset)
    }

    fn scroll_to_thumb(&mut self, bar: Bar, y: i32) {
        let offset = (bar.offset_at(y) as i32).clamp(0, self.max_scroll_offset());
        if offset != self.scroll_offset {
            self.scroll_offset = offset;
            self.place_cells();
        }
    }

    fn emit_context_menu(&self, row: usize, x: i32, y: i32, sink: &mut MessageSink) -> bool {
        match &self.on_context_menu {
            Some(cb) => {
                sink.emit_raw(cb(ContextMenuAt { row, x, y }));
                true
            }
            None => false,
        }
    }

    fn place_cells(&mut self) {
        let left = self.layout_rect().x;
        let top = self.row_top(0);
        for (index, row) in self.rows.iter_mut().enumerate() {
            let y = top + index as i32 * self.row_height;
            let mut x = left;
            for (cell, &w) in row.iter_mut().zip(&self.col_widths) {
                let at = Rect::new(x + CELL_PAD, y, (w - CELL_PAD * 2).max(0), self.row_height);
                place_widget(cell.as_mut(), at);
                x += w;
            }
        }
    }

    /// Answers whether the label was cut short.
    fn paint_heading(&self, ctx: &mut PaintContext, col: &TableColumn, x: i32, width: i32) -> bool {
        let theme = ctx.theme();
        let top = ui::centred_top(self.layout_rect().y, self.header_height(), HEADING);
        let icon = HEADING.style().icon;
        let sort = col.sort_indicator.map(|sort| match sort {
            SortIndicator::Ascending => Icon::ChevronUp,
            SortIndicator::Descending => Icon::ChevronDown,
        });
        let room = width - CELL_PAD * 2 - sort.map_or(0, |_| icon + space::XS);
        let x = x + CELL_PAD;
        let cut = ctx.draw_text_fitted(x, top, &col.label, HEADING, room, theme.text);
        if let Some(sort) = sort {
            let after = x + ui::elided_width(&col.label, HEADING, room) + space::XS;
            let y = self.layout_rect().y + (self.header_height() - icon) / 2;
            ctx.draw_icon(sort, after, y, icon, theme.text_secondary);
        }
        cut
    }

    fn key(&mut self, key: Key, sink: &mut MessageSink) -> EventResponse {
        let last = self.rows.len() - 1;
        let target = match (key, self.selected) {
            (Key::Named(NamedKey::Up), Some(0)) => return EventResponse::Ignored,
            (Key::Named(NamedKey::Up), Some(row)) => row - 1,
            (Key::Named(NamedKey::Down), Some(row)) if row >= last => {
                return EventResponse::Ignored;
            }
            (Key::Named(NamedKey::Down), Some(row)) => row + 1,
            (Key::Named(NamedKey::Up | NamedKey::Down | NamedKey::Home), _) => 0,
            (Key::Named(NamedKey::End), _) => last,
            (Key::Named(NamedKey::PageUp), row) => {
                row.unwrap_or(0).saturating_sub(self.page_size())
            }
            (Key::Named(NamedKey::PageDown), row) => {
                (row.unwrap_or(0) + self.page_size()).min(last)
            }
            (Key::Named(NamedKey::Enter), Some(row)) => return self.activate(row, sink),
            _ => return EventResponse::Ignored,
        };
        self.select_row(target, sink);
        EventResponse::Consumed
    }
}

impl Widget for TableWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.row_height = ctx.style.row_height;
        self.bar = ctx.style.scrollbar_width;
        self.thumb_min = ctx.style.scrollbar_thumb_min;
        let size = constraints.max_size();
        self.resolve_col_widths(self.rows_width(size.width, size.height));
        for row in &mut self.rows {
            for (cell, &w) in row.iter_mut().zip(&self.col_widths) {
                let size = Size::new((w - CELL_PAD * 2).max(0), self.row_height);
                measure_widget(cell.as_mut(), BoxConstraints::tight(size), ctx);
            }
        }
        constraints.max_size()
    }

    fn layout(&mut self, rect: Rect) {
        self.resolve_col_widths(self.rows_width(rect.width, rect.height));
        self.scroll_offset = self.scroll_offset.clamp(0, self.max_scroll_offset());
        self.place_cells();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let theme = ctx.theme();
        let rect = self.layout_rect();
        let header = self.header_height();

        ctx.fill_rect(rect.x, rect.y, rect.width, header, theme.surface_raised);
        let mut x = rect.x;
        let headings = self.columns.iter().zip(&self.col_widths);
        for ((col, &w), cut) in headings.zip(&self.headings_cut) {
            cut.set(self.paint_heading(ctx, col, x, w));
            x += w;
        }
        ctx.fill_rect(rect.x, rect.y + header - 1, rect.width, 1, theme.border);

        let (first, end) = self.visible_range();
        let ringed = self.ringed_row();
        ctx.with_viewport(self.body(), |ctx| {
            for i in first..end {
                let row = self.row_rect(i);
                ctx.fill_rect(row.x, row.y, row.width, row.height, theme.surface);
                let state = RowState {
                    selected: self.selected == Some(i),
                    hovered: ctx.lit(self.id()) == Some(i),
                    focused: self.focused,
                    cursor: ringed == Some(i),
                };
                super::paint_row(ctx, row, state);
                for cell in &self.rows[i] {
                    cell.paint(ctx);
                }
            }
        });

        if let Some(bar) = self.scrollbar() {
            bar.paint(ctx, self.id(), self.thumb_dragging);
        }
        if self.focused && ringed.is_none() {
            ctx.draw_inner_focus_ring(rect);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, button, .. } => {
                let rect = self.layout_rect();
                if !rect.contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                let bar = self.scrollbar().filter(|bar| bar.grabs(*x, *y));
                if let Some(bar) = bar {
                    if *button != PointerButton::Left {
                        return EventResponse::Ignored;
                    }
                    self.thumb_dragging = true;
                    self.scroll_to_thumb(bar, *y);
                    return EventResponse::Consumed;
                }
                if *y < rect.y + self.header_height() {
                    // A secondary click on a heading addresses no row.
                    if *button != PointerButton::Left {
                        return EventResponse::Ignored;
                    }
                    if let (Some(cb), Some(col)) = (&self.on_header_click, self.column_at_x(*x)) {
                        sink.emit_raw(cb(col));
                    }
                    return EventResponse::Consumed;
                }
                let Some(index) = self.row_at_y(*y) else {
                    return EventResponse::Ignored;
                };
                match button {
                    PointerButton::Left if self.selected == Some(index) => {
                        self.activate(index, sink);
                        EventResponse::Consumed
                    }
                    PointerButton::Left => {
                        self.select_row(index, sink);
                        EventResponse::Consumed
                    }
                    // Selects first: the menu acts on the selection, so the
                    // two must never disagree.
                    PointerButton::Right => {
                        self.select_row(index, sink);
                        match self.emit_context_menu(index, *x, *y, sink) {
                            true => EventResponse::Consumed,
                            false => EventResponse::Ignored,
                        }
                    }
                    PointerButton::Middle => EventResponse::Ignored,
                }
            }

            WidgetEvent::PointerUp { .. } if self.thumb_dragging => {
                self.thumb_dragging = false;
                EventResponse::Consumed
            }

            WidgetEvent::PointerMove { y, .. } if self.thumb_dragging => {
                if let Some(bar) = self.scrollbar() {
                    self.scroll_to_thumb(bar, *y);
                }
                EventResponse::Consumed
            }

            WidgetEvent::Scroll { delta_y, .. } => {
                let old = self.scroll_offset;
                self.scroll_offset = (old + *delta_y).clamp(0, self.max_scroll_offset());
                if self.scroll_offset == old {
                    return EventResponse::Ignored;
                }
                self.place_cells();
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, modifiers, .. }
                if self.focused && !modifiers.alt && !self.rows.is_empty() =>
            {
                // Shift+F10 stands in for a keyboard without a Menu key.
                let menu_key = matches!(key, Key::Named(NamedKey::Menu))
                    || (matches!(key, Key::Named(NamedKey::F10)) && modifiers.shift);
                if !menu_key {
                    return self.key(*key, sink);
                }
                let Some(row) = self.selected else {
                    return EventResponse::Ignored;
                };
                let (ax, ay) = self.row_anchor(row);
                match self.emit_context_menu(row, ax, ay, sink) {
                    true => EventResponse::Consumed,
                    false => EventResponse::Ignored,
                }
            }

            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::List
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        if let Some(part) = self.scrollbar().and_then(|bar| bar.hover(x, y)) {
            return Some(part);
        }
        self.row_at_y(y)
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    fn focus_rect(&self) -> Rect {
        match self.ringed_row() {
            Some(row) => self.row_rect(row),
            None => self.layout_rect(),
        }
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        if y < self.body().y {
            let col = self.column_at_x(x)?;
            return self.headings_cut[col]
                .get()
                .then_some(self.columns[col].label.as_str());
        }
        self.rows
            .iter()
            .flatten()
            .find_map(|cell| crate::tooltip::answer(cell.as_ref(), x, y))
            .flatten()
    }

    fn kept(&self) -> Option<Kept> {
        let mut shows = std::hash::DefaultHasher::new();
        for column in &self.columns {
            column.label.hash(&mut shows);
        }
        self.shows.hash(&mut shows);
        Some(Kept {
            shows: shows.finish(),
            at: KeptAt::Scroll(self.scroll_offset),
        })
    }

    fn keep(&mut self, at: KeptAt) {
        if let KeptAt::Scroll(y) = at {
            self.scroll_offset = y;
        }
    }
}
