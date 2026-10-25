use slopos_theme_core::{Surface, radius, with_alpha};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget};

/// A filled, rounded, optionally bordered surface with a soft drop shadow.
///
/// [`super::super::node::Node::Background`] fills a rect and nothing else, which
/// is right for a pane and wrong for anything that floats: a palette or a
/// popover has to read as *above* the surface behind it, and an edge and a
/// shadow are what say so.
pub struct CardWidget {
    core: WidgetCore,
    surface: Surface,
    border: bool,
    shadow: bool,
    child: Box<dyn Widget>,
}

impl CardWidget {
    pub fn new(surface: Surface, border: bool, shadow: bool, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            surface,
            border,
            shadow,
            child,
        }
    }
}

/// Concentric one-pixel frames of decreasing alpha around `rect`, the theme's
/// raised shadow.
///
/// Rings rather than filled rects, and blended rather than
/// [`PaintContext::fill_rounded_rect`](crate::paint::PaintContext::fill_rounded_rect):
/// the rounded fill encodes its colour opaquely, so a translucent shadow drawn
/// with it paints a hard black frame instead of a soft one.
pub fn draw_shadow(ctx: &mut PaintContext, rect: Rect) {
    let shadow = ctx.theme().shadow_raised;
    for ring in 1..=shadow.spread {
        let color = with_alpha(shadow.color, (shadow.color.alpha() as i32 / ring) as u8);
        let x = rect.x - ring;
        let y = rect.y - ring + shadow.offset_y;
        let w = rect.width + ring * 2;
        let h = rect.height + ring * 2;
        ctx.fill_rect_blended(x, y, w, 1, color);
        ctx.fill_rect_blended(x, y + h - 1, w, 1, color);
        ctx.fill_rect_blended(x, y + 1, 1, h - 2, color);
        ctx.fill_rect_blended(x + w - 1, y + 1, 1, h - 2, color);
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
        let rect = self.layout_rect();
        if self.shadow {
            draw_shadow(ctx, rect);
        }
        let theme = ctx.theme();
        let fill = theme.fill(self.surface);
        ctx.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, radius::CARD, fill);
        if self.border {
            let edge = theme.border;
            ctx.draw_rounded_rect(rect.x, rect.y, rect.width, rect.height, radius::CARD, edge);
        }
        self.child.paint(ctx);
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        self.child.event(event, phase, sink)
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
