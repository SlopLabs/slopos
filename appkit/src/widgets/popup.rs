use std::any::Any;

use crate::audit::Arrangement;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// A child floated at a point over its parent, taking the parent's whole rect
/// so that a click outside the child dismisses it.
pub struct PopupWidget {
    core: WidgetCore,
    anchor: (i32, i32),
    child: Box<dyn Widget>,
    on_dismiss: Option<Box<dyn Fn() -> Box<dyn Any>>>,
}

impl PopupWidget {
    pub fn new(
        x: i32,
        y: i32,
        child: Box<dyn Widget>,
        on_dismiss: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            anchor: (x, y),
            child,
            on_dismiss,
        }
    }

    fn dismiss(&self, sink: &mut MessageSink) -> EventResponse {
        if let Some(f) = &self.on_dismiss {
            sink.emit_raw(f());
        }
        EventResponse::Consumed
    }

    /// At the anchor, flipped where it does not fit and then kept inside;
    /// flipped first, so a menu at an edge does not cover the pointer.
    fn child_rect(&self) -> Rect {
        let rect = self.layout_rect();
        let size = self.child.measured_size();
        let place = |at: i32, len: i32, lo: i32, room: i32| {
            let flipped = match at + len > lo + room && at - len >= lo {
                true => at - len,
                false => at,
            };
            flipped.clamp(lo, (lo + room - len).max(lo))
        };
        let x = place(self.anchor.0, size.width, rect.x, rect.width);
        let y = place(self.anchor.1, size.height, rect.y, rect.height);
        Rect::new(x, y, size.width, size.height)
    }
}

impl Widget for PopupWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        measure_widget(self.child.as_mut(), constraints.loosen(), ctx);
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, _rect: Rect) {
        let child_rect = self.child_rect();
        place_widget(self.child.as_mut(), child_rect);
    }

    fn paint(&self, ctx: &mut PaintContext) {
        ctx.with_layer(self.id(), |ctx| self.child.paint(ctx));
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Escape),
                ..
            } => self.dismiss(sink),
            WidgetEvent::PointerDown { x, y, .. } if !self.child.layout_rect().contains(*x, *y) => {
                self.dismiss(sink)
            }
            // A menu bar beneath still switches menus as the pointer slides.
            WidgetEvent::PointerMove { .. } => self.child.event(event, sink),
            // Modal otherwise: nothing beneath takes what the child leaves.
            _ => match self.child.event(event, sink) {
                EventResponse::Ignored => EventResponse::Consumed,
                taken => taken,
            },
        }
    }

    fn role(&self) -> Role {
        Role::Group
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    /// Holds the keyboard while open, which nothing would for a menu opened
    /// by the pointer; a declarer inside it still wins, coming later.
    fn declares_focus(&self) -> bool {
        true
    }

    fn arrangement(&self) -> Arrangement {
        Arrangement::Layers
    }

    fn covers(&self) -> bool {
        true
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
