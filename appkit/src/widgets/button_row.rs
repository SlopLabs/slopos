use std::any::Any;

use slopos_theme_core::space;

use crate::constraints::{BoxConstraints, MAX_EXTENT, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, WidgetEvent, route,
};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget};

type Emit = Box<dyn Fn() -> Box<dyn Any>>;

/// The buttons that end a page: `leading` from the left edge, the rest from
/// the right, on one row when they fit and the leading ones above otherwise.
pub struct ButtonRowWidget {
    core: WidgetCore,
    buttons: Vec<Box<dyn Widget>>,
    leading: usize,
    /// What Enter presses.
    on_enter: Option<Emit>,
    /// The way back Escape presses, `Some(None)` while it is disabled.
    on_escape: Option<Option<Emit>>,
    stacked: bool,
}

impl ButtonRowWidget {
    pub fn new(
        buttons: Vec<Box<dyn Widget>>,
        leading: usize,
        on_enter: Option<Emit>,
        on_escape: Option<Option<Emit>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            buttons,
            leading,
            on_enter,
            on_escape,
            stacked: false,
        }
    }

    fn span(widths: impl Iterator<Item = i32>) -> i32 {
        let mut total = 0;
        let mut n = 0;
        for w in widths {
            total += w;
            n += 1;
        }
        total + space::SM * (n - 1).max(0)
    }

    fn group_width(&self, from: usize, to: usize) -> i32 {
        Self::span(
            self.buttons[from..to]
                .iter()
                .map(|b| b.measured_size().width),
        )
    }

    fn height(&self) -> i32 {
        self.buttons
            .iter()
            .map(|b| b.measured_size().height)
            .max()
            .unwrap_or(0)
    }

    /// Lays `from..to` out in `rect`, from its left or its right, each button
    /// as wide as it asked or an even share when they do not all fit.
    fn place(&mut self, from: usize, to: usize, rect: Rect, from_right: bool) {
        let n = (to - from) as i32;
        if n == 0 {
            return;
        }
        let natural = self.group_width(from, to);
        let share = (rect.width - space::SM * (n - 1)) / n;
        let width = |b: &dyn Widget| {
            if natural <= rect.width {
                b.measured_size().width
            } else {
                share.max(0)
            }
        };
        let total = Self::span(self.buttons[from..to].iter().map(|b| width(b.as_ref())));
        let mut x = if from_right {
            rect.x + rect.width - total
        } else {
            rect.x
        };
        for b in &mut self.buttons[from..to] {
            let w = width(b.as_ref());
            place_widget(b.as_mut(), Rect::new(x, rect.y, w, rect.height));
            x += w + space::SM;
        }
    }
}

impl Widget for ButtonRowWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let loose = BoxConstraints {
            min_width: 0,
            max_width: MAX_EXTENT,
            min_height: 0,
            max_height: constraints.max_height,
        };
        for b in &mut self.buttons {
            measure_widget(b.as_mut(), loose, ctx);
        }
        let (lead, trail) = (
            self.group_width(0, self.leading),
            self.group_width(self.leading, self.buttons.len()),
        );
        let gap = if lead > 0 && trail > 0 { space::XL } else { 0 };
        let one_row = lead + gap + trail;
        self.stacked = gap > 0 && constraints.is_width_bounded() && one_row > constraints.max_width;
        let h = self.height();
        let (width, height) = if self.stacked {
            (lead.max(trail), h * 2 + space::SM)
        } else {
            (one_row, h)
        };
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            width
        };
        constraints.constrain(Size::new(width, height))
    }

    fn layout(&mut self, rect: Rect) {
        let h = self.height();
        let all = self.buttons.len();
        if self.stacked {
            self.place(
                0,
                self.leading,
                Rect::new(rect.x, rect.y, rect.width, h),
                false,
            );
            let below = Rect::new(rect.x, rect.y + h + space::SM, rect.width, h);
            self.place(self.leading, all, below, true);
        } else {
            let row = Rect::new(rect.x, rect.y, rect.width, h);
            self.place(0, self.leading, row, false);
            self.place(self.leading, all, row, true);
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        for b in &self.buttons {
            b.paint(ctx);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        route(&mut self.buttons, event, phase, sink)
    }

    fn take_unhandled_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        sink: &mut MessageSink,
    ) -> bool {
        if modifiers.ctrl || modifiers.alt || modifiers.shift {
            return false;
        }
        match key {
            Key::Named(NamedKey::Enter) => {
                let Some(f) = &self.on_enter else {
                    return false;
                };
                sink.emit_raw(f());
                true
            }
            Key::Named(NamedKey::Escape) => {
                let Some(back) = &self.on_escape else {
                    return false;
                };
                if let Some(f) = back {
                    sink.emit_raw(f());
                }
                true
            }
            _ => false,
        }
    }

    fn role(&self) -> Role {
        Role::Toolbar
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.buttons
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.buttons
    }
}
