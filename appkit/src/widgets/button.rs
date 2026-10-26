use std::any::Any;

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, State, Theme, inset, radius};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::ButtonStyle;
use crate::paint::PaintContext;
use crate::text::BODY;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ButtonState {
    Idle,
    Hovered,
    Pressed,
    Disabled,
}

pub struct ButtonWidget {
    core: WidgetCore,
    label: String,
    on_press: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    style: ButtonStyle,
    enabled: bool,
    state: ButtonState,
    focused: bool,
    baseline: i32,
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
            state: if enabled {
                ButtonState::Idle
            } else {
                ButtonState::Disabled
            },
            focused: false,
            baseline: 0,
        }
    }
}

/// Returns (background, foreground) for the given style and state.
fn button_colors(theme: &Theme, bs: ButtonStyle, state: ButtonState) -> (Color32, Color32) {
    let fill = match bs {
        ButtonStyle::Primary => ColorRole::Accent,
        ButtonStyle::Secondary => ColorRole::SurfaceRaised,
        ButtonStyle::Destructive => ColorRole::Danger,
    };
    let ink = match bs {
        ButtonStyle::Secondary => theme.text,
        _ => theme.text_on_accent,
    };
    match state {
        ButtonState::Idle => (theme.color(fill), ink),
        ButtonState::Hovered => (theme.state(fill, State::Hover), ink),
        ButtonState::Pressed => (theme.state(fill, State::Pressed), ink),
        ButtonState::Disabled => (theme.surface_raised, theme.text_disabled),
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
        // The proportional metrics, because `paint` draws with them.
        let text_w = ctx.text_width(&self.label);
        let w = (text_w + inset::CONTROL.horizontal()).max(ctx.style.button_min_width);
        let h = BODY.line_height + inset::CONTROL.vertical();
        let size = constraints.constrain(Size::new(w, h));
        // `paint` centres the label's line in the button.
        self.baseline = ctx.baseline_in(size.height);
        size
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        // From the live pointer: `PointerEnter`/`PointerLeave` are declared
        // and never constructed.
        let state = match self.state {
            ButtonState::Idle if self.enabled && rect.contains(ctx.pointer.0, ctx.pointer.1) => {
                ButtonState::Hovered
            }
            other => other,
        };
        let (bg, fg) = button_colors(ctx.theme(), self.style, state);

        let edge = match (state, self.style) {
            (ButtonState::Disabled, _) => Some(ctx.theme().text_disabled),
            // A secondary button's fill is barely off its surround, so the edge
            // is what says it is a control rather than a label.
            (_, ButtonStyle::Secondary) => Some(ctx.theme().border_strong),
            _ => None,
        };
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

        let text_w = ctx.text_width(&self.label);
        let text_h = ctx.text_height();
        let tx = rect.x + (rect.width - text_w) / 2;
        let ty = rect.y + (rect.height - text_h) / 2;
        ctx.draw_text_transparent(tx, ty, &self.label, fg);

        if self.focused {
            ctx.draw_focus_ring(rect);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target {
            return EventResponse::Ignored;
        }

        if !self.enabled {
            self.state = ButtonState::Disabled;
            return EventResponse::Ignored;
        }

        match event {
            WidgetEvent::PointerEnter => {
                if self.state == ButtonState::Idle {
                    self.state = ButtonState::Hovered;
                }
                EventResponse::Ignored
            }
            WidgetEvent::PointerLeave => {
                self.state = ButtonState::Idle;
                EventResponse::Ignored
            }
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                if self.state != ButtonState::Disabled {
                    self.state = ButtonState::Pressed;
                }
                EventResponse::Consumed
            }
            WidgetEvent::PointerUp {
                x,
                y,
                button: PointerButton::Left,
            } => {
                if !self.layout_rect().contains(*x, *y) {
                    self.state = ButtonState::Idle;
                    return EventResponse::Ignored;
                }
                if self.state == ButtonState::Pressed {
                    self.state = ButtonState::Hovered;
                    if let Some(f) = &self.on_press {
                        sink.emit_raw(f());
                    }
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                if self.state == ButtonState::Pressed {
                    self.state = ButtonState::Idle;
                }
                EventResponse::Ignored
            }
            WidgetEvent::KeyDown { key, .. } if self.focused => {
                if matches!(
                    key,
                    Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space)
                ) {
                    if let Some(f) = &self.on_press {
                        sink.emit_raw(f());
                    }
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }
            _ => EventResponse::Ignored,
        }
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
