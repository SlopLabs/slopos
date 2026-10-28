use slopos_theme_core::{EdgeInsets, Surface};

use crate::audit::Arrangement;
use crate::constraints::{BoxConstraints, CrossAxisAlignment, Length, MAX_EXTENT, Rect, Size};
use crate::event::{EventResponse, MessageSink, WidgetEvent, route};
use crate::paint::PaintContext;
use crate::traits::{MeasureCtx, Widget, WidgetCore, measure_widget, place_widget};

fn main_axis(size: Size, vertical: bool) -> i32 {
    if vertical { size.height } else { size.width }
}

fn cross_axis(size: Size, vertical: bool) -> i32 {
    if vertical { size.width } else { size.height }
}

/// Only a stretching stack passes its cross-axis minimum on: a child it
/// aligns otherwise is as tall, or as wide, as it asks to be.
fn child_constraints(bc: &BoxConstraints, vertical: bool, stretch: bool) -> BoxConstraints {
    let floor = |min: i32| if stretch { min } else { 0 };
    if vertical {
        BoxConstraints {
            min_width: floor(bc.min_width),
            max_width: bc.max_width,
            min_height: 0,
            max_height: MAX_EXTENT,
        }
    } else {
        BoxConstraints {
            min_width: 0,
            max_width: MAX_EXTENT,
            min_height: floor(bc.min_height),
            max_height: bc.max_height,
        }
    }
}

struct StackWidget {
    core: WidgetCore,
    children: Vec<Box<dyn Widget>>,
    spacing: i32,
    cross_align: CrossAxisAlignment,
    vertical: bool,
    child_sizes: Vec<Size>,
    /// The shared baseline, from the top, of a row aligned by baseline.
    baseline: i32,
}

/// Where a child of a baseline-aligned row puts its baseline: its text's, or
/// its bottom edge when it has none.
fn child_baseline(child: &dyn Widget, size: Size) -> i32 {
    child.baseline().unwrap_or(size.height)
}

