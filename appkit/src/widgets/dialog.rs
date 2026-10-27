use slopos_theme_core::{EdgeInsets, inset, radius, space};

use super::card::draw_shadow;
use super::scroll_view::ScrollViewWidget;
use crate::constraints::{
    BoxConstraints, MAX_EXTENT, Rect, ScrollDirection, ScrollbarVisibility, Size,
};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
    route,
};
use crate::focus::RING_ROOM;
use crate::layout::PaddingWidget;
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget};

const MAX_WIDTH: i32 = 480;
const MARGIN: i32 = space::XL;

/// A card over a scrim, holding a title, its content and the buttons that
/// answer it. It covers the page for the pointer and the keyboard: a click
/// outside it dismisses it, and Tab moves only among its controls. Content
/// taller than the window scrolls.
pub struct DialogWidget {
    core: WidgetCore,
    /// The title, the content in a scroll view, and the footer.
    parts: Vec<Box<dyn Widget>>,
    on_dismiss: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
    content_height: i32,
    card_rect: Rect,
    anchor: Option<Rect>,
    name: Option<String>,
}

impl DialogWidget {
    pub fn new(
        title: Box<dyn Widget>,
        content: Box<dyn Widget>,
        footer: Box<dyn Widget>,
        on_dismiss: Option<Box<dyn Fn() -> Box<dyn std::any::Any>>>,
    ) -> Self {
        let content = ScrollViewWidget::new(
            Box::new(PaddingWidget::new(EdgeInsets::all(RING_ROOM), content)),
            ScrollDirection::Vertical,
            ScrollbarVisibility::WhenNeeded,
        );
        Self {
            core: WidgetCore::new(),
            parts: vec![title, Box::new(content), footer],
            on_dismiss,
            content_height: 0,
            card_rect: Rect::ZERO,
            anchor: None,
            name: None,
        }
    }

    /// Centres the card over `anchor` where it fits.
    pub fn anchored(mut self, anchor: Option<Rect>) -> Self {
        self.anchor = anchor;
        self
    }

    /// Names the dialog `name`, for a title that is more than its text.
    pub fn named(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn card_rect(&self) -> Rect {
        self.card_rect
    }

    fn card_width(available: i32) -> i32 {
        MAX_WIDTH.min(available - MARGIN * 2).max(0)
    }

    fn inner_width(available: i32) -> i32 {
        (Self::card_width(available) - inset::CARD.horizontal()).max(0)
    }

    fn gaps() -> [i32; 2] {
        [space::SM, space::XL]
    }

    /// Everything but the content.
    fn frame_height(&self) -> i32 {
        let ends = self.parts[0].measured_size().height + self.parts[2].measured_size().height;
        ends + Self::gaps().iter().sum::<i32>() + inset::CARD.vertical()
    }

    fn dismiss(&self, sink: &mut MessageSink) {
        if let Some(f) = &self.on_dismiss {
            sink.emit_raw(f());
        }
    }

    fn paint_card(&self, ctx: &mut PaintContext) {
        let theme = ctx.theme();
        let rect = self.layout_rect();
        let card = self.card_rect;
        ctx.fill_rect_blended(rect.x, rect.y, rect.width, rect.height, theme.scrim);
        draw_shadow(ctx, card);
        let (x, y, w, h) = (card.x, card.y, card.width, card.height);
        ctx.fill_rounded_rect(x, y, w, h, radius::CARD, theme.surface_raised);
        ctx.draw_rounded_rect(x, y, w, h, radius::CARD, theme.border);
        ctx.with_clip(card, |ctx| {
            for part in &self.parts {
                part.paint(ctx);
            }
        });
    }
}

impl Widget for DialogWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let inner = Self::inner_width(constraints.max_width);
        let each = BoxConstraints {
            min_width: inner,
            max_width: inner,
            min_height: 0,
            max_height: MAX_EXTENT,
        };
        measure_widget(self.parts[0].as_mut(), each, ctx);
        measure_widget(self.parts[2].as_mut(), each, ctx);
        let room = (constraints.max_height - MARGIN * 2 - self.frame_height()).max(0);
        let outset = BoxConstraints {
            min_width: inner + RING_ROOM * 2,
            max_width: inner + RING_ROOM * 2,
            min_height: 0,
            max_height: room + RING_ROOM * 2,
        };
        let content = &mut self.parts[1];
        measure_widget(content.as_mut(), outset, ctx);
        let scrolled = content.children()[0].measured_size().height - RING_ROOM * 2;
        self.content_height = scrolled.min(room);
        constraints.constrain(constraints.max_size())
    }

    fn layout(&mut self, rect: Rect) {
        let card_w = Self::card_width(rect.width);
        let card_h = self.frame_height() + self.content_height;
        let over = self.anchor.and_then(|a| a.intersect(&rect)).unwrap_or(rect);
        let centred = |start: i32, extent: i32, size: i32, lo: i32, room: i32| {
            (start + (extent - size) / 2).clamp(lo, (lo + room - size).max(lo))
        };
        let card_x = centred(over.x, over.width, card_w, rect.x, rect.width);
        let card_y = centred(over.y, over.height, card_h, rect.y, rect.height);
        self.card_rect = Rect::new(card_x, card_y, card_w, card_h);

        let inner_w = Self::inner_width(rect.width);
        let x = card_x + inset::CARD.left;
        let mut y = card_y + inset::CARD.top;
        let heights = [
            self.parts[0].measured_size().height,
            self.content_height,
            self.parts[2].measured_size().height,
        ];
        let gaps = Self::gaps();
        for (i, part) in self.parts.iter_mut().enumerate() {
            let at = Rect::new(x, y, inner_w, heights[i]);
            let at = match i {
                1 => at.inflate(RING_ROOM),
                _ => at,
            };
            place_widget(part.as_mut(), at);
            y += heights[i] + gaps.get(i).copied().unwrap_or(0);
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        ctx.with_layer(self.id(), |ctx| self.paint_card(ctx));
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
            WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                ..
            } if !self.card_rect.contains(*x, *y) => {
                self.dismiss(sink);
                EventResponse::Consumed
            }
            WidgetEvent::FocusGained | WidgetEvent::FocusLost => EventResponse::Ignored,
            _ => match route(&mut self.parts, event, phase, sink) {
                EventResponse::Ignored => EventResponse::Blocked,
                taken => taken,
            },
        }
    }

    fn role(&self) -> Role {
        Role::Group
    }

    fn accessible_name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or_else(|| self.parts[0].accessible_name())
    }

    fn is_modal(&self) -> bool {
        true
    }

    /// Enter and Escape reach the footer; Escape dismisses when the footer
    /// has no button to go back with.
    fn take_unhandled_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        sink: &mut MessageSink,
    ) -> bool {
        if self.parts[2].take_unhandled_key(key, modifiers, sink) {
            return true;
        }
        let plain = !(modifiers.ctrl || modifiers.alt || modifiers.shift);
        if key != Key::Named(NamedKey::Escape) || !plain || self.on_dismiss.is_none() {
            return false;
        }
        self.dismiss(sink);
        true
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &self.parts
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.parts
    }
}
