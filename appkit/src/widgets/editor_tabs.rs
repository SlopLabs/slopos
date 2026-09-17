//! The row of open-file tabs.
//!
//! Distinct from [`super::tab_bar::TabBarWidget`], which owns its pages and
//! switches between them: these tabs name documents the application owns, carry
//! a modified marker and a close affordance, and report clicks rather than
//! deciding anything.

use std::any::Any;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, PointerButton, WidgetEvent};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

use super::icon::{IconKind, draw_icon};
use super::tree_view::elide;

const TAB_PAD_H: i32 = 12;
const CLOSE_SIZE: i32 = 12;
const CLOSE_GAP: i32 = 8;
const MIN_TAB_WIDTH: i32 = 90;
const MAX_TAB_WIDTH: i32 = 220;

#[derive(Clone, Debug)]
pub struct EditorTab {
    pub title: String,
    pub modified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TabInput {
    Select(usize),
    Close(usize),
}

type InputCallback = Box<dyn Fn(TabInput) -> Box<dyn Any>>;

pub struct EditorTabsWidget {
    core: WidgetCore,
    tabs: Vec<EditorTab>,
    active: usize,
    on_input: Option<InputCallback>,
    hovered: Option<usize>,
    /// Widths from the last paint; hit testing must agree with what was drawn.
    widths: Vec<i32>,
    height: i32,
}

impl EditorTabsWidget {
    pub fn new(tabs: Vec<EditorTab>, active: usize, on_input: Option<InputCallback>) -> Self {
        Self {
            core: WidgetCore::new(),
            tabs,
            active,
            on_input,
            hovered: None,
            widths: Vec::new(),
            height: 34,
        }
    }

    fn compute_widths(&mut self, ctx_width: i32, text_width: impl Fn(&str) -> i32) {
        self.widths.clear();
        if self.tabs.is_empty() {
            return;
        }
        let mut total = 0;
        for tab in &self.tabs {
            let natural = TAB_PAD_H * 2 + text_width(&tab.title) + CLOSE_GAP + CLOSE_SIZE;
            let width = natural.clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH);
            self.widths.push(width);
            total += width;
        }
        // Tabs shrink together rather than scrolling, so every tab the keyboard
        // switcher can reach is on screen. Shared out exactly rather than
        // scaled: a floor under the scaled width overflows the row once there
        // are enough tabs, and the ones past the edge are then unclickable.
        if total > ctx_width && ctx_width > 0 && !self.widths.is_empty() {
            let count = self.widths.len() as i32;
            let each = ctx_width / count;
            let last = ctx_width - each * (count - 1);
            for (index, width) in self.widths.iter_mut().enumerate() {
                *width = if index as i32 == count - 1 {
                    last
                } else {
                    each
                };
            }
        }
    }

    fn tab_at(&self, x: i32) -> Option<usize> {
        let rect = self.layout_rect();
        let mut cursor = rect.x;
        for (index, width) in self.widths.iter().enumerate() {
            if x >= cursor && x < cursor + width {
                return Some(index);
            }
            cursor += width;
        }
        None
    }

    fn close_rect_x(&self, index: usize) -> Option<i32> {
        let rect = self.layout_rect();
        let mut cursor = rect.x;
        for (i, width) in self.widths.iter().enumerate() {
            if i == index {
                return Some(cursor + width - TAB_PAD_H - CLOSE_SIZE);
            }
            cursor += width;
        }
        None
    }

    fn emit(&self, input: TabInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }
}

