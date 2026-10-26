use slopos_theme_core::{Tone, space};

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::BODY;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

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
}

impl ProgressBarWidget {
    pub fn new(value: Option<u32>, label: String, tone: Tone) -> Self {
        Self {
            core: WidgetCore::new(),
            value: value.map(|v| v.min(100)),
            label,
            tone,
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
        let height = BODY.line_height + space::XS;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width.max(ctx.text_width(&self.label))
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
        let tw = ctx.text_width(&self.label);
        let tx = rect.x + (rect.width - tw) / 2;
        let ty = rect.y + (rect.height - ctx.text_height()) / 2;
        ctx.draw_text_transparent(tx, ty, &self.label, theme.text);
        ctx.with_viewport(filled, |ctx| {
            ctx.fill_rect(
                filled.x,
                filled.y,
                filled.width,
                filled.height,
                theme.ink(self.tone),
            );
            ctx.draw_text_transparent(tx, ty, &self.label, theme.text_on_accent);
        });
        ctx.draw_rect(rect.x, rect.y, rect.width, rect.height, theme.border);
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }

    fn role(&self) -> Role {
        Role::None
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }
}
