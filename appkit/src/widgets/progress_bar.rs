use std::cell::Cell;

use slopos_theme_core::{TextRole, Tone, space};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

const SWEEP_MS: u64 = 2000;

/// Where the indeterminate block is at `ms`, as an offset into a bar of
/// `width` and a length: a quarter of the bar, going to and fro.
pub fn sweep(width: i32, ms: u64) -> (i32, i32) {
    let width = width.max(0);
    let len = (width / 4).max(1).min(width);
    let travel = (width - len) as u64;
    let half = SWEEP_MS / 2;
    let phase = ms % SWEEP_MS;
    let along = if phase < half {
        phase
    } else {
        SWEEP_MS - phase
    };
    ((travel * along / half) as i32, len)
}

pub struct ProgressBarWidget {
    core: WidgetCore,
    value: Option<u32>,
    label: String,
    tone: Tone,
    cut: Cell<bool>,
}

impl ProgressBarWidget {
    pub fn new(value: Option<u32>, label: String, tone: Tone) -> Self {
        Self {
            core: WidgetCore::new(),
            value: value.map(|v| v.min(100)),
            label,
            tone,
            cut: Cell::new(false),
        }
    }
}

impl Widget for ProgressBarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let height = TextRole::Body.style().line_height + space::XS;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            let label = ui::width(&self.label, TextRole::Body) + space::SM * 2;
            ctx.style.field_min_width.max(label)
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();

        let theme = ctx.theme();
        ctx.fill_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            theme.surface_sunken,
        );

        let (at, len) = match self.value {
            Some(value) => (0, rect.width * value as i32 / 100),
            None => sweep(rect.width, slopos_windowing::get_time_ms()),
        };
        let filled = Rect::new(rect.x + at, rect.y, len, rect.height);
        let room = rect.width - space::SM * 2;
        let tw = ui::width(&self.label, TextRole::Body).min(room);
        let tx = rect.x + (rect.width - tw) / 2;
        let ty = ui::centred_top(rect.y, rect.height, TextRole::Body);
        let label = |ctx: &mut PaintContext, ink| {
            ctx.draw_text_fitted(tx, ty, &self.label, TextRole::Body, room, ink)
        };
        self.cut.set(label(ctx, theme.text));
        ctx.with_viewport(filled, |ctx| {
            ctx.fill_rect(
                filled.x,
                filled.y,
                filled.width,
                filled.height,
                theme.ink(self.tone),
            );
            label(ctx, theme.text_on_accent);
        });
        ctx.draw_rect(rect.x, rect.y, rect.width, rect.height, theme.border);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn tooltip(&self, _x: i32, _y: i32) -> Option<&str> {
        self.cut.get().then_some(self.label.as_str())
    }
}
