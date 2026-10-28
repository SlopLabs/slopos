//! A divider dragged to resize a pane: it reports where the pointer is, and
//! the app decides what that makes the pane.

use std::any::Any;

use crate::constraints::{BoxConstraints, Orientation, Size};
use crate::event::{EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DragInput {
    Begin,
    /// The pointer's absolute position along the handle's axis.
    Move(i32),
    End,
}

type DragCallback = Box<dyn Fn(DragInput) -> Box<dyn Any>>;

pub struct DragHandleWidget {
    core: WidgetCore,
    orientation: Orientation,
    /// Given back by the app, since the tree is rebuilt between a drag's
    /// events, and set at the press, since its release may come before that.
    active: bool,
    on_drag: Option<DragCallback>,
}

impl DragHandleWidget {
    pub fn new(orientation: Orientation, active: bool, on_drag: Option<DragCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            orientation,
            active,
            on_drag,
        }
    }

    fn emit(&self, value: DragInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_drag {
            Some(cb) => {
                sink.emit_raw(cb(value));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }
}

impl Widget for DragHandleWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let across = ctx.style.divider_width;
        let size = match self.orientation {
            Orientation::Vertical => Size::new(across, constraints.max_height),
            Orientation::Horizontal => Size::new(constraints.max_width, across),
        };
        constraints.constrain(size)
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let color = if self.active || ctx.lit(self.id()).is_some() {
            ctx.theme().accent
        } else {
            ctx.theme().border
        };
        match self.orientation {
            Orientation::Vertical => {
                ctx.fill_rect(rect.x + rect.width / 2, rect.y, 1, rect.height, color)
            }
            Orientation::Horizontal => {
                ctx.fill_rect(rect.x, rect.y + rect.height / 2, rect.width, 1, color)
            }
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) || *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                self.active = true;
                self.emit(DragInput::Begin, sink);
                EventResponse::Consumed
            }
            WidgetEvent::PointerMove { x, y } if self.active => {
                let value = match self.orientation {
                    Orientation::Vertical => *x,
                    Orientation::Horizontal => *y,
                };
                self.emit(DragInput::Move(value), sink)
            }
            // A release reaches every widget the pointer missed, and every
            // message costs a rebuild.
            WidgetEvent::PointerUp { .. } if self.active => {
                self.active = false;
                self.emit(DragInput::End, sink);
                EventResponse::Consumed
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Separator
    }

    fn hover_part(&self, _x: i32, _y: i32) -> Option<usize> {
        Some(0)
    }
}
