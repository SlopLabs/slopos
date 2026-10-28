use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

/// A 1 px line across whichever axis its parent leaves free: a column's width,
/// a row's height.
pub struct SeparatorWidget {
    core: WidgetCore,
}

impl SeparatorWidget {
    pub fn new() -> Self {
        Self {
            core: WidgetCore::new(),
        }
    }
}

impl Widget for SeparatorWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let h_free = !constraints.is_height_bounded();
        let w_free = !constraints.is_width_bounded();
        let size = match (w_free, h_free) {
            (false, true) => Size::new(constraints.max_width, 1),
            (true, false) => Size::new(1, constraints.max_height),
            _ => Size::new(1, 1),
        };
        constraints.constrain(size)
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let color = ctx.theme().border;
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, color);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Separator
    }
}