impl StackWidget {
    fn new(
        children: Vec<Box<dyn Widget>>,
        spacing: i32,
        align: CrossAxisAlignment,
        vertical: bool,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            children,
            spacing,
            cross_align: align,
            vertical,
            child_sizes: Vec::new(),
            baseline: 0,
        }
    }

    fn aligns_baselines(&self) -> bool {
        !self.vertical && self.cross_align == CrossAxisAlignment::Baseline
    }

    fn measure_impl(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let v = self.vertical;
        let stretch = self.cross_align == CrossAxisAlignment::Stretch;
        let loose = child_constraints(&constraints, v, stretch);
        let max_main = if v {
            constraints.max_height
        } else {
            constraints.max_width
        };
        self.child_sizes.clear();
        self.child_sizes.resize(self.children.len(), Size::ZERO);

        let mut total_fixed: i32 = 0;
        let mut max_cross: i32 = 0;
        let mut total_flex_weight: u32 = 0;

        for (i, child) in self.children.iter_mut().enumerate() {
            let w = child.flex_weight();
            if w > 0 {
                total_flex_weight += w as u32;
            } else {
                let child_size = measure_widget(child.as_mut(), loose, ctx);
                total_fixed = total_fixed.saturating_add(main_axis(child_size, v));
                max_cross = max_cross.max(cross_axis(child_size, v));
                self.child_sizes[i] = child_size;
            }
        }

        let gap_count = if self.children.len() > 1 {
            (self.children.len() - 1) as i32
        } else {
            0
        };
        let spacing_total = gap_count.saturating_mul(self.spacing);

        let mut total_main = total_fixed;
        if total_flex_weight > 0 && constraints.is_main_axis_bounded(v) {
            let remaining = (max_main - total_fixed - spacing_total).max(0);
            for (i, child) in self.children.iter_mut().enumerate() {
                let w = child.flex_weight();
                if w > 0 {
                    let share = (remaining as u32 * w as u32 / total_flex_weight) as i32;
                    let tight = if v {
                        BoxConstraints {
                            min_width: loose.min_width,
                            max_width: loose.max_width,
                            min_height: share,
                            max_height: share,
                        }
                    } else {
                        BoxConstraints {
                            min_width: share,
                            max_width: share,
                            min_height: loose.min_height,
                            max_height: loose.max_height,
                        }
                    };
                    let child_size = measure_widget(child.as_mut(), tight, ctx);
                    max_cross = max_cross.max(cross_axis(child_size, v));
                    self.child_sizes[i] = child_size;
                    total_main = total_main.saturating_add(main_axis(child_size, v));
                }
            }
        } else if total_flex_weight > 0 {
            for (i, child) in self.children.iter_mut().enumerate() {
                if child.flex_weight() > 0 {
                    let child_size = measure_widget(child.as_mut(), loose, ctx);
                    max_cross = max_cross.max(cross_axis(child_size, v));
                    self.child_sizes[i] = child_size;
                    total_main = total_main.saturating_add(main_axis(child_size, v));
                }
            }
        }

        total_main = total_main.saturating_add(spacing_total);

        if self.aligns_baselines() {
            let (mut above, mut below) = (0, 0);
            for (child, &size) in self.children.iter().zip(&self.child_sizes) {
                let baseline = child_baseline(child.as_ref(), size);
                above = above.max(baseline);
                below = below.max(size.height - baseline);
            }
            self.baseline = above;
            max_cross = above + below;
        }

        let size = if v {
            Size::new(max_cross, total_main)
        } else {
            Size::new(total_main, max_cross)
        };
        constraints.constrain(size)
    }

    fn layout_impl(&mut self, rect: Rect) {
        let v = self.vertical;
        let avail_main = if v { rect.height } else { rect.width };
        let avail_cross = cross_axis(Size::new(rect.width, rect.height), v);

        let mut total_fixed: i32 = 0;
        let mut total_flex_weight: u32 = 0;
        let n = self.child_sizes.len();
        for i in 0..self.children.len().min(n) {
            let w = self.children[i].flex_weight();
            if w > 0 {
                total_flex_weight += w as u32;
            } else {
                total_fixed = total_fixed.saturating_add(main_axis(self.child_sizes[i], v));
            }
        }

        let gap_count = if self.children.len() > 1 {
            (self.children.len() - 1) as i32
        } else {
            0
        };
        let spacing_total = gap_count.saturating_mul(self.spacing);
        let remaining = (avail_main - total_fixed - spacing_total).max(0);

        let mut cursor: i32 = 0;
        for i in 0..self.children.len().min(n) {
            let child_main = if total_flex_weight > 0 && self.children[i].flex_weight() > 0 {
                let w = self.children[i].flex_weight() as u32;
                (remaining as u32 * w / total_flex_weight) as i32
            } else {
                main_axis(self.child_sizes[i], v)
            };
            let child_cross = cross_axis(self.child_sizes[i], v);

            let cross_pos = match self.cross_align {
                CrossAxisAlignment::Baseline if self.aligns_baselines() => {
                    self.baseline - child_baseline(self.children[i].as_ref(), self.child_sizes[i])
                }
                CrossAxisAlignment::Start | CrossAxisAlignment::Baseline => 0,
                CrossAxisAlignment::Center => (avail_cross - child_cross) / 2,
                CrossAxisAlignment::End => avail_cross - child_cross,
                CrossAxisAlignment::Stretch => 0,
            };
            let layout_cross = if self.cross_align == CrossAxisAlignment::Stretch {
                avail_cross
            } else {
                child_cross
            };

            let (abs_x, abs_y, w, h) = if v {
                (
                    rect.x + cross_pos,
                    rect.y + cursor,
                    layout_cross,
                    child_main,
                )
            } else {
                (
                    rect.x + cursor,
                    rect.y + cross_pos,
                    child_main,
                    layout_cross,
                )
            };
            place_widget(self.children[i].as_mut(), Rect::new(abs_x, abs_y, w, h));
            cursor += child_main + self.spacing;
        }
    }

    fn paint_impl(&self, ctx: &mut PaintContext) {
        for child in &self.children {
            child.paint(ctx);
        }
    }
}

pub struct VStackWidget {
    inner: StackWidget,
}

