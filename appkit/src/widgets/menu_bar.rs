//! An in-window menu bar: the titles only.
//!
//! The dropdown is an ordinary [`super::popup::PopupWidget`] holding a
//! [`super::menu::MenuWidget`], which the application opens at the anchor this
//! widget reports. Keeping the two apart is what lets the application decide
//! what a menu contains at the moment it opens, rather than building every
//! menu's items on every frame.

use std::any::Any;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

const ITEM_PAD_H: i32 = 10;

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
    hovered: Option<usize>,
    widths: Vec<i32>,
    height: i32,
}

impl MenuBarWidget {
    pub fn new(titles: Vec<String>, open: Option<usize>, on_input: Option<InputCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            titles,
            open,
            on_input,
            hovered: None,
            widths: Vec::new(),
            height: 28,
        }
    }

    fn title_at(&self, x: i32) -> Option<usize> {
        let rect = self.layout_rect();
        let mut cursor = rect.x;
        for (index, width) in self.widths.iter().enumerate() {
            if x >= cursor && x < cursor + width {
                return Some(index);
            }
            cursor += width;
        }
        None
    }

    fn title_x(&self, index: usize) -> i32 {
        let rect = self.layout_rect();
        rect.x + self.widths.iter().take(index).sum::<i32>()
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
        self.height = ctx.style.menu_item_height + ctx.style.spacing_xs;
        self.widths = self
            .titles
            .iter()
            .map(|t| ctx.text_width(t) + ITEM_PAD_H * 2)
            .collect();
        let natural: i32 = self.widths.iter().sum();
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            natural
        };
        constraints.constrain(Size::new(width, self.height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let text_h = ctx.text_height();
        let mut x = rect.x;

        for (index, title) in self.titles.iter().enumerate() {
            let width = *self.widths.get(index).unwrap_or(&0);
            let is_open = self.open == Some(index);
            if is_open {
                ctx.fill_rounded_rect(
                    x + 2,
                    rect.y + 2,
                    width - 4,
                    rect.height - 4,
                    4,
                    style.bg_selected,
                );
            } else if self.hovered == Some(index) {
                ctx.fill_rounded_rect(
                    x + 2,
                    rect.y + 2,
                    width - 4,
                    rect.height - 4,
                    4,
                    style.bg_hover,
                );
            }
            let color = if is_open {
                style.text_primary
            } else {
                style.text_secondary
            };
            ctx.draw_text_transparent(
                x + ITEM_PAD_H,
                rect.y + (rect.height - text_h) / 2,
                title,
                color,
            );
            x += width;
        }
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
                let rect = self.layout_rect();
                self.emit(
                    MenuBarInput::Open {
                        index,
                        x: self.title_x(index),
                        y: rect.y + rect.height,
                    },
                    sink,
                )
            }

            WidgetEvent::PointerMove { x, y } => {
                let inside = self.layout_rect().contains(*x, *y);
                let hovered = if inside { self.title_at(*x) } else { None };
                let changed = hovered != self.hovered;
                self.hovered = hovered;
                // While a menu is open, crossing onto another title switches to
                // it — the behaviour every menu bar has, and the reason a menu
                // bar is a widget rather than a row of buttons.
                if let (Some(index), Some(open)) = (hovered, self.open) {
                    if index != open {
                        let rect = self.layout_rect();
                        return self.emit(
                            MenuBarInput::Hover {
                                index,
                                x: self.title_x(index),
                                y: rect.y + rect.height,
                            },
                            sink,
                        );
                    }
                }
                if changed {
                    EventResponse::Consumed
                } else {
                    EventResponse::Ignored
                }
            }

            WidgetEvent::PointerLeave => {
                self.hovered = None;
                EventResponse::Ignored
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Menu
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
