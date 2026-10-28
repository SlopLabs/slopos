use std::any::Any;
use std::cell::Cell;

use slopos_abi::draw::Color32;
use slopos_theme_core::{TextRole, Theme, space};

use super::RowState;
use super::text::break_lines;
use crate::components::{ListRow, Wrap};
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

type Pick = Box<dyn Fn(usize) -> Box<dyn Any>>;

enum Entry {
    Heading(String),
    Row(usize),
}

struct Placed {
    rect: Rect,
    /// Lines of the row's detail, or of its reason when it is disabled.
    lines: Vec<(usize, usize)>,
    title_cut: Cell<bool>,
    detail_cut: Cell<bool>,
}

/// Rows in sections, one of which may be chosen; one tab stop.
pub struct ListWidget {
    core: WidgetCore,
    entries: Vec<Entry>,
    rows: Vec<ListRow>,
    placed: Vec<Placed>,
    headings: Vec<(Rect, Cell<bool>)>,
    selected: Option<usize>,
    on_select: Option<Pick>,
    on_activate: Option<Pick>,
    focused: bool,
    /// Paints the selection as if focused.
    active: bool,
}

const PAD: i32 = space::SM;
const TITLE: TextRole = TextRole::Body;
const DETAIL: TextRole = TextRole::Caption;
const HEADING: TextRole = TextRole::Label;

impl ListWidget {
    pub fn new(
        sections: Vec<(String, Vec<ListRow>)>,
        selected: Option<usize>,
        on_select: Option<Pick>,
        on_activate: Option<Pick>,
    ) -> Self {
        let mut entries = Vec::new();
        let mut rows = Vec::new();
        for (heading, section) in sections {
            if !heading.is_empty() {
                entries.push(Entry::Heading(heading));
            }
            for row in section {
                entries.push(Entry::Row(rows.len()));
                rows.push(row);
            }
        }
        Self {
            core: WidgetCore::new(),
            entries,
            rows,
            placed: Vec::new(),
            headings: Vec::new(),
            selected,
            on_select,
            on_activate,
            focused: false,
            active: false,
        }
    }

    pub fn active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    fn icon_room(&self) -> i32 {
        let lead = match self.rows.iter().any(|r| r.avatar) {
            true => super::avatar::ROW.0,
            false if self.rows.iter().any(|r| r.icon.is_some()) => TITLE.style().icon,
            false => return 0,
        };
        lead + space::SM
    }

    fn trailing_room(row: &ListRow) -> i32 {
        if row.trailing.is_empty() {
            0
        } else {
            ui::width(&row.trailing, DETAIL) + space::SM
        }
    }

    fn second_line(row: &ListRow) -> &str {
        row.disabled_reason.as_deref().unwrap_or(&row.detail)
    }

    fn text_width(&self, row: &ListRow, width: i32) -> i32 {
        (width - PAD * 2 - self.icon_room() - Self::trailing_room(row)).max(0)
    }

    /// Lays every entry out from `top` for `width`; answers the height.
    fn lay_out(&mut self, x: i32, top: i32, width: Option<i32>) -> i32 {
        self.placed.clear();
        self.headings.clear();
        let mut y = top;
        for (i, entry) in self.entries.iter().enumerate() {
            match entry {
                Entry::Heading(_) => {
                    if i > 0 {
                        y += space::MD;
                    }
                    let h = HEADING.style().line_height + space::XS;
                    let at = Rect::new(x + PAD, y, width.unwrap_or(0) - PAD * 2, h);
                    self.headings.push((at, Cell::new(false)));
                    y += h;
                }
                Entry::Row(r) => {
                    let row = &self.rows[*r];
                    let text_w = width.map(|w| self.text_width(row, w));
                    let second = Self::second_line(row);
                    let lines = if second.is_empty() {
                        Vec::new()
                    } else if row.detail_tail && row.enabled() {
                        vec![(0, second.len())]
                    } else {
                        break_lines(second, DETAIL, Wrap::Words, text_w)
                    };
                    let h = Self::row_height(lines.len());
                    self.placed.push(Placed {
                        rect: Rect::new(x, y, width.unwrap_or(0), h),
                        lines,
                        title_cut: Cell::new(false),
                        detail_cut: Cell::new(false),
                    });
                    y += h;
                }
            }
        }
        y - top
    }

