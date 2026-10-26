use slopos_theme_core::{Icon, TextRole, Tone};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

/// An icon at the size of text in a role, in a line box of that role.
pub struct IconWidget {
    core: WidgetCore,
    icon: Icon,
    role: TextRole,
    tone: Tone,
}

impl IconWidget {
    pub fn new(icon: Icon, role: TextRole, tone: Tone) -> Self {
        Self {
            core: WidgetCore::new(),
            icon,
            role,
            tone,
        }
    }
}

impl Widget for IconWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let style = self.role.style();
        constraints.constrain(Size::new(style.icon, style.line_height.max(style.icon)))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let size = self.role.style().icon;
        let color = ctx.theme().ink(self.tone);
        ctx.draw_icon(
            self.icon,
            rect.x + (rect.width - size) / 2,
            rect.y + (rect.height - size) / 2,
            size,
            color,
        );
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }
}