impl Widget for EditorTabsWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.height = ctx.style.tab_height;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            MAX_TAB_WIDTH * self.tabs.len() as i32
        };
        let font = ctx.style.font_size as u16;
        self.compute_widths(width, |t| crate::text::ui::width(t, font));
        constraints.constrain(Size::new(width, self.height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        // A tab row narrower than its tabs still draws only inside itself.
        ctx.with_clip(rect, |ctx| {
            let style = ctx.style;
            ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, style.bg_secondary);

            let text_h = ctx.text_height();
            let mut x = rect.x;
            // From the live pointer, not a field the last rebuild cleared: the
            // glyph must be drawn exactly where clicking it closes the tab.
            let (px, py) = ctx.pointer;
            let pointer_tab = rect.contains(px, py).then(|| self.tab_at(px)).flatten();

            for (index, tab) in self.tabs.iter().enumerate() {
                let width = *self.widths.get(index).unwrap_or(&MIN_TAB_WIDTH);
                let active = index == self.active;
                let hovered = pointer_tab == Some(index);

                if active {
                    ctx.fill_rect(x, rect.y, width, rect.height, style.code_bg);
                    ctx.fill_rect(x, rect.y, width, 2, style.text_accent);
                } else if hovered {
                    ctx.fill_rect(x, rect.y, width, rect.height, style.bg_hover);
                }
                ctx.fill_rect(
                    x + width - 1,
                    rect.y + 6,
                    1,
                    rect.height - 12,
                    style.border_divider,
                );

                let label_color = if active {
                    style.text_primary
                } else {
                    style.text_secondary
                };
                let label_budget = width - TAB_PAD_H * 2 - CLOSE_GAP - CLOSE_SIZE;
                let label = elide(&tab.title, label_budget, |t| ctx.text_width(t));
                let label_y = rect.y + (rect.height - text_h) / 2;
                ctx.draw_text_transparent(x + TAB_PAD_H, label_y, &label, label_color);

                let icon_x = x + width - TAB_PAD_H - CLOSE_SIZE;
                let icon_y = rect.y + (rect.height - CLOSE_SIZE) / 2;
                // One slot: the dot gives way to the close affordance on hover.
                if tab.modified && !hovered {
                    draw_icon(
                        ctx,
                        IconKind::Dot,
                        icon_x,
                        icon_y,
                        CLOSE_SIZE,
                        style.text_accent,
                    );
                } else if hovered || active {
                    draw_icon(
                        ctx,
                        IconKind::Close,
                        icon_x,
                        icon_y,
                        CLOSE_SIZE,
                        style.text_secondary,
                    );
                }

                x += width;
            }

            if x < rect.x + rect.width {
                ctx.fill_rect(
                    x,
                    rect.y,
                    rect.x + rect.width - x,
                    rect.height,
                    style.bg_secondary,
                );
            }
            ctx.fill_rect(
                rect.x,
                rect.y + rect.height - 1,
                rect.width,
                1,
                style.border_divider,
            );
        });
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
            WidgetEvent::PointerDown { x, y, button, .. } => {
                if !self.layout_rect().contains(*x, *y) {
                    return EventResponse::Ignored;
                }
                let Some(index) = self.tab_at(*x) else {
                    return EventResponse::Ignored;
                };
                if *button == PointerButton::Middle {
                    return self.emit(TabInput::Close(index), sink);
                }
                if *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                // `index` came from the press's own x, so the glyph is drawn.
                // It is a box: bounding only x closes from the tab's top and
                // bottom pixels.
                if let Some(close_x) = self.close_rect_x(index) {
                    let strip = self.layout_rect();
                    let close_y = strip.y + (strip.height - CLOSE_SIZE) / 2;
                    if *x >= close_x
                        && *x < close_x + CLOSE_SIZE
                        && *y >= close_y
                        && *y < close_y + CLOSE_SIZE
                    {
                        return self.emit(TabInput::Close(index), sink);
                    }
                }
                self.emit(TabInput::Select(index), sink)
            }

            WidgetEvent::PointerMove { x, y } => {
                let hovered = if self.layout_rect().contains(*x, *y) {
                    self.tab_at(*x)
                } else {
                    None
                };
                if hovered != self.hovered {
                    self.hovered = hovered;
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }

            WidgetEvent::PointerLeave => {
                self.hovered = None;
                EventResponse::Ignored
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::Tab
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
