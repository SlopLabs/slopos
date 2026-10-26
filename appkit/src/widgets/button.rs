use std::any::Any;

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, State, TextRole, Theme, inset, radius};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::ButtonStyle;
use crate::paint::PaintContext;
use crate::text::BODY;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// What every pressable control shares: pressed by a click that starts and
/// ends on it, or by Enter or Space while it holds focus.
#[derive(Default)]
pub(crate) struct Press {
    pressed: bool,
    pub(crate) focused: bool,
}

impl Press {
    /// Answers how the event was taken and whether it pressed the control.
    pub(crate) fn event(
        &mut self,
        rect: Rect,
        enabled: bool,
        event: &WidgetEvent,
        phase: EventPhase,
    ) -> (EventResponse, bool) {
        if phase != EventPhase::Target {
            return (EventResponse::Ignored, false);
        }
        match event {
            WidgetEvent::FocusGained => {
                self.focused = true;
                (EventResponse::Ignored, false)
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                self.pressed = false;
                (EventResponse::Ignored, false)
            }
            _ if !enabled => (EventResponse::Ignored, false),
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } if rect.contains(*x, *y) => {
                self.pressed = true;
                (EventResponse::Consumed, false)
            }
            WidgetEvent::PointerUp {
                x,
                y,
                button: PointerButton::Left,
            } => {
                let was = core::mem::take(&mut self.pressed);
                if was && rect.contains(*x, *y) {
                    (EventResponse::Consumed, true)
                } else {
                    (EventResponse::Ignored, false)
                }
            }
            WidgetEvent::KeyDown {
                key: Key::Named(NamedKey::Enter | NamedKey::Space),
                ..
            } if self.focused => (EventResponse::Consumed, true),
            _ => (EventResponse::Ignored, false),
        }
    }

    /// Normal, hovered under the pointer, or pressed.
    pub(crate) fn state(&self, rect: Rect, ctx: &PaintContext) -> State {
        if self.pressed {
            State::Pressed
        } else if rect.contains(ctx.pointer.0, ctx.pointer.1) {
            State::Hover
        } else {
            State::Normal
        }
    }
}

pub struct ButtonWidget {
    core: WidgetCore,
    label: String,
    on_press: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    style: ButtonStyle,
    enabled: bool,
    press: Press,
    baseline: i32,
    cut: bool,
}

impl ButtonWidget {
    pub fn new(
        label: String,
        on_press: Option<Box<dyn Fn() -> Box<dyn Any>>>,
        style: ButtonStyle,
        enabled: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            label,
            on_press,
            style,
            enabled,
            press: Press::default(),
            baseline: 0,
            cut: false,
        }
    }

    fn activate(&self, sink: &mut MessageSink) -> bool {
        match &self.on_press {
            Some(f) if self.enabled => {
                sink.emit_raw(f());
                true
            }
            _ => false,
        }
    }

    pub(crate) fn natural_width(&self, ctx: &MeasureCtx) -> i32 {
        let text_w = ctx.text_width(&self.label);
        (text_w + inset::CONTROL.horizontal()).max(ctx.style.button_min_width)
    }
}

/// Fill, ink and edge for the given style and state.
fn button_colors(
    theme: &Theme,
    style: ButtonStyle,
    enabled: bool,
    state: State,
) -> (Color32, Color32, Option<Color32>) {
    if !enabled {
        let edge = match style {
            ButtonStyle::Destructive => theme.danger,
            _ => theme.text_disabled,
        };
        return (theme.surface_raised, theme.text_disabled, Some(edge));
    }
    match style {
        ButtonStyle::Primary => (
            theme.state(ColorRole::Accent, state),
            theme.text_on_accent,
            None,
        ),
        ButtonStyle::Destructive => (
            theme.state(ColorRole::Danger, state),
            theme.text_on_accent,
            None,
        ),
        // A secondary button's fill is barely off its surround, so the edge
        // is what says it is a control rather than a label.
        ButtonStyle::Secondary => (
            theme.state(ColorRole::SurfaceRaised, state),
            theme.text,
            Some(theme.border_strong),
        ),
    }
}

impl Widget for ButtonWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let w = self.natural_width(ctx);
        let h = BODY.line_height + inset::CONTROL.vertical();
        let size = constraints.constrain(Size::new(w, h));
        self.baseline = ctx.baseline_in(size.height);
        size
    }

    fn layout(&mut self, rect: Rect) {
        let room = rect.width - inset::CONTROL.horizontal();
        self.cut = crate::text::ui::role_width(&self.label, TextRole::Body) > room;
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let state = self.press.state(rect, ctx);
        let (bg, fg, edge) = button_colors(ctx.theme(), self.style, self.enabled, state);
        ctx.fill_rounded_rect(rect.x, rect.y, rect.width, rect.height, radius::CONTROL, bg);
        if let Some(edge) = edge {
            ctx.draw_rounded_rect(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                radius::CONTROL,
                edge,
            );
        }

        let room = rect.width - inset::CONTROL.horizontal();
        let text_w = ctx.text_width(&self.label).min(room);
        let tx = rect.x + (rect.width - text_w) / 2;
        let ty = rect.y + (rect.height - ctx.text_height()) / 2;
        ctx.draw_text_fitted(tx, ty, &self.label, TextRole::Body, room, fg);

        if self.press.focused {
            ctx.draw_focus_ring(rect);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        let (response, pressed) = self
            .press
            .event(self.layout_rect(), self.enabled, event, phase);
        if pressed {
            self.activate(sink);
        }
        response
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.label)
    }

    fn baseline(&self) -> Option<i32> {
        Some(self.baseline)
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.cut.then_some(self.label.as_str())
    }

    /// A disabled control is not a tab stop: it answers nothing, draws no ring,
    /// and leaving it in the chain makes Tab appear to skip two.
    fn focus_policy(&self) -> FocusPolicy {
        if self.enabled {
            FocusPolicy::StrongFocus
        } else {
            FocusPolicy::None
        }
    }
}