    pub(crate) const fn row_height(lines: usize) -> i32 {
        PAD * 2 + TITLE.style().line_height + DETAIL.style().line_height * lines as i32
    }

    fn natural_width(&self) -> i32 {
        let rows = self.rows.iter().map(|row| {
            let text = ui::width(&row.title, TITLE).max(ui::width(Self::second_line(row), DETAIL));
            PAD * 2 + self.icon_room() + text + Self::trailing_room(row)
        });
        let headings = self.entries.iter().filter_map(|e| match e {
            Entry::Heading(h) => Some(PAD * 2 + ui::width(h, HEADING)),
            Entry::Row(_) => None,
        });
        rows.chain(headings).max().unwrap_or(0)
    }

    /// Whether anything answers a choice, and there is a row to choose.
    fn choosable(&self) -> bool {
        (self.on_select.is_some() || self.on_activate.is_some())
            && self.rows.iter().any(ListRow::enabled)
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        self.placed.iter().position(|p| p.rect.contains(x, y))
    }

    fn cursor(&self) -> Option<usize> {
        self.selected
            .filter(|&i| i < self.rows.len())
            .or_else(|| self.rows.iter().position(ListRow::enabled))
    }

    fn step(&self, from: Option<usize>, forward: bool) -> Option<usize> {
        super::next_enabled(self.rows.len(), from, forward, |i| self.rows[i].enabled())
    }

    fn pick(&self, row: usize, sink: &mut MessageSink, activate: bool) {
        let f = if activate {
            self.on_activate.as_ref()
        } else {
            self.on_select.as_ref()
        };
        if let Some(f) = f {
            sink.emit_raw(f(row));
        }
    }

    fn inks(theme: &Theme, enabled: bool) -> (Color32, Color32) {
        if enabled {
            (theme.text, theme.text_secondary)
        } else {
            (theme.text_disabled, theme.text_disabled)
        }
    }
}

