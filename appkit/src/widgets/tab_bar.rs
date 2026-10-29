use std::any::Any;
use std::cell::Cell;

use slopos_theme_core::{ColorRole, State, TextRole, space};

use crate::constraints::{BoxConstraints, MAX_EXTENT, Rect, Size};
use crate::event::{EventResponse, Key, MessageSink, NamedKey, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{
    FocusPolicy, MeasureCtx, Role, Widget, WidgetCore, measure_widget, place_widget,
};

/// Tabs above the panel of the one chosen.
pub struct TabBarWidget {
    core: WidgetCore,
    tabs: Vec<String>,
    active: usize,
    on_change: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
    content: Vec<Box<dyn Widget>>,
    focused: bool,
    tab_height: i32,
    cut: Vec<Cell<bool>>,
}

impl TabBarWidget {
    pub fn new(
        tabs: Vec<String>,
        active: usize,
        on_change: Option<Box<dyn Fn(usize) -> Box<dyn Any>>>,
        content: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            cut: tabs.iter().map(|_| Cell::new(false)).collect(),
            tabs,
            active,
            on_change,
            content,
            focused: false,
            tab_height: 0,
        }
    }

    fn panel_rect(&self) -> Rect {
        let rect = self.layout_rect();
        Rect::new(
            rect.x,
            rect.y + self.tab_height,
            rect.width,
            (rect.height - self.tab_height).max(0),
        )
    }

    /// Each tab's offset and width, the last taking what division leaves.
    fn tab_layout(&self, total_width: i32) -> Vec<(i32, i32)> {
        let count = self.tabs.len() as i32;
        (0..count)
            .map(|i| {
                let each = total_width / count;
                let w = if i == count - 1 {
                    total_width - each * i
                } else {
                    each
                };
                (i * each, w)
            })
            .collect()
    }

    fn tab_at(&self, x: i32, y: i32) -> Option<usize> {
        let rect = self.layout_rect();
        if y < rect.y || y >= rect.y + self.tab_height {
            return None;
        }
        let rel_x = x - rect.x;
        self.tab_layout(rect.width)
            .iter()
            .position(|&(at, w)| rel_x >= at && rel_x < at + w)
    }

    fn panel_event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match self.content.get_mut(self.active) {
            Some(panel) => panel.event(event, sink),
            None => EventResponse::Ignored,
        }
    }

    fn switch_to(&mut self, tab: usize, sink: &mut MessageSink) -> EventResponse {
        if tab != self.active {
            self.active = tab;
            if let Some(cb) = &self.on_change {
                sink.emit_raw(cb(tab));
            }
        }
        EventResponse::Consumed
    }
}

impl Widget for TabBarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let tab_height = ctx.style.tab_height;
        self.tab_height = tab_height;
        let w = constraints.max_width;

        let panel_constraints = BoxConstraints {
            min_width: w,
            max_width: w,
            min_height: 0,
            max_height: if constraints.is_height_bounded() {
                (constraints.max_height - tab_height).max(0)
            } else {
                MAX_EXTENT
            },
        };

        // Measure every panel so a tab switch needs no fresh measure pass.
        let mut panel_height = 0;
        for (i, panel) in self.content.iter_mut().enumerate() {
            let size = measure_widget(panel.as_mut(), panel_constraints, ctx);
            if i == self.active {
                panel_height = size.height;
            }
        }

        constraints.constrain(Size::new(w, tab_height + panel_height))
    }

    fn layout(&mut self, _rect: Rect) {
        let panel_rect = self.panel_rect();
        for panel in &mut self.content {
            place_widget(panel.as_mut(), panel_rect);
        }
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let tab_height = self.tab_height;
        let theme = ctx.theme();

        ctx.fill_rect(rect.x, rect.y, rect.width, tab_height, theme.surface_sunken);

        let layout = self.tab_layout(rect.width);
        let ty = ui::centred_top(rect.y, tab_height, TextRole::Body);
        let lit = ctx.lit(self.id());

        for (i, (tab_x, tab_w)) in layout.iter().enumerate() {
            let abs_x = rect.x + tab_x;

            if i == self.active {
                ctx.fill_rect(abs_x, rect.y, *tab_w, tab_height, theme.surface);

                let indicator = super::INDICATOR;
                let y = rect.y + tab_height - indicator;
                ctx.fill_rect(abs_x, y, *tab_w, indicator, theme.accent);
            } else if lit == Some(i) {
                let hover = theme.state(ColorRole::SurfaceSunken, State::Hover);
                ctx.fill_rect(abs_x, rect.y, *tab_w, tab_height, hover);
            }

            let label = &self.tabs[i];
            let room = *tab_w - space::SM * 2;
            let text_w = ui::width(label, TextRole::Body).min(room);
            let tx = abs_x + (*tab_w - text_w) / 2;
            let fg = if i == self.active {
                theme.text
            } else {
                theme.text_secondary
            };
            let cut = ctx.draw_text_fitted(tx, ty, label, TextRole::Body, room, fg);
            self.cut[i].set(cut);
        }

        ctx.fill_rect(rect.x, rect.y + tab_height, rect.width, 1, theme.border);

        if let Some(panel) = self.content.get(self.active) {
            let panel_rect = self.panel_rect();
            ctx.with_clip(panel_rect, |ctx| panel.paint(ctx));
        }

        if self.focused {
            // Clear of the indicator, which is focus-coloured and ring-thick.
            let tabs = Rect::new(rect.x, rect.y, rect.width, tab_height - super::INDICATOR);
            ctx.draw_inner_focus_ring(tabs);
        }
    }

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        match event {
            WidgetEvent::PointerDown { x, y, .. } => {
                let rect = self.layout_rect();
                if *y < rect.y || *y >= rect.y + self.tab_height {
                    return self.panel_event(event, sink);
                }
                match self.tab_at(*x, *y) {
                    Some(tab) => self.switch_to(tab, sink),
                    None => EventResponse::Ignored,
                }
            }

            WidgetEvent::KeyDown { key, .. } if self.focused => match key {
                Key::Named(NamedKey::Left) if self.active > 0 => {
                    self.switch_to(self.active - 1, sink)
                }
                Key::Named(NamedKey::Right) if self.active + 1 < self.tabs.len() => {
                    self.switch_to(self.active + 1, sink)
                }
                _ => self.panel_event(event, sink),
            },

            WidgetEvent::FocusGained => {
                self.focused = true;
                EventResponse::Ignored
            }
            WidgetEvent::FocusLost => {
                self.focused = false;
                EventResponse::Ignored
            }

            _ => self.panel_event(event, sink),
        }
    }

    fn role(&self) -> Role {
        Role::Tab
    }

    fn focus_policy(&self) -> FocusPolicy {
        match self.tabs.is_empty() {
            true => FocusPolicy::None,
            false => FocusPolicy::TabFocus,
        }
    }

    fn hover_part(&self, x: i32, y: i32) -> Option<usize> {
        self.tab_at(x, y)
    }

    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        let tab = self.tab_at(x, y)?;
        self.cut[tab].get().then_some(self.tabs[tab].as_str())
    }

    /// Only the shown panel: every panel shares one rect, so a hit test would
    /// otherwise find the last.
    fn children(&self) -> &[Box<dyn Widget>] {
        self.content
            .get(self.active)
            .map(core::slice::from_ref)
            .unwrap_or(&[])
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        let active = self.active;
        self.content
            .get_mut(active)
            .map(core::slice::from_mut)
            .unwrap_or(&mut [])
    }
}
