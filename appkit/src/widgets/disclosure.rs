use std::any::Any;

use slopos_theme_core::{Icon, State, TextRole, radius, space};

use super::button::Press;
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

const ROLE: TextRole = TextRole::Body;
const PAD: i32 = space::XS;

/// A chevron and a title that open and close what is below them.
pub struct DisclosureWidget {
    core: WidgetCore,
    title: String,
    expanded: bool,
    on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
    /// The content, while expanded.
    content: Vec<Box<dyn Widget>>,
    press: Press,
    header: Rect,
}

impl DisclosureWidget {
    pub fn new(
        title: String,
        expanded: bool,
        on_toggle: Option<Box<dyn Fn() -> Box<dyn Any>>>,
        content: Option<Box<dyn Widget>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            title,
            expanded,
            on_toggle,
            content: content.into_iter().collect(),
            press: Press::default(),
            header: Rect::ZERO,
        }
    }

    fn header_height() -> i32 {
        ROLE.style().line_height + PAD * 2
    }

    fn indent() -> i32 {
        ROLE.style().icon + space::SM
    }

    fn header_width(&self) -> i32 {
        PAD * 2 + Self::indent() + ui::role_width(&self.title, ROLE)
    }

    fn title_room(&self) -> i32 {
        self.header.width - PAD * 2 - Self::indent()
    }
}

impl Widget for DisclosureWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let mut size = Size::new(self.header_width(), Self::header_height());
        if let Some(content) = self.content.first_mut() {
            let inner = constraints
                .loosen()
                .deflate(slopos_theme_core::EdgeInsets::new(
                    0,
                    0,
                    0,
                    Self::indent() + PAD,
                ));
            let c = measure_widget(content.as_mut(), inner, ctx);
            size.width = size.width.max(c.width + Self::indent() + PAD);
            size.height += space::XS + c.height;
        }
        if constraints.is_width_bounded() {
            size.width = constraints.max_width;
        }
        constraints.constrain(size)
    }

    fn layout(&mut self, rect: Rect) {
        let header_w = self.header_width().min(rect.width);
        self.header = Rect::new(rect.x, rect.y, header_w, Self::header_height());
        let x = rect.x + Self::indent() + PAD;
        let y = rect.y + Self::header_height() + space::XS;
        if let Some(content) = self.content.first_mut() {
            let h = content.measured_size().height;
            place_widget(
                content.as_mut(),
                Rect::new(x, y, (rect.x + rect.width - x).max(0), h),
            );
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let h = self.header;
        let theme = ctx.theme();
        let state = self.press.state(h, ctx);
        if state != State::Normal {
            ctx.fill_rounded_wash(h, radius::CONTROL, theme.overlay(state));
        }
        let size = ROLE.style().icon;
        let chevron = if self.expanded {
            Icon::ChevronDown
        } else {
            Icon::ChevronRight
        };
        let x = h.x + PAD;
        ctx.draw_icon(
            chevron,
            x,
            h.y + (h.height - size) / 2,
            size,
            theme.text_secondary,
        );
        let top = h.y + (h.height - ui::role_line_height(ROLE)) / 2;
        let room = self.title_room();
        ctx.draw_text_fitted(x + Self::indent(), top, &self.title, ROLE, room, theme.text);
        if self.press.focused {
            ctx.draw_focus_ring(h);
        }
        for content in &self.content {
            content.paint(ctx);
        }
    }

    fn event(
        &mut self,
        event: &WidgetEvent,
        phase: EventPhase,
        sink: &mut MessageSink,
    ) -> EventResponse {
        let (response, pressed) = self.press.event(self.header, true, event, phase);
        if pressed {
            if let Some(f) = &self.on_toggle {
                sink.emit_raw(f());
            }
        }
        if response.is_consumed() {
            return response;
        }
        if matches!(event, WidgetEvent::FocusGained | WidgetEvent::FocusLost) {
            return response;
        }
        for content in &mut self.content {
            let r = content.event(event, phase, sink);
            if r.is_consumed() {
                return r;
            }
        }
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::Button
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.title)
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        let cut = ui::role_width(&self.title, ROLE) > self.title_room();
        (cut && self.header.contains(x, y)).then_some(&self.title)
    }

    fn baseline(&self) -> Option<i32> {
        Some((Self::header_height() - ui::role_line_height(ROLE)) / 2 + ui::role_ascent(ROLE))
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::StrongFocus
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.content
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.content
    }
}
