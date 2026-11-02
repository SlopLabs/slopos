use std::any::Any;

use slopos_theme_core::{ColorRole, EdgeInsets, State, radius, stroke};

use super::button::Press;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::style::MIN_TARGET;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// An on/off setting whose whole row is one control.
pub struct SwitchWidget {
    core: WidgetCore,
    on: bool,
    label: String,
    words: Vec<Box<dyn Widget>>,
    on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    enabled: bool,
    press: Press,
    track: Size,
    knob_inset: i32,
    gap: i32,
}

impl SwitchWidget {
    pub fn new(
        on: bool,
        label: String,
        words: Box<dyn Widget>,
        on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
        enabled: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            on,
            label,
            words: vec![words],
            on_toggle,
            enabled,
            press: Press::toggle(),
            track: Size::ZERO,
            knob_inset: 0,
            gap: 0,
        }
    }

    fn track_rect(&self) -> Rect {
        let rect = self.layout_rect();
        Rect::new(
            rect.x + rect.width - self.track.width,
            rect.y + (rect.height - self.track.height) / 2,
            self.track.width,
            self.track.height,
        )
    }
}

impl Widget for SwitchWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.track = Size::new(ctx.style.switch_width, ctx.style.switch_height);
        self.knob_inset = ctx.style.switch_knob_inset;
        self.gap = ctx.style.switch_gap;
        let beside = EdgeInsets::new(0, self.track.width + self.gap, 0, 0);
        let room = constraints.loosen().deflate(beside);
        let words = measure_widget(self.words[0].as_mut(), room, ctx);
        let mut size = Size::new(
            words.width + self.gap + self.track.width,
            words.height.max(self.track.height).max(MIN_TARGET),
        );
        if constraints.is_width_bounded() {
            size.width = constraints.max_width;
        }
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        let width = (rect.width - self.gap - self.track.width).max(0);
        let height = self.words[0].measured_size().height;
        let top = rect.y + (rect.height - height) / 2;
        place_widget(
            self.words[0].as_mut(),
            Rect::new(rect.x, top, width, height),
        );
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let theme = ctx.theme();
        let state = match self.enabled {
            true => self.press.state(self.id(), ctx),
            false => State::Normal,
        };
        if state != State::Normal {
            ctx.fill_rounded_rect(self.layout_rect(), radius::CONTROL, theme.overlay(state));
        }
        for words in &self.words {
            words.paint(ctx);
        }

        let track = self.track_rect();
        let round = track.height / 2;
        let side = track.height - 2 * self.knob_inset;
        let knob_x = match self.on {
            true => track.x + track.width - self.knob_inset - side,
            false => track.x + self.knob_inset,
        };
        let knob = Rect::new(knob_x, track.y + self.knob_inset, side, side);
        if self.on {
            let fill = match self.enabled {
                true => theme.state(ColorRole::AccentFill, state),
                false => theme.text_disabled,
            };
            ctx.fill_rounded_rect(track, round, fill);
            if self.enabled {
                ctx.draw_top_rim(track, round);
            }
            ctx.fill_rounded_rect(knob, side / 2, theme.text_on_accent);
        } else {
            let (edge, knob_ink) = match self.enabled {
                true => (theme.border_strong, theme.text_secondary),
                false => (theme.text_disabled, theme.text_disabled),
            };
            ctx.fill_rounded_rect(track, round, theme.state(ColorRole::SurfaceSunken, state));
            ctx.draw_rounded_rect(track, round, stroke::EDGE, edge);
            ctx.fill_rounded_rect(knob, side / 2, knob_ink);
        }

        if self.press.focused {
            ctx.draw_focus_ring(self.layout_rect());
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        let (response, pressed) = self.press.event(self.layout_rect(), self.enabled, event);
        if pressed {
            self.on = !self.on;
            if let Some(f) = &self.on_toggle {
                sink.emit_raw(f());
            }
        }
        response
    }

    fn role(&self) -> Role {
        Role::Switch
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

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.words
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.words
    }
}
