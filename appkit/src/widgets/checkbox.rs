use std::any::Any;
use std::cell::Cell;

use slopos_theme_core::{ColorRole, Icon, State, TextRole, radius, stroke};

use super::button::Press;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::style::MIN_TARGET;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub struct CheckboxWidget {
    core: WidgetCore,
    checked: bool,
    label: String,
    on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    enabled: bool,
    press: Press,
    /// From the box's left edge to the label.
    indent: i32,
    cut: Cell<bool>,
}

impl CheckboxWidget {
    pub fn new(
        checked: bool,
        label: String,
        on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
        enabled: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            checked,
            label,
            on_toggle,
            enabled,
            press: Press::toggle(),
            indent: 0,
            cut: Cell::new(false),
        }
    }
}

impl Widget for CheckboxWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let side = ctx.style.checkbox_size;
        self.indent = side + ctx.style.checkbox_gap;
        let width = self.indent + ui::width(&self.label, TextRole::Body);
        let height = side.max(ui::font_height(TextRole::Body)).max(MIN_TARGET);
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        let side = ctx.style.checkbox_size;
        let boxed = Rect::new(rect.x, rect.y + (rect.height - side) / 2, side, side);
        let r = radius::CONTROL.min(side / 4);
        let state = match self.enabled {
            true => self.press.state(self.id(), ctx),
            false => State::Normal,
        };

        if self.checked {
            let fill = match self.enabled {
                true => theme.state(ColorRole::AccentFill, state),
                false => theme.text_disabled,
            };
            ctx.fill_rounded_rect(boxed, r, fill);
            if self.enabled {
                ctx.draw_top_rim(boxed, r);
            }
            ctx.draw_icon(Icon::Check, boxed.x, boxed.y, side, theme.text_on_accent);
        } else {
            ctx.fill_rounded_rect(boxed, r, theme.state(ColorRole::SurfaceSunken, state));
            let edge = match self.enabled {
                true => theme.border_strong,
                false => theme.text_disabled,
            };
            ctx.draw_rounded_rect(boxed, r, stroke::EDGE, edge);
        }

        let text_x = rect.x + self.indent;
        let text_y = ui::centred_top(rect.y, rect.height, TextRole::Body);
        let ink = match self.enabled {
            true => theme.text,
            false => theme.text_disabled,
        };
        let room = rect.x + rect.width - text_x;
        let cut = ctx.draw_text_fitted(text_x, text_y, &self.label, TextRole::Body, room, ink);
        self.cut.set(cut);

        if self.press.focused {
            ctx.draw_focus_ring(rect);
        }
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.cut.get().then_some(&self.label)
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        let (response, pressed) = self.press.event(self.layout_rect(), self.enabled, event);
        if pressed {
            self.checked = !self.checked;
            if let Some(f) = &self.on_toggle {
                sink.emit_raw(f());
            }
        }
        response
    }

    fn role(&self) -> Role {
        Role::Checkbox
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.label)
    }

    fn focus_policy(&self) -> FocusPolicy {
        Press::focus_policy(self.enabled)
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        Press::hover_part(self.enabled, self.layout_rect(), x, y)
    }
}
