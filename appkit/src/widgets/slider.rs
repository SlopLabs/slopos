use std::any::Any;

use slopos_theme_core::{State, stroke};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::focus::RING_REACH;
use crate::paint::PaintContext;
use crate::style::MIN_TARGET;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

pub(crate) const KNOB: i32 = 16;
const TRACK: i32 = 4;

/// What a slider reports; the app keeps a drag's [`SliderGrab`] from `Begin`
/// to `End`, since the tree is rebuilt between them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SliderInput {
    /// A press begins a drag: at the value under the pointer, or where the
    /// knob is for a press on it, so a click there changes nothing.
    Begin(u64, SliderGrab),
    /// The drag or a key moved the value.
    Change(u64),
    End,
}

/// Where on the knob a drag holds it, so the knob does not jump its centre to
/// the pointer.
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
        }
    }

    pub fn value(&self) -> u64 {
        self.value
    }

    /// The span the thumb's centre travels, in window coordinates.
    fn track(&self) -> (i32, i32) {
        let rect = self.layout_rect();
        (rect.x + KNOB / 2, (rect.width - KNOB).max(1))
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
        let half = KNOB / 2;
        Rect::new(
            self.thumb_x() - half,
            rect.y + rect.height / 2 - half,
            KNOB,
            KNOB,
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
        let height = (KNOB + RING_REACH * 2).max(MIN_TARGET);
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width * 2
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let theme = ctx.theme();
        let (left, width) = self.track();
        let mid = rect.y + rect.height / 2;
        let bar = TRACK;
        let cx = self.thumb_x();
        let knob = self.knob();
        let (fill, thumb) = if self.enabled {
            (theme.accent, theme.text)
        } else {
            (theme.text_disabled, theme.text_disabled)
        };
        let track = Rect::new(left, mid - bar / 2, width, bar);
        ctx.fill_rounded_rect(track, bar / 2, theme.border);
        let done = Rect::new(left, track.y, cx - left, bar);
        ctx.fill_rounded_rect(done, bar / 2, fill);

        let state = match (self.grab, ctx.lit(self.id())) {
            _ if !self.enabled => State::Normal,
            (Some(_), _) => State::Pressed,
            (None, Some(_)) => State::Hover,
            (None, None) => State::Normal,
        };
        if state != State::Normal {
            let halo = knob.inflate(RING_REACH);
            ctx.fill_circle(halo.x, halo.y, halo.width, theme.overlay(state));
        }
        ctx.fill_rounded_rect(knob, knob.width / 2, thumb);
        if self.enabled {
            let edge = theme.border_strong;
            ctx.draw_rounded_rect(knob, knob.width / 2, stroke::EDGE, edge);
        }
        if self.focused {
            ctx.draw_focus_ring(knob);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            // A release reaches every widget, so only a live drag's is taken;
            // one disabled mid-drag still ends it.
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

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        super::button::Press::hover_part(self.enabled, self.layout_rect(), x, y)
    }
}