impl VStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>, spacing: i32, align: CrossAxisAlignment) -> Self {
        Self {
            inner: StackWidget::new(children, spacing, align, true),
        }
    }
}

impl Widget for VStackWidget {
    fn measure(&mut self, c: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.inner.measure_impl(c, ctx)
    }
    fn layout(&mut self, r: Rect) {
        self.inner.layout_impl(r);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.inner.paint_impl(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        route(&mut self.inner.children, e, sink)
    }
    fn core(&self) -> &WidgetCore {
        &self.inner.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.inner.core
    }
    fn baseline(&self) -> Option<i32> {
        self.inner
            .children
            .first()
            .and_then(|child| child.baseline())
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.inner.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.inner.children
    }
}

pub struct HStackWidget {
    inner: StackWidget,
}

impl HStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>, spacing: i32, align: CrossAxisAlignment) -> Self {
        Self {
            inner: StackWidget::new(children, spacing, align, false),
        }
    }
}

impl Widget for HStackWidget {
    fn measure(&mut self, c: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.inner.measure_impl(c, ctx)
    }
    fn layout(&mut self, r: Rect) {
        self.inner.layout_impl(r);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.inner.paint_impl(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        route(&mut self.inner.children, e, sink)
    }
    fn core(&self) -> &WidgetCore {
        &self.inner.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.inner.core
    }
    fn baseline(&self) -> Option<i32> {
        self.inner.aligns_baselines().then_some(self.inner.baseline)
    }
    fn arrangement(&self) -> Arrangement {
        let aligned = matches!(
            self.inner.cross_align,
            CrossAxisAlignment::Center | CrossAxisAlignment::Baseline
        );
        Arrangement::Row { aligned }
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.inner.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.inner.children
    }
}

pub struct ZStackWidget {
    core: WidgetCore,
    children: Vec<Box<dyn Widget>>,
}

impl ZStackWidget {
    pub fn new(children: Vec<Box<dyn Widget>>) -> Self {
        Self {
            core: WidgetCore::new(),
            children,
        }
    }
}

impl Widget for ZStackWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let loose = constraints.loosen();
        let mut max_w: i32 = 0;
        let mut max_h: i32 = 0;
        for child in &mut self.children {
            let s = measure_widget(child.as_mut(), loose, ctx);
            max_w = max_w.max(s.width);
            max_h = max_h.max(s.height);
        }
        constraints.constrain(Size::new(max_w, max_h))
    }
    fn layout(&mut self, rect: Rect) {
        for child in &mut self.children {
            place_widget(child.as_mut(), rect);
        }
    }
    fn paint(&self, ctx: &mut PaintContext) {
        for (i, child) in self.children.iter().enumerate() {
            match i {
                0 => child.paint(ctx),
                _ => ctx.with_layer(child.id(), |ctx| child.paint(ctx)),
            }
        }
    }
    fn arrangement(&self) -> Arrangement {
        Arrangement::Layers
    }
    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        for child in self.children.iter_mut().rev() {
            let resp = child.event(event, sink);
            if resp.is_consumed() {
                return resp;
            }
        }
        EventResponse::Ignored
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        &self.children
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut self.children
    }
}

pub struct PaddingWidget {
    core: WidgetCore,
    insets: EdgeInsets,
    child: Box<dyn Widget>,
}

impl PaddingWidget {
    pub fn new(insets: EdgeInsets, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            insets,
            child,
        }
    }
}

impl Widget for PaddingWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let inner = constraints.deflate(self.insets);
        let child_size = measure_widget(self.child.as_mut(), inner, ctx);
        constraints.constrain(Size::new(
            child_size.width + self.insets.horizontal(),
            child_size.height + self.insets.vertical(),
        ))
    }
    fn layout(&mut self, rect: Rect) {
        place_widget(
            self.child.as_mut(),
            Rect::new(
                rect.x + self.insets.left,
                rect.y + self.insets.top,
                (rect.width - self.insets.horizontal()).max(0),
                (rect.height - self.insets.vertical()).max(0),
            ),
        );
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, sink)
    }
    fn baseline(&self) -> Option<i32> {
        self.child.baseline().map(|b| b + self.insets.top)
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

pub struct SpacerWidget {
    core: WidgetCore,
    length: Length,
}

impl SpacerWidget {
    pub fn new(length: Length) -> Self {
        Self {
            core: WidgetCore::new(),
            length,
        }
    }
}

impl Widget for SpacerWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, _ctx: &mut MeasureCtx) -> Size {
        let px = match self.length {
            Length::Px(n) => n,
            Length::Fill(_) => 0,
        };
        constraints.constrain(Size::new(px, px))
    }
    fn flex_weight(&self) -> u16 {
        self.length.flex_weight()
    }
    fn paint(&self, _ctx: &mut PaintContext) {}
    fn event(&mut self, _e: &WidgetEvent, _sink: &mut MessageSink) -> EventResponse {
        EventResponse::Ignored
    }
}