impl Widget for ListWidget {
    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.row_at(x, y)
            .filter(|&at| self.choosable() && self.rows.get(at).is_some_and(ListRow::enabled))
    }

    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            self.natural_width()
        };
        let height = self.lay_out(0, 0, Some(width));
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        self.lay_out(rect.x, rect.y, Some(rect.width));
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let theme = ctx.theme();
        let mut headings = self.headings.iter();
        let ringed = self
            .cursor()
            .filter(|&r| self.placed.get(r).is_some_and(|p| ctx.shows(p.rect)));
        for entry in &self.entries {
            let Entry::Row(r) = entry else {
                if let (Entry::Heading(text), Some((at, cut))) = (entry, headings.next()) {
                    let ink = theme.text_secondary;
                    cut.set(ctx.draw_text_fitted(at.x, at.y, text, HEADING, at.width, ink));
                }
                continue;
            };
            let (row, placed) = (&self.rows[*r], &self.placed[*r]);
            let rect = placed.rect;
            let state = RowState {
                selected: self.selected == Some(*r),
                hovered: ctx.lit(self.id()) == Some(*r),
                focused: self.focused || self.active,
                cursor: self.focused && ringed == Some(*r),
            };
            super::paint_row(ctx, rect, state);

            let (ink, quiet) = Self::inks(theme, row.enabled());
            let mut x = rect.x + PAD;
            let top = rect.y + PAD;
            if row.avatar {
                let top = top + (TITLE.style().line_height - super::avatar::ROW.0) / 2;
                super::avatar::paint(ctx, &row.title, x, top, super::avatar::ROW);
                x += self.icon_room();
            } else if let Some(icon) = row.icon {
                let size = TITLE.style().icon;
                let icon_top = top + (TITLE.style().line_height - size) / 2;
                ctx.draw_icon(icon, x, icon_top, size, quiet);
                x += self.icon_room();
            }
            let text_w = self.text_width(row, rect.width);
            let title_lh = TITLE.style().line_height;
            let title_top = ui::centred_top(top, title_lh, TITLE);
            let cut = ctx.draw_text_fitted(x, title_top, &row.title, TITLE, text_w, ink);
            placed.title_cut.set(cut);

            let second = Self::second_line(row);
            let detail_lh = DETAIL.style().line_height;
            for (i, &(s, e)) in placed.lines.iter().enumerate() {
                let y = ui::centred_top(top + title_lh + i as i32 * detail_lh, detail_lh, DETAIL);
                let line = &second[s..e];
                if row.detail_tail && row.enabled() {
                    let cut = ctx.draw_text_tail(x, y, line, DETAIL, text_w, quiet);
                    placed.detail_cut.set(cut);
                } else {
                    ctx.draw_text_fitted(x, y, line, DETAIL, text_w, quiet);
                }
            }
            if !row.trailing.is_empty() {
                let w = ui::width(&row.trailing, DETAIL);
                let y = ui::centred_top(top, title_lh, DETAIL);
                ctx.draw_text(
                    rect.x + rect.width - PAD - w,
                    y,
                    &row.trailing,
                    DETAIL,
                    quiet,
                );
            }
        }
        if self.focused && ringed.is_none() {
            ctx.draw_inner_focus_ring(self.layout_rect());
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } => match self.row_at(*x, *y) {
                Some(r) => {
                    if self.rows[r].enabled() {
                        self.pick(r, sink, self.on_select.is_none());
                    }
                    EventResponse::Consumed
                }
                None => EventResponse::Ignored,
            },
            WidgetEvent::KeyDown { key, modifiers, .. } if self.focused && !modifiers.alt => {
                let cursor = self.cursor();
                let target = match key {
                    Key::Named(NamedKey::Down) => self.step(cursor, true),
                    Key::Named(NamedKey::Up) => self.step(cursor, false),
                    Key::Named(NamedKey::Home) => self.step(None, true),
                    Key::Named(NamedKey::End) => self.step(None, false),
                    Key::Named(NamedKey::Enter) if self.on_activate.is_none() => {
                        return EventResponse::Ignored;
                    }
                    Key::Named(key @ (NamedKey::Enter | NamedKey::Space)) => {
                        return match cursor.filter(|&c| self.rows[c].enabled()) {
                            Some(c) => {
                                let opens = *key == NamedKey::Enter || self.on_select.is_none();
                                self.pick(c, sink, opens);
                                EventResponse::Consumed
                            }
                            None => EventResponse::Ignored,
                        };
                    }
                    _ => return EventResponse::Ignored,
                };
                match (target, &self.on_select) {
                    (Some(t), Some(_)) => self.pick(t, sink, false),
                    (Some(t), None) => self.selected = Some(t),
                    (None, _) => {}
                }
                EventResponse::Consumed
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::List
    }

    /// Its first row, which names what it holds.
    fn accessible_name(&self) -> Option<&str> {
        self.rows.first().map(|row| row.title.as_str())
    }

    fn focus_policy(&self) -> FocusPolicy {
        if self.choosable() {
            FocusPolicy::StrongFocus
        } else {
            FocusPolicy::None
        }
    }

    fn focus_rect(&self) -> Rect {
        match self.cursor().and_then(|r| self.placed.get(r)) {
            Some(row) => row.rect,
            None => self.layout_rect(),
        }
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        if let Some(r) = self.row_at(x, y) {
            let (row, placed) = (&self.rows[r], &self.placed[r]);
            let below_title = y >= placed.rect.y + PAD + TITLE.style().line_height;
            let detail = (below_title && placed.detail_cut.get()).then_some(row.detail.as_str());
            let title = placed.title_cut.get().then_some(row.title.as_str());
            return detail.or(title);
        }
        let headings = self.entries.iter().filter_map(|e| match e {
            Entry::Heading(text) => Some(text.as_str()),
            Entry::Row(_) => None,
        });
        let (text, (_, cut)) = headings
            .zip(&self.headings)
            .find(|(_, (at, _))| at.contains(x, y))?;
        cut.get().then_some(text)
    }
}
