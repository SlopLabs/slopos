use slopos_abi::draw::Color32;
use slopos_theme_core::{TextRole, with_alpha};

use crate::components::{Graph, GraphKey, GraphSeries, GraphSize};
use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::text::ui;
use crate::traits::{MeasureCtx, Widget, WidgetCore};

pub(crate) const THUMBNAIL_WIDTH: i32 = 64;

const KEY_WIDTH: i32 = 16;
/// Subpixels per pixel.
const SUB: i64 = 256;
const FILL_ALPHA: i64 = 64;
const GRID_SAMPLES: u64 = 10;
const DASH: i32 = 4;

/// The least 1, 2 or 5 × 10ⁿ × `base`ᵐ at or above `value`; the steps stop at
/// 500 within a power of `base`, so 600 in base 1024 rises to 1024.
pub fn nice_ceiling(value: u64, base: u64) -> u64 {
    const STEPS: [u64; 9] = [1, 2, 5, 10, 20, 50, 100, 200, 500];
    let mut unit: u64 = 1;
    loop {
        for step in STEPS {
            match step.checked_mul(unit) {
                Some(ceiling) if ceiling >= value => return ceiling,
                Some(_) => {}
                None => return u64::MAX,
            }
        }
        unit = match unit.checked_mul(base.max(2)) {
            Some(next) => next,
            None => return u64::MAX,
        };
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Plot {
    Large,
    Small,
    Thumbnail,
}

impl Plot {
    fn of(size: GraphSize) -> Self {
        match size {
            GraphSize::Large => Plot::Large,
            GraphSize::Small => Plot::Small,
        }
    }

    /// In subpixels.
    fn half_line(self) -> i64 {
        match self {
            Plot::Large | Plot::Small => SUB,
            Plot::Thumbnail => SUB * 3 / 4,
        }
    }
}

/// A sample is named by how many come after it, the last at the right edge.
struct Curve<'a> {
    values: &'a [u64],
    max: u64,
    span: i64,
    width: i64,
    height: i64,
}

impl<'a> Curve<'a> {
    fn new(values: &'a [u64], max: u64, window: usize, width: i32, height: i32) -> Self {
        let window = window.max(2);
        let shown = values.len().min(window);
        Self {
            values: &values[values.len() - shown..],
            max,
            span: window as i64 - 1,
            width: i64::from(width) * SUB,
            height: i64::from(height) * SUB,
        }
    }

    fn of(series: &'a GraphSeries, graph: &Graph, plot: Rect) -> Self {
        let (max, window) = (graph.max, graph.window);
        Self::new(&series.values, max, window, plot.width, plot.height)
    }

    fn oldest(&self) -> usize {
        self.values.len() - 1
    }

    fn x(&self, back: usize) -> i64 {
        self.width - back as i64 * self.width / self.span
    }

    fn y(&self, back: usize) -> i64 {
        let v = self.values[self.oldest() - back].min(self.max) as u128;
        self.height - (v * self.height as u128 / self.max as u128) as i64
    }

    fn left(&self) -> i64 {
        self.x(self.oldest())
    }

    fn right_of(&self, x: i64) -> usize {
        let oldest = self.oldest();
        let mut back = (((self.width - x).max(0) * self.span) / self.width) as usize;
        back = back.min(oldest);
        while back > 0 && self.x(back) < x {
            back -= 1;
        }
        while back < oldest && self.x(back + 1) >= x {
            back += 1;
        }
        back
    }

    fn y_at(&self, x: i64) -> i64 {
        let x = x.clamp(self.left(), self.width);
        let right = self.right_of(x);
        if right == self.oldest() || self.x(right) == x {
            return self.y(right);
        }
        let (xl, xr) = (self.x(right + 1), self.x(right));
        let (yl, yr) = (self.y(right + 1), self.y(right));
        yl + (yr - yl) * (x - xl) / (xr - xl)
    }

    /// Its ends and the samples strictly inside: one beside the column would
    /// stretch the line to it.
    fn extent(&self, a: i64, b: i64) -> (i64, i64) {
        let (a, b) = (
            a.clamp(self.left(), self.width),
            b.clamp(self.left(), self.width),
        );
        let (ya, yb) = (self.y_at(a), self.y_at(b));
        let (mut top, mut bottom) = (ya.min(yb), ya.max(yb));
        let mut back = self.right_of(b);
        while back <= self.oldest() && self.x(back) > a {
            if self.x(back) < b {
                let y = self.y(back);
                top = top.min(y);
                bottom = bottom.max(y);
            }
            back += 1;
        }
        (top, bottom)
    }
}

fn coverage(top: i64, bottom: i64, row: i64) -> u8 {
    let start = top.max(row);
    let end = bottom.min(row + SUB);
    ((end - start).max(0) * 255 / SUB) as u8
}

fn paint_fill(ctx: &mut PaintContext, inner: Rect, curve: &Curve, ink: Color32) {
    for col in 0..inner.width {
        let mid = i64::from(col) * SUB + SUB / 2;
        if mid < curve.left() {
            continue;
        }
        let y = curve.y_at(mid);
        let row = y / SUB;
        if row >= i64::from(inner.height) {
            continue;
        }
        let x = inner.x + col;
        let edge = ((row + 1) * SUB - y) * FILL_ALPHA / SUB;
        ctx.fill_rect_blended(x, inner.y + row as i32, 1, 1, with_alpha(ink, edge as u8));
        let below = row as i32 + 1;
        ctx.fill_rect_blended(
            x,
            inner.y + below,
            1,
            inner.height - below,
            with_alpha(ink, FILL_ALPHA as u8),
        );
    }
}

fn paint_line(
    ctx: &mut PaintContext,
    inner: Rect,
    curve: &Curve,
    ink: Color32,
    dashed: bool,
    half: i64,
) {
    for col in 0..inner.width {
        let gap = (inner.width - 1 - col) / DASH % 2 == 1;
        if dashed && gap {
            continue;
        }
        let a = i64::from(col) * SUB;
        if a + SUB <= curve.left() {
            continue;
        }
        let (top, bottom) = curve.extent(a, a + SUB);
        let (top, bottom) = (top - half, bottom + half);
        let first = (top.div_euclid(SUB)).max(0);
        let last = ((bottom - 1).div_euclid(SUB)).min(i64::from(inner.height) - 1);
        for row in first..=last {
            let alpha = coverage(top, bottom, row * SUB);
            if alpha > 0 {
                let y = inner.y + row as i32;
                ctx.fill_rect_blended(inner.x + col, y, 1, 1, with_alpha(ink, alpha));
            }
        }
    }
}

pub(crate) fn paint_plot(ctx: &mut PaintContext, rect: Rect, graph: &Graph, plot: Plot) {
    let theme = ctx.theme();
    ctx.fill_rect(
        rect.x,
        rect.y,
        rect.width,
        rect.height,
        theme.surface_sunken,
    );
    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width - 2, rect.height - 2);
    if inner.width > 0 && inner.height > 0 {
        let quarters: &[i32] = match plot {
            Plot::Large => &[1, 2, 3],
            Plot::Small | Plot::Thumbnail => &[2],
        };
        for &q in quarters {
            let y = inner.y + inner.height * q / 4;
            ctx.fill_rect(inner.x, y, inner.width, 1, theme.border);
        }
        if plot == Plot::Large {
            let scale = Curve::new(&[0], 1, graph.window, inner.width, inner.height);
            let mut back = graph.elapsed.saturating_sub(1) % GRID_SAMPLES;
            while back <= scale.span as u64 {
                let col = (scale.x(back as usize) / SUB).min(i64::from(inner.width) - 1);
                ctx.fill_rect(inner.x + col as i32, inner.y, 1, inner.height, theme.border);
                back += GRID_SAMPLES;
            }
        }
        if graph.max > 0 {
            ctx.with_clip(inner, |ctx| {
                let drawn = || graph.series.iter().filter(|s| !s.values.is_empty());
                for s in drawn().filter(|s| !s.dashed) {
                    paint_fill(
                        ctx,
                        inner,
                        &Curve::of(s, graph, inner),
                        theme.series_ink(s.series),
                    );
                }
                for s in drawn() {
                    let ink = theme.series_ink(s.series);
                    let curve = Curve::of(s, graph, inner);
                    paint_line(ctx, inner, &curve, ink, s.dashed, plot.half_line());
                }
            });
        }
    }
    ctx.draw_rect(rect.x, rect.y, rect.width, rect.height, theme.edge);
}