pub struct ExpandWidget {
    core: WidgetCore,
    weight: u16,
    child: Box<dyn Widget>,
}

impl ExpandWidget {
    pub fn new(weight: u16, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            weight,
            child,
        }
    }
}

impl Widget for ExpandWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let child_size = measure_widget(self.child.as_mut(), constraints, ctx);
        Size::new(
            child_size.width.max(constraints.min_width),
            child_size.height.max(constraints.min_height),
        )
    }
    fn layout(&mut self, rect: Rect) {
        place_widget(self.child.as_mut(), rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, sink)
    }
    fn baseline(&self) -> Option<i32> {
        self.child.baseline()
    }
    fn flex_weight(&self) -> u16 {
        self.weight
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

pub struct BackgroundWidget {
    core: WidgetCore,
    surface: Surface,
    child: Box<dyn Widget>,
}

impl BackgroundWidget {
    pub fn new(surface: Surface, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            surface,
            child,
        }
    }
}

impl Widget for BackgroundWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        measure_widget(self.child.as_mut(), constraints, ctx)
    }
    fn layout(&mut self, rect: Rect) {
        place_widget(self.child.as_mut(), rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let fill = ctx.theme().fill(self.surface);
        ctx.fill_rect(rect.x, rect.y, rect.width, rect.height, fill);
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, sink)
    }
    fn baseline(&self) -> Option<i32> {
        self.child.baseline()
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}

pub struct SizedBoxWidget {
    core: WidgetCore,
    width: Option<Length>,
    height: Option<Length>,
    child: Box<dyn Widget>,
}

impl SizedBoxWidget {
    pub fn new(width: Option<Length>, height: Option<Length>, child: Box<dyn Widget>) -> Self {
        Self {
            core: WidgetCore::new(),
            width,
            height,
            child,
        }
    }

    fn resolve_w(&self, constraints: &BoxConstraints) -> Option<i32> {
        self.width
            .map(|l| l.resolve(constraints.min_width, constraints.max_width))
    }

    fn resolve_h(&self, constraints: &BoxConstraints) -> Option<i32> {
        self.height
            .map(|l| l.resolve(constraints.min_height, constraints.max_height))
    }
}

impl Widget for SizedBoxWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        let rw = self.resolve_w(&constraints);
        let rh = self.resolve_h(&constraints);
        let inner = BoxConstraints {
            min_width: rw.unwrap_or(constraints.min_width),
            max_width: rw.unwrap_or(constraints.max_width),
            min_height: rh.unwrap_or(constraints.min_height),
            max_height: rh.unwrap_or(constraints.max_height),
        };
        let child_size = measure_widget(self.child.as_mut(), inner, ctx);
        Size::new(
            rw.unwrap_or(child_size.width),
            rh.unwrap_or(child_size.height),
        )
    }
    fn layout(&mut self, rect: Rect) {
        place_widget(self.child.as_mut(), rect);
    }
    fn paint(&self, ctx: &mut PaintContext) {
        self.child.paint(ctx);
    }
    fn event(&mut self, e: &WidgetEvent, sink: &mut MessageSink) -> EventResponse {
        self.child.event(e, sink)
    }
    fn baseline(&self) -> Option<i32> {
        self.child.baseline()
    }
    fn children(&self) -> &[Box<dyn Widget>] {
        core::slice::from_ref(&self.child)
    }
    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        core::slice::from_mut(&mut self.child)
    }
}
