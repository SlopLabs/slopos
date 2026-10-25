use std::any::Any;

use slopos_theme_core::space;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent,
};
use crate::node::RadioOption;
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

/// Options of which one is chosen, one tab stop for the whole group.
///
/// Selection follows the arrows, as WAI-ARIA's radio group has it, and skips
/// disabled options; Space chooses the first enabled one while none is chosen.
pub struct RadioGroupWidget {
    core: WidgetCore,
    options: Vec<RadioOption>,
    selected: Option<usize>,
    on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    focused: bool,
    /// Each option's top and height within the group, and the label line's
    /// height, from the last measure.
    rows: Vec<(i32, i32)>,
    label_height: i32,
}

impl RadioGroupWidget {
    pub fn new(
        options: Vec<RadioOption>,
        selected: Option<usize>,
        on_select: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    ) -> Self {
        let selected = selected.filter(|&i| i < options.len());
        Self {
            core: WidgetCore::new(),
            options,
            selected,
            on_select,
            focused: false,
            rows: Vec::new(),
            label_height: 0,
        }
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    fn enabled(&self, index: usize) -> bool {
        self.options.get(index).is_some_and(|o| o.enabled)
    }

    /// The next enabled option after `from` going forward or back, wrapping.
    fn step(&self, from: Option<usize>, forward: bool) -> Option<usize> {
        let len = self.options.len();
        let start = match from {
            Some(i) => i,
            None if forward => len.checked_sub(1)?,
            None => 0,
        };
        (1..=len)
            .map(|k| {
                if forward {
                    (start + k) % len
                } else {
                    (start + len - k % len) % len
                }
            })
            .find(|&i| self.enabled(i))
    }

    fn choose(&mut self, index: usize, sink: &mut MessageSink) -> EventResponse {
        if self.selected != Some(index) {
            self.selected = Some(index);
            if let Some(f) = &self.on_select {
                sink.emit_raw(f(index));
            }
        }
        EventResponse::Consumed
    }

    fn row_at(&self, x: i32, y: i32) -> Option<usize> {
        let rect = self.layout_rect();
        if !rect.contains(x, y) {
            return None;
        }
        let dy = y - rect.y;
        self.rows
            .iter()
            .position(|&(top, height)| dy >= top && dy < top + height)
    }
}

impl Widget for RadioGroupWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let style = ctx.style;
        let text_h = ctx.text_height();
        self.label_height = style.checkbox_size.max(text_h);
        let indent = style.checkbox_size + style.checkbox_gap;

        self.rows.clear();
        let mut y = 0;
        let mut width = 0;
        for (i, option) in self.options.iter().enumerate() {
            if i > 0 {
                y += space::SM;
            }
            let mut height = self.label_height;
            let mut text_w = ctx.text_width(&option.label);
            if !option.detail.is_empty() {
                height += text_h;
                text_w = text_w.max(ctx.text_width(&option.detail));
            }
            self.rows.push((y, height));
            y += height;
            width = width.max(indent + text_w);
        }
        constraints.constrain(Size::new(width, y))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let theme = ctx.theme();
        let size = style.checkbox_size;
        let text_h = ctx.text_height();
        let cursor = self.selected.or_else(|| self.step(None, true));

        for (i, (option, &(top, _))) in self.options.iter().zip(&self.rows).enumerate() {
            let row_y = rect.y + top;
            let dot = Rect::new(rect.x, row_y + (self.label_height - size) / 2, size, size);
            let (ring, fill) = if option.enabled {
                (theme.border_strong, theme.accent)
            } else {
                (theme.text_disabled, theme.text_disabled)
            };
            if self.selected == Some(i) {
                ctx.fill_rounded_rect(dot.x, dot.y, size, size, size / 2, fill);
                let inner = size * 3 / 8;
                let at = (size - inner) / 2;
                ctx.fill_rounded_rect(
                    dot.x + at,
                    dot.y + at,
                    inner,
                    inner,
                    inner / 2,
                    theme.text_on_accent,
                );
            } else {
                ctx.draw_rounded_rect(dot.x, dot.y, size, size, size / 2, ring);
            }

            let text_x = rect.x + size + style.checkbox_gap;
            let label_fg = if option.enabled {
                theme.text
            } else {
                theme.text_disabled
            };
            let label_y = row_y + (self.label_height - text_h) / 2;
            ctx.draw_text_transparent(text_x, label_y, &option.label, label_fg);
            if !option.detail.is_empty() {
                ctx.draw_text_transparent(
                    text_x,
                    row_y + self.label_height,
                    &option.detail,
                    theme.text_secondary,
                );
            }

            if self.focused && cursor == Some(i) {
                ctx.draw_focus_ring(dot);
            }
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
        match event {
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } => match self.row_at(*x, *y) {
                Some(i) if self.enabled(i) => self.choose(i, sink),
                Some(_) => EventResponse::Consumed,
                None => EventResponse::Ignored,
            },
            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }
            WidgetEvent::KeyDown { key, .. } if self.focused => {
                let next = match key {
                    Key::Named(NamedKey::Down | NamedKey::Right) => self.step(self.selected, true),
                    Key::Named(NamedKey::Up | NamedKey::Left) => self.step(self.selected, false),
                    Key::Named(NamedKey::Space) => self.selected.or_else(|| self.step(None, true)),
                    _ => return EventResponse::Ignored,
                };
                match next {
                    Some(i) => self.choose(i, sink),
                    None => EventResponse::Ignored,
                }
            }
            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::RadioGroup
    }

    /// A group with nothing to choose is no tab stop, as a disabled control
    /// is not.
    fn focus_policy(&self) -> FocusPolicy {
        if self.options.iter().any(|o| o.enabled) {
            FocusPolicy::StrongFocus
        } else {
            FocusPolicy::None
        }
    }
}
