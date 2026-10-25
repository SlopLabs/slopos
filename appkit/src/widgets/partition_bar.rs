use slopos_theme_core::{inset, space};

use crate::constraints::{BoxConstraints, Size};
use crate::event::{EventPhase, EventResponse, MessageSink, WidgetEvent};
use crate::node::{BarSegment, SegmentFill};
use crate::paint::PaintContext;
use crate::text::BODY;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

/// The narrowest a segment is drawn, so a partition a thousandth of the disk
/// still shows.
pub const MIN_SEGMENT_WIDTH: i32 = 8;

const HATCH_PERIOD: i32 = 8;
const HIGHLIGHT_WIDTH: i32 = 2;

/// Each segment's share of `width`: `MIN_SEGMENT_WIDTH`, or what the bar
/// gives each when it is narrower than that, plus the rest in proportion to
/// its size, the remainder going to the largest fractions. A segment of no
/// size gets nothing unless all are, when they share alike. The widths add up
/// to `width`.
pub fn segment_widths(sizes: &[u64], width: i32) -> Vec<i32> {
    let width = width.max(0);
    let weights: Vec<u128> = if sizes.iter().all(|&s| s == 0) {
        vec![1; sizes.len()]
    } else {
        sizes.iter().map(|&s| s as u128).collect()
    };
    let shown = weights.iter().filter(|&&w| w > 0).count() as i32;
    if shown == 0 {
        return Vec::new();
    }
    let floor = MIN_SEGMENT_WIDTH.min(width / shown);
    let rest = (width - floor * shown) as u128;
    let total: u128 = weights.iter().sum();
    let mut widths: Vec<i32> = weights
        .iter()
        .map(|&w| match w {
            0 => 0,
            _ => floor + (rest * w / total) as i32,
        })
        .collect();
    let left = width - widths.iter().sum::<i32>();
    let mut order: Vec<usize> = (0..weights.len()).filter(|&i| weights[i] > 0).collect();
    order.sort_by_key(|&i| core::cmp::Reverse(rest * weights[i] % total));
    for i in order.into_iter().take(left as usize) {
        widths[i] += 1;
    }
    widths
}

/// A disk drawn as proportional segments, one of which may be highlighted.
pub struct PartitionBarWidget {
    core: WidgetCore,
    segments: Vec<BarSegment>,
    highlight: Option<usize>,
}

impl PartitionBarWidget {
    pub fn new(segments: Vec<BarSegment>, highlight: Option<usize>) -> Self {
        Self {
            core: WidgetCore::new(),
            segments,
            highlight,
        }
    }

    fn hatch(ctx: &mut PaintContext, x: i32, y: i32, w: i32, h: i32) {
        let theme = ctx.theme();
        ctx.fill_rect(x, y, w, h, theme.surface_sunken);
        let stripe = theme.border;
        for row in 0..h {
            let mut col = (HATCH_PERIOD - row % HATCH_PERIOD) % HATCH_PERIOD;
            while col < w {
                ctx.fill_rect(x + col, y + row, 2.min(w - col), 1, stripe);
                col += HATCH_PERIOD;
            }
        }
    }
}

impl Widget for PartitionBarWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let height = BODY.line_height + inset::CONTROL.vertical();
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width * 4
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let sizes: Vec<u64> = self.segments.iter().map(|s| s.size).collect();
        let widths = segment_widths(&sizes, rect.width);
        let text_h = ctx.text_height();
        let pad = space::XS;
        let theme = ctx.theme();

        ctx.fill_rect(
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            theme.surface_sunken,
        );
        let mut x = rect.x;
        for (i, (segment, &w)) in self.segments.iter().zip(&widths).enumerate() {
            if w == 0 {
                continue;
            }
            let fg = match segment.fill {
                SegmentFill::Series(n) => {
                    ctx.fill_rect(x, rect.y, w, rect.height, theme.series(n));
                    theme.text_on_accent
                }
                SegmentFill::Hatched => {
                    Self::hatch(ctx, x, rect.y, w, rect.height);
                    theme.text_secondary
                }
            };
            if i > 0 {
                ctx.fill_rect(x, rect.y, 1, rect.height, theme.surface);
            }
            let tw = ctx.text_width(&segment.label);
            if tw <= w - pad * 2 {
                let ty = rect.y + (rect.height - text_h) / 2;
                ctx.draw_text_transparent(x + (w - tw) / 2, ty, &segment.label, fg);
            }
            x += w;
        }

        ctx.draw_rect(rect.x, rect.y, rect.width, rect.height, theme.border);
        let mut x = rect.x;
        for (i, &w) in widths.iter().enumerate() {
            if self.highlight == Some(i) {
                for k in 0..HIGHLIGHT_WIDTH.min(w / 2) {
                    ctx.draw_rect(
                        x + k,
                        rect.y + k,
                        w - k * 2,
                        rect.height - k * 2,
                        theme.text,
                    );
                }
            }
            x += w;
        }
    }

    fn event(
        &mut self,
        _event: &WidgetEvent,
        _phase: EventPhase,
        _sink: &mut MessageSink,
    ) -> EventResponse {
        EventResponse::Ignored
    }
}
