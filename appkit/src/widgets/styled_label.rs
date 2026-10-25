use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size, TextAlignment};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Label with an explicit foreground color that overrides the theme.
pub struct StyledLabelWidget {
    core: WidgetCore,
    text: String,
    color: Color32,
    alignment: TextAlignment,
    baseline: i32,
}

impl StyledLabelWidget {
    pub fn new(text: String, color: Color32, alignment: TextAlignment) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            color,
            alignment,
            baseline: 0,
        }
    }
}

impl Widget for StyledLabelWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let text_w = ctx.text_width(&self.text);
        let size = constraints.constrain(Size::new(text_w, ctx.style.line_height));
        // `paint` centres the line in whatever height it is given.
        self.baseline = ctx.baseline_in(size.height);
        size
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let tw = ctx.text_width(&self.text);
        let x = match self.alignment {
            TextAlignment::Start => rect.x,
            TextAlignment::Center => rect.x + (rect.width - tw) / 2,
            TextAlignment::End => rect.x + rect.width - tw,
        };
        // Centred in the rect, not parked at its top: a label in a bar taller
        // than one line — a status bar, a header — gets the bar's height, and
        // drawing at `rect.y` leaves the text riding high in it. In a rect one
        // line box tall this is the baseline a `LabelWidget` uses.
        let baseline = rect.y + ctx.baseline_in(rect.height);
        ctx.draw_text_on_baseline(x, baseline, &self.text, self.color);
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
        Some(self.baseline)
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
