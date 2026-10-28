//! A virtualized tree, as a file sidebar shows one: the app owns the tree, its
//! expansion and its scroll, and hands over the rows in view.

use std::any::Any;
use std::cell::Cell;

use slopos_theme_core::{Icon, TextRole, space};

use super::RowState;
use super::scrollbar::Bar;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, Modifiers, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

const LABEL: TextRole = TextRole::Body;
const PAD: i32 = space::SM;

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub label: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// The file open in the editor, in full ink.
    pub active: bool,
    /// Unsaved, which a dot marks.
    pub modified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeInput {
    /// A click on the row itself: open a file, or select a directory.
    Activate {
        row: usize,
    },
    /// A click on the disclosure chevron.
    Toggle {
        row: usize,
    },
    Scroll {
        delta_rows: i32,
    },
    /// The scrollbar was pressed or dragged.
    ScrollTo {
        first_row: usize,
    },
    /// Ends a scrollbar drag.
    Release,
    Key {
        key: Key,
        modifiers: Modifiers,
    },
}

type InputCallback = Box<dyn Fn(TreeInput) -> Box<dyn Any>>;

pub struct TreeViewWidget {
    core: WidgetCore,
    rows: Vec<TreeRow>,
    /// The index of `rows[0]` among all the rows.
    first_row: usize,
    total_rows: usize,
    selected: Option<usize>,
    focused: bool,
    /// Given by the app: a rebuild comes between a press and its moves.
    scroll_dragging: bool,
    row_height: i32,
    bar: i32,
    thumb_min: i32,
    on_input: Option<InputCallback>,
    cut: Vec<Cell<bool>>,
}

impl TreeViewWidget {
    pub fn new(
        rows: Vec<TreeRow>,
        first_row: usize,
        total_rows: usize,
        selected: Option<usize>,
        focused: bool,
        scroll_dragging: bool,
        on_input: Option<InputCallback>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            first_row,
            total_rows,
            selected,
            focused,
            scroll_dragging,
            row_height: 0,
            bar: 0,
            thumb_min: 0,
            cut: rows.iter().map(|_| Cell::new(false)).collect(),
            rows,
            on_input,
        }
    }

    fn icon() -> i32 {
        LABEL.style().icon
    }

    /// A level deeper sets a row's chevron under its parent's folder.
    fn indent() -> i32 {
        Self::icon() + space::XS
    }

    fn visible(&self) -> usize {
        (self.layout_rect().height / self.row_height.max(1)).max(1) as usize
    }

    fn scrollbar(&self) -> Option<Bar> {
        let (shown, total) = (self.visible() as i64, self.total_rows as i64);
        let first = self.first_row as i64;
        Bar::needed(
            self.layout_rect(),
            self.bar,
            self.thumb_min,
            shown,
            total,
            first,
        )
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        let rect = self.layout_rect();
        if self.row_height <= 0 || y < rect.y || y >= rect.y + rect.height {
            return None;
        }
        let index = ((y - rect.y) / self.row_height) as usize;
        (index < self.rows.len()).then(|| self.first_row + index)
    }

    /// Beside the scrollbar, so its focus ring shows all round.
    fn row_rect(&self, index: usize) -> Rect {
        let rect = self.layout_rect();
        let y = rect.y + index as i32 * self.row_height;
        let bar = self.scrollbar().map_or(0, |_| self.bar);
        Rect::new(rect.x, y, rect.width - bar, self.row_height)
    }

    fn chevron_x(&self, depth: usize) -> i32 {
        self.layout_rect().x + PAD + depth as i32 * Self::indent()
    }

    fn on_chevron(&self, x: i32, depth: usize) -> bool {
        let start = self.chevron_x(depth);
        x >= start - space::XS && x < start + Self::indent()
    }

    /// Where the label of a row at `depth` starts, and the room it has before
    /// the dot that marks unsaved changes.
    fn label_room(&self, row: Rect, depth: usize) -> (i32, i32) {
        let x = self.chevron_x(depth) + Self::indent() + Self::icon() + space::SM;
        let end = row.x + row.width - PAD - Self::icon() - space::XS;
        (x, end - x)
    }

    fn emit(&self, input: TreeInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }

    fn paint_rows(&self, ctx: &mut PaintContext) {
        let theme = ctx.theme();
        let icon = Self::icon();
        for (index, row) in self.rows.iter().enumerate() {
            let at = self.row_rect(index);
            let absolute = self.first_row + index;
            let state = RowState {
                selected: self.selected == Some(absolute),
                hovered: ctx.lit(self.id()) == Some(absolute),
                focused: self.focused,
                cursor: self.selected == Some(absolute),
            };
            super::paint_row(ctx, at, state);

            let icon_y = at.y + (at.height - icon) / 2;
            let x = self.chevron_x(row.depth);
            if row.is_dir {
                let chevron = match row.expanded {
                    true => Icon::ChevronDown,
                    false => Icon::ChevronRight,
                };
                ctx.draw_icon(chevron, x, icon_y, icon, theme.text_secondary);
            }
            let (glyph, tint) = match (row.is_dir, row.expanded) {
                (true, true) => (Icon::FolderOpen, theme.accent),
                (true, false) => (Icon::Folder, theme.accent),
                (false, _) => (Icon::File, theme.text_secondary),
            };
            ctx.draw_icon(glyph, x + Self::indent(), icon_y, icon, tint);

            let ink = match row.active || row.is_dir {
                true => theme.text,
                false => theme.text_secondary,
            };
            let (label_x, room) = self.label_room(at, row.depth);
            let top = ui::centred_top(at.y, at.height, LABEL);
            let cut = ctx.draw_text_tail(label_x, top, &row.label, LABEL, room, ink);
            self.cut[index].set(cut);
            if row.modified {
                let dot_x = label_x + room + space::XS;
                super::modified_dot(ctx, dot_x, icon_y, icon, theme.accent);
            }
        }
        if let Some(bar) = self.scrollbar() {
            bar.paint(ctx, self.id(), self.scroll_dragging);
        }
    }
}

