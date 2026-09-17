//! A draggable divider, for a resizable sidebar.
//!
//! Reports the pointer's absolute position while dragging and nothing else; how
//! wide the pane may get, and what it does when the window is too narrow, are
//! the application's to decide.

use std::any::Any;

use crate::constraints::{BoxConstraints, Orientation, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Pixels the handle spans; wider than the line it draws, because a one-pixel
/// hit target is one no pointer finds.
const HIT_WIDTH: i32 = 6;

/// What a handle reports. A drag spans several events and the widget tree is
/// rebuilt between them, so the application is told when one begins and ends
/// rather than the widget remembering.
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
    /// Whether a drag is live, as the application sees it.
    active: bool,
    on_drag: Option<DragCallback>,
    hovered: bool,
}

impl DragHandleWidget {
    pub fn new(orientation: Orientation, active: bool, on_drag: Option<DragCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            orientation,
            active,
            on_drag,
            hovered: false,
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

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let size = match self.orientation {
            Orientation::Vertical => Size::new(HIT_WIDTH, constraints.max_height),
            Orientation::Horizontal => Size::new(constraints.max_width, HIT_WIDTH),
        };
        constraints.constrain(size)
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let color = if self.active || self.hovered {
            ctx.style.text_accent
        } else {
            ctx.style.border_divider
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
                self.emit(DragInput::Begin, sink);
                EventResponse::CapturePointer
            }
            WidgetEvent::PointerMove { x, y } => {
                if self.active {
                    let value = match self.orientation {
                        Orientation::Vertical => *x,
                        Orientation::Horizontal => *y,
                    };
                    return self.emit(DragInput::Move(value), sink);
                }
                let hovered = self.layout_rect().contains(*x, *y);
                if hovered != self.hovered {
                    self.hovered = hovered;
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }
            WidgetEvent::PointerUp { .. } => {
                // A release reaches every widget the pointer missed, and every
                // message costs a rebuild.
                if !self.active {
                    return EventResponse::Ignored;
                }
                self.emit(DragInput::End, sink);
                EventResponse::ReleasePointer
            }
            WidgetEvent::PointerLeave => {
                self.hovered = false;
                EventResponse::Ignored
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Separator
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
