//! The open documents' tabs, each with a dot for unsaved changes and a close
//! button.

use std::any::Any;
use std::cell::Cell;

use slopos_theme_core::{ColorRole, Icon, State, TextRole, space};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::style::MIN_TARGET;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

const LABEL: TextRole = TextRole::Body;
const PAD: i32 = space::MD;
const MIN_TAB_WIDTH: i32 = 90;
const MAX_TAB_WIDTH: i32 = 220;

#[derive(Clone, Debug)]
pub struct EditorTab {
    pub title: String,
    pub modified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabInput {
    Select(usize),
    Close(usize),
}

type InputCallback = Box<dyn Fn(TabInput) -> Box<dyn Any>>;

pub struct EditorTabsWidget {
    core: WidgetCore,
    tabs: Vec<EditorTab>,
    active: usize,
    on_input: Option<InputCallback>,
    widths: Vec<i32>,
    cut: Vec<Cell<bool>>,
}

impl EditorTabsWidget {
    pub fn new(tabs: Vec<EditorTab>, active: usize, on_input: Option<InputCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            tabs,
            active,
            on_input,
            widths: Vec::new(),
            cut: Vec::new(),
        }
    }

    fn glyph() -> i32 {
        LABEL.style().icon
    }

    fn compute_widths(&mut self, room: i32) {
        let chrome = PAD * 2 + space::SM + Self::glyph();
        self.widths = self
            .tabs
            .iter()
            .map(|tab| (chrome + ui::width(&tab.title, LABEL)).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH))
            .collect();
        self.cut = self.tabs.iter().map(|_| Cell::new(false)).collect();
        // Shrunk alike rather than scrolled, so every tab stays on screen;
        // shared out exactly, since a floor under a scaled width overflows.
        let count = self.widths.len() as i32;
        if count > 0 && room > 0 && self.widths.iter().sum::<i32>() > room {
            let each = room / count;
            self.widths.fill(each);
            if let Some(last) = self.widths.last_mut() {
                *last = room - each * (count - 1);
            }
        }
    }

    fn tab_x(&self, index: usize) -> i32 {
        self.layout_rect().x + self.widths[..index].iter().sum::<i32>()
    }

    fn tab_at(&self, x: i32) -> Option<usize> {
        let mut right = self.layout_rect().x;
        self.widths.iter().position(|&w| {
            right += w;
            x >= right - w && x < right
        })
    }

    /// The close glyph's box, which the dot shares.
    fn glyph_rect(&self, index: usize) -> Rect {
        let rect = self.layout_rect();
        let glyph = Self::glyph();
        let x = self.tab_x(index) + self.widths[index] - PAD - glyph;
        Rect::new(x, rect.y + (rect.height - glyph) / 2, glyph, glyph)
    }

    fn label_room(&self, index: usize) -> i32 {
        self.widths[index] - PAD * 2 - space::SM - Self::glyph()
    }

    fn emit(&self, input: TabInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }

    fn paint_tab(&self, ctx: &mut PaintContext, index: usize, hovered: bool) {
        let theme = ctx.theme();
        let rect = self.layout_rect();
        let tab = &self.tabs[index];
        let (x, w) = (self.tab_x(index), self.widths[index]);
        let active = index == self.active;
        if active {
            ctx.fill_rect(x, rect.y, w, rect.height, theme.surface);
            ctx.fill_rect(x, rect.y, w, super::INDICATOR, theme.accent);
        } else if hovered {
            let hover = theme.state(ColorRole::SurfaceSunken, State::Hover);
            ctx.fill_rect(x, rect.y, w, rect.height, hover);
        }
        let edge = rect.height - space::SM * 2;
        ctx.fill_rect(x + w - 1, rect.y + space::SM, 1, edge, theme.border);

        let ink = match active {
            true => theme.text,
            false => theme.text_secondary,
        };
        let top = ui::centred_top(rect.y, rect.height, LABEL);
        let room = self.label_room(index);
        let cut = ctx.draw_text_tail(x + PAD, top, &tab.title, LABEL, room, ink);
        self.cut[index].set(cut);

        let glyph = self.glyph_rect(index);
        // One slot: the dot gives way to the close glyph under the pointer.
        if tab.modified && !hovered {
            super::modified_dot(ctx, glyph.x, glyph.y, glyph.width, theme.accent);
        } else if hovered || active {
            let quiet = theme.text_secondary;
            ctx.draw_icon(Icon::X, glyph.x, glyph.y, glyph.width, quiet);
        }
    }
}

impl Widget for EditorTabsWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let width = match constraints.is_width_bounded() {
            true => constraints.max_width,
            false => MAX_TAB_WIDTH * self.tabs.len() as i32,
        };
        self.compute_widths(width);
        constraints.constrain(Size::new(width, ctx.style.tab_height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        ctx.with_clip(rect, |ctx| {
            let theme = ctx.theme();
            ctx.fill_rect(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                theme.surface_sunken,
            );
            let lit = ctx.lit(self.id());
            for index in 0..self.tabs.len() {
                self.paint_tab(ctx, index, lit == Some(index));
            }
            let bottom = rect.y + rect.height - 1;
            ctx.fill_rect(rect.x, bottom, rect.width, 1, theme.border);
        });
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                let Some(index) = self.tab_at(*x) else {
                    return EventResponse::Ignored;
                };
                let glyph = self.glyph_rect(index);
                let on_close = glyph
                    .inflate((MIN_TARGET - glyph.width) / 2)
                    .contains(*x, *y);
                match button {
                    PointerButton::Middle => self.emit(TabInput::Close(index), sink),
                    PointerButton::Left if on_close => self.emit(TabInput::Close(index), sink),
                    PointerButton::Left => self.emit(TabInput::Select(index), sink),
                    PointerButton::Right => EventResponse::Ignored,
                }
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Tab
    }

    fn hover_part(&self, x: i32, _y: i32) -> Option<usize> {
        self.tab_at(x)
    }

    fn tooltip(&self, x: i32, _y: i32) -> Option<&str> {
        let index = self.tab_at(x)?;
        self.cut[index]
            .get()
            .then_some(self.tabs[index].title.as_str())
    }
}