impl Widget for TreeViewWidget {
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
        constraints.constrain(constraints.max_size())
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        ctx.with_viewport(rect, |ctx| self.paint_rows(ctx));
        let shown = self.first_row..self.first_row + self.rows.len();
        if self.focused && !self.selected.is_some_and(|row| shown.contains(&row)) {
            ctx.draw_inner_focus_ring(rect);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) || *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                if let Some(bar) = self.scrollbar().filter(|bar| bar.grabs(*x, *y)) {
                    let first_row = bar.offset_at(*y) as usize;
                    return self.emit(TreeInput::ScrollTo { first_row }, sink);
                }
                let Some(row) = self.row_at(*y) else {
                    return EventResponse::Ignored;
                };
                let entry = &self.rows[row - self.first_row];
                if entry.is_dir && self.on_chevron(*x, entry.depth) {
                    return self.emit(TreeInput::Toggle { row }, sink);
                }
                self.emit(TreeInput::Activate { row }, sink)
            }

            WidgetEvent::PointerMove { y, .. } if self.scroll_dragging => {
                let first_row = self.scrollbar().map_or(0, |bar| bar.offset_at(*y));
                self.emit(
                    TreeInput::ScrollTo {
                        first_row: first_row as usize,
                    },
                    sink,
                )
            }

            // Sent whether or not a drag is live: a press and its release can
            // come in one batch, before the rebuild that says one began.
            WidgetEvent::PointerUp { .. } => self.emit(TreeInput::Release, sink),

            WidgetEvent::Scroll { delta_y, .. } if *delta_y != 0 => {
                let rows = match delta_y / self.row_height.max(1) {
                    0 => delta_y.signum(),
                    rows => rows,
                };
                self.emit(TreeInput::Scroll { delta_rows: rows }, sink)
            }

            WidgetEvent::KeyDown { key, modifiers, .. } if self.focused => self.emit(
                TreeInput::Key {
                    key: *key,
                    modifiers: *modifiers,
                },
                sink,
            ),

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
        self.row_at(y)
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::ClickFocus
    }

    fn declares_focus(&self) -> bool {
        self.focused
    }

    fn tooltip(&self, _x: i32, y: i32) -> Option<&str> {
        let index = self.row_at(y)? - self.first_row;
        self.cut[index]
            .get()
            .then_some(self.rows[index].label.as_str())
    }
}
