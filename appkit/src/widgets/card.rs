use slopos_theme_core::{Surface, radius, stroke};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget};

/// A surface's fill inside an edge, and whether a shadow says it floats above
/// what is behind it.
#[derive(Copy, Clone)]
pub(crate) struct Frame {
    pub surface: Surface,
    pub radius: i32,
    pub shadow: bool,
}

impl Frame {
    /// What floats over a page: a dialog, a menu, a tooltip.
    pub const fn raised(radius: i32) -> Self {
        Self {
            surface: Surface::Raised,
            radius,
            shadow: true,
        }
    }

    pub fn paint(&self, ctx: &mut PaintContext, rect: Rect) {
        if self.shadow {
            ctx.draw_shadow(rect, self.radius);
        }
        let theme = ctx.theme();
        ctx.fill_rounded_rect(rect, self.radius, theme.fill(self.surface));
        ctx.draw_rounded_rect(rect, self.radius, stroke::EDGE, theme.border);
        if self.shadow {
            ctx.draw_top_rim(rect, self.radius);
        }
    }
}

/// A rounded surface inside an edge, which a shadow lifts above the page, as
/// a palette or a popover is, where it floats.
pub struct CardWidget {
    core: WidgetCore,
    frame: Frame,
    child: Box<dyn Widget>,
}

impl CardWidget {
    pub fn new(surface: Surface, shadow: bool, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            frame: Frame {
                surface,
                radius: radius::CARD,
                shadow,
            },
            child,
        }
    }
}

impl Widget for CardWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let size = measure_widget(self.child.as_mut(), constraints, ctx);
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        place_widget(self.child.as_mut(), rect);
    }

    fn paint(&self, ctx: &mut PaintContext) {
        self.frame.paint(ctx, self.layout_rect());
        self.child.paint(ctx);
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        self.child.event(event, sink)
    }

    fn role(&self) -> Role {
        Role::Group
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
