use slopos_theme_core::{TextRole, series_of};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

/// The circle a person's portrait takes, and the role its initial is set in.
pub const PORTRAIT: (i32, TextRole) = (TextRole::Display.style().icon * 2, TextRole::Display);
/// The circle beside a list row's title, a line of it tall.
pub const ROW: (i32, TextRole) = (TextRole::Body.style().line_height, TextRole::Caption);

/// `name`'s initial on a circle of `diameter` with its box's top-left at
/// `(x, y)`, the initial in `role`.
pub(crate) fn paint(
    ctx: &mut PaintContext,
    name: &str,
    x: i32,
    y: i32,
    (diameter, role): (i32, TextRole),
) {
    let theme = ctx.theme();
    let fill = theme.series(series_of(name));
    ctx.fill_rounded_rect(x, y, diameter, diameter, diameter / 2, fill);
    let initial: String = name
        .chars()
        .next()
        .map_or_else(|| String::from("?"), |c| c.to_uppercase().collect());
    let w = ui::role_width(&initial, role);
    let top = y + (diameter - role.style().line_height) / 2;
    ctx.draw_text_in(
        x + (diameter - w) / 2,
        top,
        &initial,
        role,
        theme.text_on_accent,
    );
}

/// A person, by the initial of their name on a circle.
pub struct AvatarWidget {
    core: WidgetCore,
    name: String,
}

impl AvatarWidget {
    pub fn new(name: String) -> Self {
        Self {
            core: WidgetCore::new(),
            name,
        }
    }
}

impl Widget for AvatarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(Size::new(PORTRAIT.0, PORTRAIT.0))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let x = rect.x + (rect.width - PORTRAIT.0) / 2;
        let y = rect.y + (rect.height - PORTRAIT.0) / 2;
        paint(ctx, &self.name, x, y, PORTRAIT);
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.name)
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
