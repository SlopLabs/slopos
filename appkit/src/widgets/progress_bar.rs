use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
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
    color: Option<Color32>,
}

impl ProgressBarWidget {
    pub fn new(value: Option<u32>, label: String, color: Option<Color32>) -> Self {
        Self {
            core: WidgetCore::new(),
            value: value.map(|v| v.min(100)),
            label,
            color,
        }
    }

    fn fill_color(&self, value: u32) -> Color32 {
        if let Some(c) = self.color {
            return c;
        }
        if value < 50 {
            Color32::rgb(46, 170, 78)
        } else if value < 80 {
            Color32::rgb(204, 170, 34)
        } else {
            Color32::rgb(204, 51, 51)
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
        let height = ctx.style.line_height + 4;
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width.max(ctx.text_width(&self.label))
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();

        ctx.fill_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            ctx.style.bg_tertiary,
        );

        match self.value {
            Some(value) => {
                let fill_w = rect.width * value as i32 / 100;
                if fill_w > 0 {
                    ctx.fill_rect(rect.x, rect.y, fill_w, rect.height, self.fill_color(value));
                }
            }
            None => {
                let (at, len) = sweep(rect.width, slopos_windowing::get_time_ms());
                let color = self.color.unwrap_or(ctx.style.bg_accent);
                ctx.fill_rect(rect.x + at, rect.y, len, rect.height, color);
            }
        }

        ctx.draw_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            ctx.style.border_default,
        );

        let tw = ctx.text_width(&self.label);
        let th = ctx.text_height();
        let tx = rect.x + (rect.width - tw) / 2;
        let ty = rect.y + (rect.height - th) / 2;
        ctx.draw_text_transparent(tx, ty, &self.label, ctx.style.text_primary);
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
