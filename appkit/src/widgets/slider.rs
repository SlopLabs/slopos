use std::any::Any;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// What a slider reports. A drag spans several events and the widget tree is
/// rebuilt between them, so the application keeps the drag's [`SliderGrab`]
/// from `Begin` to `End`, as it keeps a [`crate::DragInput`]'s.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SliderInput {
    /// A press begins a drag: at the value under the pointer, or where the
    /// knob is for a press on it, so a click there changes nothing.
    Begin(u64, SliderGrab),
    /// The drag or a key moved the value.
    Change(u64),
    End,
}

/// Where on the knob a drag holds it, so the knob keeps its place under the
/// pointer rather than jumping its centre there; the default holds it by its
/// centre.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SliderGrab(pub(crate) i32);

/// The values a slider takes: `min + k * step` up to `max`, and `max`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Range {
    min: u64,
    max: u64,
    step: u64,
}

impl Range {
    pub(crate) fn new(min: u64, max: u64, step: u64) -> Self {
        Self {
            min,
            max: max.max(min),
            step: step.max(1),
        }
    }

    fn floor(&self, value: u64) -> u64 {
        self.min + (value - self.min) / self.step * self.step
    }

    /// The value nearest `value`, `max` among them.
    pub(crate) fn snap(&self, value: u64) -> u64 {
        let value = value.clamp(self.min, self.max);
        let below = self.floor(value);
        let above = below.saturating_add(self.step).min(self.max);
        if value - below < above - value {
            below
        } else {
            above
        }
    }

    /// The `steps`-th value along from `value`, back for a negative count;
    /// with `page`, counting pages of a tenth of the range, a step at least.
    pub(crate) fn moved(&self, value: u64, steps: i64, page: bool) -> u64 {
        let unit = if page {
            ((self.max - self.min) / 10 / self.step).max(1) * self.step
        } else {
            self.step
        };
        let delta = unit.saturating_mul(steps.unsigned_abs());
        let value = value.clamp(self.min, self.max);
        if steps < 0 {
            match value.saturating_sub(delta) {
                to if to <= self.min => self.min,
                to if self.floor(to) == to => to,
                to => self.floor(to) + self.step,
            }
        } else {
            match value.saturating_add(delta) {
                to if to >= self.max => self.max,
                to => self.floor(to),
            }
        }
    }
}

/// A horizontal slider whose value and drag the application owns.
pub struct SliderWidget {
    core: WidgetCore,
    value: u64,
    range: Range,
    enabled: bool,
    grab: Option<SliderGrab>,
    on_input: Option<Box<dyn Fn(SliderInput) -> Box<dyn Any>>>,
    focused: bool,
    thumb: i32,
}

impl SliderWidget {
    pub fn new(
        value: u64,
        min: u64,
        max: u64,
        step: u64,
        enabled: bool,
        grab: Option<SliderGrab>,
        on_input: Option<Box<dyn Fn(SliderInput) -> Box<dyn Any>>>,
    ) -> Self {
        let range = Range::new(min, max, step);
        Self {
            core: WidgetCore::new(),
            value: value.clamp(range.min, range.max),
            range,
            enabled,
            grab,
            on_input,
            focused: false,
            thumb: 0,
        }
    }

    pub fn value(&self) -> u64 {
        self.value
    }

    /// The span the thumb's centre travels, in window coordinates.
    fn track(&self) -> (i32, i32) {
        let rect = self.layout_rect();
        let half = self.thumb / 2;
        (rect.x + half, (rect.width - self.thumb).max(1))
    }

    /// The value under window column `x`.
    pub fn value_at(&self, x: i32) -> u64 {
        let (left, width) = self.track();
        let along = (x - left).clamp(0, width) as u128;
        let span = (self.range.max - self.range.min) as u128;
        let raw = self.range.min as u128 + (along * span + width as u128 / 2) / width as u128;
        self.range.snap(raw as u64)
    }

    /// The window column the thumb's centre sits at.
    pub fn thumb_x(&self) -> i32 {
        self.thumb_at(self.value)
    }

    fn thumb_at(&self, value: u64) -> i32 {
        let (left, width) = self.track();
        let span = (self.range.max - self.range.min) as u128;
        if span == 0 {
            return left;
        }
        let along = (value - self.range.min) as u128;
        left + ((along * width as u128 + span / 2) / span) as i32
    }

