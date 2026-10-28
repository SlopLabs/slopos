use std::any::Any;
use std::cell::Cell;
use std::hash::{Hash, Hasher};

use slopos_theme_core::{TextRole, radius, space};

use super::RowState;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::node::{MenuItem, MenuItemKind};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, Kept, KeptAt, MeasureCtx, Role, Widget, WidgetCore};

const PAD: i32 = space::MD;

/// A menu's items, which an enclosing [`super::popup::PopupWidget`] places.
pub struct MenuWidget {
    core: WidgetCore,
    items: Vec<MenuItem>,
    on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    cursor: Option<usize>,
    focused: bool,
    item_height: i32,
    cut: Vec<Cell<bool>>,
}

impl MenuWidget {
    pub fn new(
        items: Vec<MenuItem>,
        on_action: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            cut: items.iter().map(|_| Cell::new(false)).collect(),
            items,
            on_action,
            cursor: None,
            focused: false,
            item_height: 0,
        }
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        let rect = self.layout_rect();
        if self.item_height <= 0 || !rect.contains(x, y) {
            return None;
        }
        let at = ((y - rect.y) / self.item_height) as usize;
        (at < self.items.len()).then_some(at)
    }

    fn item_rect(&self, at: usize) -> Rect {
        let rect = self.layout_rect();
        let y = rect.y + at as i32 * self.item_height;
        Rect::new(rect.x, y, rect.width, self.item_height)
    }

    fn actionable(&self, at: usize) -> bool {
        self.items
            .get(at)
            .is_some_and(|i| matches!(i.kind, MenuItemKind::Action) && i.enabled)
    }

    fn choose(&self, at: usize, sink: &mut MessageSink) -> EventResponse {
        if let Some(cb) = &self.on_action {
            sink.emit_raw(cb(at));
        }
        EventResponse::Consumed
    }
}

impl Widget for MenuWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.item_height = ctx.style.menu_item_height;
        let actions = self
            .items
            .iter()
            .filter(|item| matches!(item.kind, MenuItemKind::Action));
        let label_w = actions
            .clone()
            .map(|item| ui::width(item.label, TextRole::Body))
            .max()
            .unwrap_or(0);
        let shortcut_w = actions
            .filter_map(|item| item.shortcut)
            .map(|sc| ui::width(sc, TextRole::Body))
            .max()
            .unwrap_or(0);
        let gap = if shortcut_w > 0 { space::LG } else { 0 };
        let w = (PAD * 2 + label_w + gap + shortcut_w).max(ctx.style.menu_min_width);
        let h = self.items.len() as i32 * self.item_height;
        constraints.constrain(Size::new(w, h))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        super::card::Frame::raised(radius::CONTROL).paint(ctx, rect);

        for (i, item) in self.items.iter().enumerate() {
            let row = self.item_rect(i);
            if let MenuItemKind::Separator = item.kind {
                let mid = row.y + row.height / 2;
                ctx.fill_rect(row.x + PAD, mid, row.width - PAD * 2, 1, theme.border);
                continue;
            }
            let state = RowState {
                hovered: ctx.lit(self.id()) == Some(i),
                focused: self.focused,
                cursor: self.cursor == Some(i),
                ..RowState::default()
            };
            super::paint_row(ctx, row, state);
            let (ink, quiet) = match item.enabled {
                true => (theme.text, theme.text_secondary),
                false => (theme.text_disabled, theme.text_disabled),
            };
            let text_y = ui::centred_top(row.y, row.height, TextRole::Body);
            let mut room = row.width - PAD * 2;
            if let Some(sc) = item.shortcut {
                let sc_w = ui::width(sc, TextRole::Body);
                let sc_x = row.x + row.width - PAD - sc_w;
                ctx.draw_text(sc_x, text_y, sc, TextRole::Body, quiet);
                room -= sc_w + space::LG;
            }
            let cut =
                ctx.draw_text_fitted(row.x + PAD, text_y, item.label, TextRole::Body, room, ink);
            self.cut[i].set(cut);
        }
        if self.focused && self.cursor.is_none() {
            ctx.draw_inner_focus_ring(rect);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, .. } => {
                match self.item_at(*x, *y).filter(|&at| self.actionable(at)) {
                    Some(at) => self.choose(at, sink),
                    None => EventResponse::Ignored,
                }
            }
            WidgetEvent::KeyDown { key, .. } if self.focused => {
                let len = self.items.len();
                let next = |from, forward| {
                    super::next_enabled(len, from, forward, |at| self.actionable(at))
                };
                let cursor = match key {
                    Key::Named(NamedKey::Up) => next(self.cursor, false),
                    Key::Named(NamedKey::Down) => next(self.cursor, true),
                    Key::Named(NamedKey::Home) => next(None, true),
                    Key::Named(NamedKey::End) => next(None, false),
                    Key::Named(NamedKey::Enter | NamedKey::Space) => {
                        return match self.cursor.filter(|&at| self.actionable(at)) {
                            Some(at) => self.choose(at, sink),
                            None => EventResponse::Ignored,
                        };
                    }
                    _ => return EventResponse::Ignored,
                };
                self.cursor = cursor.or(self.cursor);
                EventResponse::Consumed
            }
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                self.cursor = None;
                EventResponse::Ignored
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Menu
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    fn focus_rect(&self) -> Rect {
        match self.cursor {
            Some(at) => self.item_rect(at),
            None => self.layout_rect(),
        }
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.item_at(x, y).filter(|&at| self.actionable(at))
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        let at = self.item_at(x, y)?;
        self.cut[at].get().then_some(self.items[at].label)
    }

    fn kept(&self) -> Option<Kept> {
        let mut shows = std::hash::DefaultHasher::new();
        for item in &self.items {
            item.label.hash(&mut shows);
        }
        Some(Kept {
            shows: shows.finish(),
            at: KeptAt::Cursor(self.cursor),
        })
    }

    fn keep(&mut self, at: KeptAt) {
        if let KeptAt::Cursor(cursor) = at {
            self.cursor = cursor.filter(|&at| self.actionable(at));
        }
    }
}
