use std::any::Any;
use std::hash::{Hash, Hasher};

use slopos_theme_core::TextRole;

use super::scrollbar::Bar;
use crate::audit::Arrangement;
use crate::constraints::{BoxConstraints, MAX_EXTENT, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{
    FocusPolicy, Kept, KeptAt, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// A view down a child taller than it, with a bar down its side while the
/// child overflows.
pub struct ScrollViewWidget {
    core: WidgetCore,
    child: Box<dyn Widget>,
    offset: i32,
    content_size: Size,
    viewport_size: Size,
    bar_width: i32,
    thumb_min: i32,
    dragging: bool,
    focused: bool,
    follow_end: bool,
    on_scroll: Option<Box<dyn Fn(i32) -> Box<dyn Any>>>,
}

impl ScrollViewWidget {
    pub fn new(child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            child,
            offset: 0,
            content_size: Size::ZERO,
            viewport_size: Size::ZERO,
            bar_width: 0,
            thumb_min: 0,
            dragging: false,
            focused: false,
            follow_end: false,
            on_scroll: None,
        }
    }

    pub fn following_end(mut self, follow_end: bool) -> Self {
        self.follow_end = follow_end;
        self
    }

    pub fn with_scroll(
        child: Box<dyn Widget>,
        scroll_y: i32,
        on_scroll: Option<Box<dyn Fn(i32) -> Box<dyn Any>>>,
    ) -> Self {
        let mut sv = Self::new(child);
        sv.offset = scroll_y;
        sv.on_scroll = on_scroll;
        sv
    }

    fn max_offset(&self) -> i32 {
        (self.content_size.height - self.viewport_size.height).max(0)
    }

    fn bar(&self) -> Option<Bar> {
        let (shown, total) = (self.viewport_size.height, self.content_size.height);
        let (width, min) = (self.bar_width, self.thumb_min);
        let offset = self.offset as i64;
        Bar::needed(
            self.layout_rect(),
            width,
            min,
            shown as i64,
            total as i64,
            offset,
        )
    }

    fn drag_to(&mut self, y: i32, sink: &mut MessageSink) {
        if let Some(bar) = self.bar() {
            let to = bar.offset_at(y) as i32;
            self.scroll_by(to - self.offset, sink);
        }
    }

    fn scroll_by(&mut self, dy: i32, sink: &mut MessageSink) -> EventResponse {
        let before = self.offset;
        self.offset = (self.offset + dy).clamp(0, self.max_offset());
        if self.offset == before {
            return EventResponse::Ignored;
        }
        self.scrolled(sink);
        EventResponse::Consumed
    }

    fn scrolled(&mut self, sink: &mut MessageSink) {
        self.place_child();
        if let Some(cb) = &self.on_scroll {
            let at_end = self.follow_end && self.offset > 0 && self.offset == self.max_offset();
            sink.emit_raw(cb(if at_end { i32::MAX } else { self.offset }));
        }
    }

    fn place_child(&mut self) {
        let rect = self.layout_rect();
        place_widget(
            self.child.as_mut(),
            Rect::new(
                rect.x,
                rect.y - self.offset,
                self.content_size.width,
                self.content_size.height,
            ),
        );
    }
}

impl Widget for ScrollViewWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn reveal(&mut self, rect: Rect, sink: &mut MessageSink) -> Rect {
        let view = self.layout_rect();
        let below = (rect.y + rect.height) - (view.y + view.height);
        let above = rect.y - view.y;
        let wanted = if above < 0 || rect.height > view.height {
            above
        } else {
            below.max(0)
        };
        let before = self.offset;
        self.offset = (self.offset + wanted).clamp(0, self.max_offset());
        let moved = self.offset - before;
        if moved != 0 {
            self.scrolled(sink);
        }
        Rect::new(rect.x, rect.y - moved, rect.width, rect.height)
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.bar_width = ctx.style.scrollbar_width;
        self.thumb_min = ctx.style.scrollbar_thumb_min;
        let child_constraints = BoxConstraints {
            min_height: 0,
            max_height: MAX_EXTENT,
            ..constraints
        };
        self.content_size = measure_widget(self.child.as_mut(), child_constraints, ctx);
        // Beside the bar rather than under it, where a click reaches it.
        let overflows = self.content_size.height > constraints.max_height;
        if overflows && constraints.is_width_bounded() {
            let width = (constraints.max_width - self.bar_width).max(0);
            let beside = BoxConstraints {
                min_width: child_constraints.min_width.min(width),
                max_width: width,
                ..child_constraints
            };
            self.content_size = measure_widget(self.child.as_mut(), beside, ctx);
        }
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, rect: Rect) {
        self.viewport_size = Size::new(rect.width, rect.height);
        self.offset = self.offset.clamp(0, self.max_offset());
        self.place_child();
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let viewport = self.layout_rect();
        ctx.with_viewport(viewport, |ctx| self.child.paint(ctx));
        if let Some(bar) = self.bar() {
            bar.paint(ctx, self.id(), self.dragging);
        }
        if self.focused {
            ctx.draw_inner_focus_ring(viewport);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::Scroll { delta_y, .. } => match self.child.event(event, sink) {
                EventResponse::Ignored => self.scroll_by(*delta_y, sink),
                taken => taken,
            },

            WidgetEvent::PointerDown { x, y, button, .. } => {
                match self.bar().filter(|bar| bar.grabs(*x, *y)) {
                    Some(_) if *button == PointerButton::Left => {
                        self.dragging = true;
                        self.drag_to(*y, sink);
                        EventResponse::Consumed
                    }
                    Some(_) => EventResponse::Ignored,
                    None => self.child.event(event, sink),
                }
            }

            WidgetEvent::PointerMove { y, .. } if self.dragging => {
                self.drag_to(*y, sink);
                EventResponse::Consumed
            }

            WidgetEvent::PointerUp { .. } if self.dragging => {
                self.dragging = false;
                EventResponse::Consumed
            }

            WidgetEvent::KeyDown { key, .. } if self.focused => {
                let line = TextRole::Body.style().line_height;
                let page = self.viewport_size.height;
                let dy = match key {
                    Key::Named(NamedKey::Up) => -line,
                    Key::Named(NamedKey::Down) => line,
                    Key::Named(NamedKey::PageUp) => -page,
                    Key::Named(NamedKey::PageDown) => page,
                    Key::Named(NamedKey::Home) => -self.offset,
                    Key::Named(NamedKey::End) => self.max_offset() - self.offset,
                    _ => return EventResponse::Ignored,
                };
                self.scroll_by(dy, sink)
            }

            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }

            _ => self.child.event(event, sink),
        }
    }

    fn role(&self) -> Role {
        Role::ScrollArea
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.bar()?.hover(x, y)
    }

    /// What it shows is known by the first name in it: a page's first
    /// heading, a list's first row.
    fn kept(&self) -> Option<Kept> {
        fn first_name(widget: &dyn Widget) -> Option<&str> {
            let inner = || {
                widget
                    .children()
                    .iter()
                    .find_map(|c| first_name(c.as_ref()))
            };
            widget.accessible_name().or_else(inner)
        }
        if self.on_scroll.is_some() {
            return None;
        }
        let mut shows = std::hash::DefaultHasher::new();
        first_name(self.child.as_ref()).hash(&mut shows);
        Some(Kept {
            shows: shows.finish(),
            at: KeptAt::Scroll(self.offset),
        })
    }

    fn keep(&mut self, at: KeptAt) {
        if let (KeptAt::Scroll(y), None) = (at, &self.on_scroll) {
            self.offset = y;
        }
    }

    /// A tab stop only while there is something to scroll to.
    fn focus_policy(&self) -> FocusPolicy {
        if self.max_offset() > 0 {
            FocusPolicy::TabFocus
        } else {
            FocusPolicy::None
        }
    }

    fn arrangement(&self) -> Arrangement {
        Arrangement::Scroll
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
