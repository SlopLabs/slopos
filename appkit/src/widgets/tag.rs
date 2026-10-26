use slopos_theme_core::{TextRole, Tone, space};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

const ROLE: TextRole = TextRole::Caption;
const PAD_X: i32 = space::SM;
const PAD_Y: i32 = 2;

/// A word in a pill edged and inked in its tone.
pub struct TagWidget {
    core: WidgetCore,
    text: String,
    tone: Tone,
}

impl TagWidget {
    pub fn new(text: String, tone: Tone) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            tone,
        }
    }
}

impl Widget for TagWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let w = ui::role_width(&self.text, ROLE) + PAD_X * 2;
        let h = ROLE.style().line_height + PAD_Y * 2;
        constraints.constrain(Size::new(w, h))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let ink = ctx.theme().ink(self.tone);
        let r = rect.height / 2;
        ctx.draw_rounded_rect(rect.x, rect.y, rect.width, rect.height, r, ink);
        let top = rect.y + (rect.height - ui::role_line_height(ROLE)) / 2;
        let room = rect.width - PAD_X * 2;
        ctx.draw_text_fitted(rect.x + PAD_X, top, &self.text, ROLE, room, ink);
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn baseline(&self) -> Option<i32> {
        let h = ROLE.style().line_height + PAD_Y * 2;
        Some((h - ui::role_line_height(ROLE)) / 2 + ui::role_ascent(ROLE))
    }
}
