//! An in-window menu bar, the titles alone: the app opens a menu in a
//! [`super::popup::PopupWidget`] at the place this reports.

use std::any::Any;

use slopos_theme_core::{ColorRole, State, TextRole, radius, space};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

const PAD: i32 = space::MD;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MenuBarInput {
    /// Clicked a title: `x` is the title's left edge, for the popup anchor.
    Open { index: usize, x: i32, y: i32 },
    /// The pointer crossed onto another title while a menu was open.
    Hover { index: usize, x: i32, y: i32 },
    /// Clicked the open title again.
    Close,
}

type InputCallback = Box<dyn Fn(MenuBarInput) -> Box<dyn Any>>;

pub struct MenuBarWidget {
    core: WidgetCore,
    titles: Vec<String>,
    open: Option<usize>,
    on_input: Option<InputCallback>,
    widths: Vec<i32>,
    item_height: i32,
}

impl MenuBarWidget {
    pub fn new(titles: Vec<String>, open: Option<usize>, on_input: Option<InputCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            titles,
            open,
            on_input,
            widths: Vec::new(),
            item_height: 0,
        }
    }

    fn title_at(&self, x: i32) -> Option<usize> {
        let mut right = self.layout_rect().x;
        self.widths.iter().position(|&w| {
            right += w;
            x >= right - w && x < right
        })
    }

    fn title_x(&self, index: usize) -> i32 {
        self.layout_rect().x + self.widths[..index].iter().sum::<i32>()
    }

    /// Where the menu of title `index` opens.
    fn anchor(&self, index: usize) -> (i32, i32) {
        let rect = self.layout_rect();
        (self.title_x(index), rect.y + rect.height)
    }

    fn emit(&self, input: MenuBarInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }
}

impl Widget for MenuBarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.item_height = ctx.style.menu_item_height;
        self.widths = self
            .titles
            .iter()
            .map(|t| ui::width(t, TextRole::Body) + PAD * 2)
            .collect();
        let width = match constraints.is_width_bounded() {
            true => constraints.max_width,
            false => self.widths.iter().sum(),
        };
        constraints.constrain(Size::new(width, ctx.style.menu_bar_height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        let text_y = ui::centred_top(rect.y, rect.height, TextRole::Body);
        let pill_y = rect.y + (rect.height - self.item_height) / 2;
        let lit = ctx.lit(self.id());
        for (index, title) in self.titles.iter().enumerate() {
            let (x, w) = (self.title_x(index), self.widths[index]);
            let open = self.open == Some(index);
            let state = match (open, lit == Some(index)) {
                (true, _) => State::Pressed,
                (false, true) => State::Hover,
                (false, false) => State::Normal,
            };
            if state != State::Normal {
                let pill = Rect::new(x, pill_y, w, self.item_height);
                let fill = theme.state(ColorRole::Chrome, state);
                ctx.fill_rounded_rect(pill, radius::CONTROL, fill);
            }
            let ink = match open {
                true => theme.text,
                false => theme.text_secondary,
            };
            ctx.draw_text_fitted(x + PAD, text_y, title, TextRole::Body, w - PAD * 2, ink);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) || *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                let Some(index) = self.title_at(*x) else {
                    return EventResponse::Ignored;
                };
                if self.open == Some(index) {
                    return self.emit(MenuBarInput::Close, sink);
                }
                let (x, y) = self.anchor(index);
                self.emit(MenuBarInput::Open { index, x, y }, sink)
            }

            WidgetEvent::PointerMove { x, y } if self.layout_rect().contains(*x, *y) => {
                match (self.title_at(*x), self.open) {
                    (Some(index), Some(open)) if index != open => {
                        let (x, y) = self.anchor(index);
                        self.emit(MenuBarInput::Hover { index, x, y }, sink)
                    }
                    _ => EventResponse::Ignored,
                }
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Menu
    }

    fn hover_part(&self, x: i32, _y: i32) -> Option<usize> {
        self.title_at(x)
    }
}