    fn knob(&self) -> Rect {
        let rect = self.layout_rect();
        let half = self.thumb / 2;
        Rect::new(
            self.thumb_x() - half,
            rect.y + rect.height / 2 - half,
            self.thumb,
            self.thumb,
        )
    }

    /// Latches the drag too: a press and its release can reach this widget
    /// before the application's answer to the press rebuilds it.
    fn emit(&mut self, input: SliderInput, sink: &mut MessageSink) -> EventResponse {
        match input {
            SliderInput::Begin(value, grab) => {
                self.value = value;
                self.grab = Some(grab);
            }
            SliderInput::Change(value) => self.value = value,
            SliderInput::End => self.grab = None,
        }
        if let Some(f) = &self.on_input {
            sink.emit_raw(f(input));
        }
        EventResponse::Consumed
    }

    fn key_value(&self, key: Key) -> Option<u64> {
        let range = &self.range;
        Some(match key {
            Key::Named(NamedKey::Left | NamedKey::Down) => range.moved(self.value, -1, false),
            Key::Named(NamedKey::Right | NamedKey::Up) => range.moved(self.value, 1, false),
            Key::Named(NamedKey::PageDown) => range.moved(self.value, -1, true),
            Key::Named(NamedKey::PageUp) => range.moved(self.value, 1, true),
            Key::Named(NamedKey::Home) => range.min,
            Key::Named(NamedKey::End) => range.max,
            _ => return None,
        })
    }
}

impl Widget for SliderWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.thumb = ctx.style.checkbox_size;
        let height = self.thumb + (ctx.style.focus_ring_width + ctx.style.focus_ring_offset) * 2;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width * 2
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let (left, width) = self.track();
        let mid = rect.y + rect.height / 2;
        let bar = 4;
        let cx = self.thumb_x();
        let knob = self.knob();
        let (fill, thumb) = if self.enabled {
            (style.bg_accent, style.text_primary)
        } else {
            (style.text_disabled, style.text_disabled)
        };
        ctx.fill_rounded_rect(left, mid - bar / 2, width, bar, bar / 2, style.bg_tertiary);
        ctx.fill_rounded_rect(left, mid - bar / 2, cx - left, bar, bar / 2, fill);

        ctx.fill_rounded_rect(
            knob.x,
            knob.y,
            knob.width,
            knob.height,
            knob.width / 2,
            thumb,
        );
        if self.enabled {
            ctx.draw_rounded_rect(
                knob.x,
                knob.y,
                knob.width,
                knob.height,
                knob.width / 2,
                style.border_default,
            );
        }
        if self.focused {
            ctx.draw_focus_ring(knob);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        if phase != EventPhase::Target && phase != EventPhase::Bubble {
            return EventResponse::Ignored;
        }
        match event {
            // Every widget the pointer missed gets the release too, and each
            // message costs a rebuild; a slider disabled mid-drag still ends it.
            WidgetEvent::PointerUp {
                button: PointerButton::Left,
                ..
            } if self.grab.is_some() => self.emit(SliderInput::End, sink),
            _ if !self.enabled => EventResponse::Ignored,
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                let value = if self.knob().contains(*x, *y) {
                    self.value
                } else {
                    self.value_at(*x)
                };
                let grab = SliderGrab(*x - self.thumb_at(value));
                self.emit(SliderInput::Begin(value, grab), sink)
            }
            WidgetEvent::PointerMove { x, .. } if self.grab.is_some() => {
                let knob = *x - self.grab.map_or(0, |g| g.0);
                // On a range with more values than pixels, the column the knob
                // already sits at maps to a value near, not at, its own.
                if knob == self.thumb_x() {
                    return EventResponse::Ignored;
                }
                let value = self.value_at(knob);
                if value == self.value {
                    return EventResponse::Ignored;
                }
                self.emit(SliderInput::Change(value), sink)
            }
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }
            WidgetEvent::KeyDown { key, .. } if self.focused => match self.key_value(*key) {
                Some(value) if value != self.value => self.emit(SliderInput::Change(value), sink),
                Some(_) => EventResponse::Consumed,
                None => EventResponse::Ignored,
            },
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Slider
    }

    fn focus_policy(&self) -> FocusPolicy {
        if self.enabled {
            FocusPolicy::StrongFocus
        } else {
            FocusPolicy::None
        }
    }
}