pub struct GraphWidget {
    core: WidgetCore,
    graph: Graph,
}

impl GraphWidget {
    pub fn new(graph: Graph) -> Self {
        Self {
            core: WidgetCore::new(),
            graph,
        }
    }
}

impl Widget for GraphWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let width = if constraints.is_width_bounded() {
            constraints.max_width
        } else {
            ctx.style.field_min_width * 4
        };
        let height = match self.graph.size {
            GraphSize::Large => (width / 3).clamp(120, 220),
            GraphSize::Small => (width / 2).clamp(40, 72),
        };
        constraints.constrain(Size::new(width, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let plot = Plot::of(self.graph.size);
        paint_plot(ctx, self.layout_rect(), &self.graph, plot);
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn accessible_name(&self) -> Option<&str> {
        Some(&self.graph.label)
    }
}

pub struct GraphKeyWidget {
    core: WidgetCore,
    key: GraphKey,
}

impl GraphKeyWidget {
    pub fn new(key: GraphKey) -> Self {
        Self {
            core: WidgetCore::new(),
            key,
        }
    }
}

impl Widget for GraphKeyWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let height = TextRole::Body.style().line_height;
        constraints.constrain(Size::new(KEY_WIDTH, height))
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let ink = ctx.theme().series_ink(self.key.series);
        let y = rect.y + rect.height / 2 - 1;
        let mut x = rect.x + rect.width;
        let mut drawn = true;
        while x > rect.x {
            let run = DASH.min(x - rect.x);
            if drawn || !self.key.dashed {
                ctx.fill_rect(x - run, y, run, 2, ink);
            }
            drawn = !drawn;
            x -= run;
        }
    }

    fn event(&mut self, _event: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }

    fn baseline(&self) -> Option<i32> {
        Some(ui::baseline_in(self.measured_size().height, TextRole::Body))
    }
}
