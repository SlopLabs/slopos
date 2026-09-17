use std::any::Any;

use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::ButtonStyle;
use crate::paint::PaintContext;
use crate::style::StyleSheet;
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
        }
    }
}

fn lighten(c: Color32, amount: u8) -> Color32 {
    Color32::new(
        c.red().saturating_add(amount),
        c.green().saturating_add(amount),
        c.blue().saturating_add(amount),
        c.alpha(),
    )
}

fn darken(c: Color32, amount: u8) -> Color32 {
    Color32::new(
        c.red().saturating_sub(amount),
        c.green().saturating_sub(amount),
        c.blue().saturating_sub(amount),
        c.alpha(),
    )
}

/// Returns (background, foreground) for the given style and state.
fn button_colors(style: &StyleSheet, bs: ButtonStyle, state: ButtonState) -> (Color32, Color32) {
    match (bs, state) {
        (ButtonStyle::Primary, ButtonState::Idle) => (style.bg_accent, style.text_on_accent),
        (ButtonStyle::Primary, ButtonState::Hovered) => {
            (lighten(style.bg_accent, 20), style.text_on_accent)
        }
        (ButtonStyle::Primary, ButtonState::Pressed) => {
            (darken(style.bg_accent, 20), style.text_on_accent)
        }

        (ButtonStyle::Secondary, ButtonState::Idle) => (style.bg_secondary, style.text_primary),
        (ButtonStyle::Secondary, ButtonState::Hovered) => (style.bg_tertiary, style.text_primary),
        (ButtonStyle::Secondary, ButtonState::Pressed) => {
            (darken(style.bg_secondary, 10), style.text_primary)
        }

        (ButtonStyle::Destructive, ButtonState::Idle) => {
            (style.bg_destructive, style.text_on_accent)
        }
        (ButtonStyle::Destructive, ButtonState::Hovered) => {
            (lighten(style.bg_destructive, 20), style.text_on_accent)
        }
        (ButtonStyle::Destructive, ButtonState::Pressed) => {
            (darken(style.bg_destructive, 20), style.text_on_accent)
        }

        (_, ButtonState::Disabled) => (style.bg_secondary, style.text_disabled),
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
        // The proportional metrics, because `paint` draws with them: measuring
        // with the fixed-cell atlas over-pads every button and, where the UI
        // font's line box is the taller of the two, makes the label overflow
        // the box that was sized for it.
        let text_w = ctx.text_width(&self.label);
        let text_h = ctx.text_height();
        let w = (text_w + ctx.style.button_padding_h * 2).max(ctx.style.button_min_width);
        let h = text_h + ctx.style.button_padding_v * 2;
        constraints.constrain(Size::new(w, h))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        // Hover from the live pointer. `PointerEnter`/`PointerLeave` are
        // declared and never constructed, so the only thing that ever set
        // `Hovered` was a release on this button — and the rebuild that
        // followed reset it. Every hover colour in this file was unreachable.
        let state = match self.state {
            ButtonState::Idle if self.enabled && rect.contains(ctx.pointer.0, ctx.pointer.1) => {
                ButtonState::Hovered
            }
            other => other,
        };
        let (bg, fg) = button_colors(ctx.style, self.style, state);

        ctx.fill_rounded_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            ctx.style.corner_radius,
            bg,
        );
        // A secondary button's fill is barely off its surround, so the edge is
        // what says it is a control rather than a label.
        if matches!(self.style, ButtonStyle::Secondary) {
            ctx.draw_rounded_rect(
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                ctx.style.corner_radius,
                ctx.style.border_default,
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
                // No prior PointerEnter is required: the framework may not
                // synthesise enter/leave from pointer motion.
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
            // Only when this button holds the keyboard focus. A key is offered
            // to every widget in turn until one consumes it, so a button that
            // answered Enter or Space unconditionally took them from whatever
            // the user was actually typing into.
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
