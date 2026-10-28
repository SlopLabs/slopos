use std::any::Any;
use std::cell::Cell;

use slopos_abi::draw::Color32;
use slopos_theme_core::{ColorRole, State, TextRole, Theme, inset, radius, stroke};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::node::ButtonStyle;
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, WidgetId};

/// What every pressable control shares: pressed by a click that starts and
/// ends on it, or by Space or Enter while it holds focus.
#[derive(Default)]
pub(crate) struct Press {
    pressed: bool,
    pub(crate) focused: bool,
    /// Leaves Enter to the page's default button.
    toggles: bool,
}

impl Press {
    /// A control that turns something on and off, which Space alone presses.
    pub(crate) fn toggle() -> Self {
        Self {
            toggles: true,
            ..Self::default()
        }
    }

    /// A tab stop while enabled; a click leaves focus where it was, so
    /// pressing a tool does not take the keyboard from a field.
    pub(crate) fn focus_policy(enabled: bool) -> FocusPolicy {
        match enabled {
            true => FocusPolicy::TabFocus,
            false => FocusPolicy::None,
        }
    }

    /// How the event was taken, and whether it pressed the control.
    pub(crate) fn event(
        &mut self,
        rect: Rect,
        enabled: bool,
        event: &WidgetEvent,
    ) -> (EventResponse, bool) {
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
                key: Key::Named(key @ (NamedKey::Enter | NamedKey::Space)),
                ..
            } if self.focused && !(self.toggles && *key == NamedKey::Enter) => {
                (EventResponse::Consumed, true)
            }
            _ => (EventResponse::Ignored, false),
        }
    }

    /// Lit whole while `enabled`, wherever its `rect` is.
    pub(crate) fn hover_part(enabled: bool, rect: Rect, x: i32, y: i32) -> Option<usize> {
        (enabled && rect.contains(x, y)).then_some(0)
    }

    /// What the control `id` shows: pressed, lit or neither.
    pub(crate) fn state(&self, id: WidgetId, ctx: &PaintContext) -> State {
        if self.pressed {
            State::Pressed
        } else if ctx.lit(id).is_some() {
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
    cut: Cell<bool>,
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
            cut: Cell::new(false),
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
        let text_w = ui::width(&self.label, TextRole::Body);
        (text_w + inset::CONTROL.horizontal()).max(ctx.style.button_min_width)
    }
}

/// Fill, ink and edge.
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
        let h = ctx.style.control_height;
        let size = constraints.constrain(Size::new(w, h));
        self.baseline = ui::baseline_in(size.height, TextRole::Body);
        size
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let state = self.press.state(self.id(), ctx);
        let (bg, fg, edge) = button_colors(ctx.theme(), self.style, self.enabled, state);
        ctx.fill_rounded_rect(rect, radius::CONTROL, bg);
        if let Some(edge) = edge {
            ctx.draw_rounded_rect(rect, radius::CONTROL, stroke::EDGE, edge);
        }

        let room = rect.width - inset::CONTROL.horizontal();
        let text_w = ui::width(&self.label, TextRole::Body).min(room);
        let tx = rect.x + (rect.width - text_w) / 2;
        let ty = ui::centred_top(rect.y, rect.height, TextRole::Body);
        let cut = ctx.draw_text_fitted(tx, ty, &self.label, TextRole::Body, room, fg);
        self.cut.set(cut);

        if self.press.focused {
            ctx.draw_focus_ring(rect);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        let (response, pressed) = self.press.event(self.layout_rect(), self.enabled, event);
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
        self.cut.get().then_some(self.label.as_str())
    }

    fn focus_policy(&self) -> FocusPolicy {
        Press::focus_policy(self.enabled)
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        Press::hover_part(self.enabled, self.layout_rect(), x, y)
    }
}
