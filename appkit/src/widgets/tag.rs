use std::cell::Cell;

use slopos_theme_core::{TextRole, Tone, space, stroke, with_alpha};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore};

const ROLE: TextRole = TextRole::Caption;
const PAD_X: i32 = space::SM;
/// A line of body text, so a row of text holding a tag keeps its height.
const HEIGHT: i32 = TextRole::Body.style().line_height;
/// Kept low so the word in the same tone still reads 4.5:1 on it.
const TINT: u8 = 0x1a;
const EDGE: u8 = 0x59;

pub struct TagWidget {
    core: WidgetCore,
    text: String,
    tone: Tone,
    cut: Cell<bool>,
}

impl TagWidget {
    pub fn new(text: String, tone: Tone) -> Self {
        Self {
            core: WidgetCore::new(),
            text,
            tone,
            cut: Cell::new(false),
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
        let w = ui::width(&self.text, ROLE) + PAD_X * 2;
        constraints.constrain(Size::new(w, HEIGHT))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let ink = ctx.theme().ink(self.tone);
        let r = rect.height / 2;
        ctx.fill_rounded_rect(rect, r, with_alpha(ink, TINT));
        ctx.draw_rounded_rect(rect, r, stroke::EDGE, with_alpha(ink, EDGE));
        let top = ui::centred_top(rect.y, rect.height, ROLE);
        let room = rect.width - PAD_X * 2;
        let cut = ctx.draw_text_fitted(rect.x + PAD_X, top, &self.text, ROLE, room, ink);
        self.cut.set(cut);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Label
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.text)
    }

    fn baseline(&self) -> Option<i32> {
        Some(ui::baseline_in(HEIGHT, ROLE))
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.cut.get().then_some(self.text.as_str())
    }
}
