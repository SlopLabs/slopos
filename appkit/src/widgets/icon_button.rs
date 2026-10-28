use std::any::Any;

use slopos_theme_core::{Icon, State, TextRole, radius, space};

use super::button::Press;
use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// An [`IconButton`](crate::IconButton) is square, this wide and tall.
pub const ICON_BUTTON_SIDE: i32 = TextRole::Body.style().icon + 2 * space::SM;

/// A square control showing an icon, named by its tooltip.
pub struct IconButtonWidget {
    core: WidgetCore,
    icon: Icon,
    label: String,
    on_press: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    enabled: bool,
    active: bool,
    press: Press,
}

impl IconButtonWidget {
    pub fn new(
        icon: Icon,
        label: String,
        on_press: Option<Box<dyn Fn() -> Box<dyn Any>>>,
        enabled: bool,
        active: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            icon,
            label,
            on_press,
            enabled,
            active,
            press: Press::default(),
        }
    }
}

impl Widget for IconButtonWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        constraints.constrain(Size::new(ICON_BUTTON_SIDE, ICON_BUTTON_SIDE))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        let state = match self.press.state(self.id(), ctx) {
            _ if !self.enabled => State::Normal,
            _ if self.active => State::Pressed,
            state => state,
        };
        if state != State::Normal {
            ctx.fill_rounded_rect(rect, radius::CONTROL, theme.overlay(state));
        }
        let ink = if self.enabled {
            theme.text
        } else {
            theme.text_disabled
        };
        let size = TextRole::Body.style().icon;
        ctx.draw_icon(
            self.icon,
            rect.x + (rect.width - size) / 2,
            rect.y + (rect.height - size) / 2,
            size,
            ink,
        );
        if self.press.focused {
            ctx.draw_focus_ring(rect);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        let (response, pressed) = self.press.event(self.layout_rect(), self.enabled, event);
        if let (true, Some(f)) = (pressed, &self.on_press) {
            sink.emit_raw(f());
        }
        response
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.label)
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        Some(&self.label)
    }

    fn focus_policy(&self) -> FocusPolicy {
        Press::focus_policy(self.enabled)
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        Press::hover_part(self.enabled, self.layout_rect(), x, y)
    }
}
