//! Holds a laid-out tree to what a screen owes the person reading it: no
//! text cut short without an ellipsis, no widget over another or outside its
//! parent, one baseline per row, focus shown all the way round a control,
//! controls a pointer can find and text that reads against what is behind it.
//!
//! It paints the tree off screen and judges what was drawn, so a widget is
//! audited by the pixels it puts down rather than by what it says of itself.

use std::fmt;

use slopos_abi::draw::{Canvas, Color32};
use slopos_gfx::DrawBuffer;
use slopos_theme_core::contrast;

use crate::constraints::{Rect, Size};
use crate::event::{MessageSink, WidgetEvent};
use crate::paint::PaintContext;
use crate::style::StyleSheet;
use crate::traits::{Widget, WidgetId};

/// One line of text as drawn: its line box, ink, and the colour most of the
/// box held before it.
#[derive(Clone, Debug)]
pub struct TextRun {
    pub x: i32,
    pub top: i32,
    pub baseline: i32,
    pub width: i32,
    pub height: i32,
    pub text: String,
    pub fg: Color32,
    pub bg: Color32,
    pub clip: Rect,
    /// The clip less every viewport's: the edges that cut text short.
    pub fixed: Rect,
    /// The innermost view onto content that goes on past it.
    pub viewport: Option<Rect>,
    /// The layer that drew it, whose text may lie over another layer's.
    pub layer: WidgetId,
    pub mark: Mark,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Mark {
    Whole,
    /// Part of a line cut short with an ellipsis, which fits across by
    /// construction.
    Elided,
    /// Not drawn: there was no room even for an ellipsis.
    Dropped,
}

impl TextRun {
    fn line_box(&self) -> Rect {
        Rect::new(self.x, self.top, self.width.max(1), self.height)
    }

    /// The top-left of what is shown of the run, which the widget that drew
    /// it holds.
    fn origin(&self) -> (i32, i32) {
        let shown = self.shown().unwrap_or(self.line_box());
        (shown.x + shown.width.min(1), shown.y + shown.height.min(1))
    }

    fn shown(&self) -> Option<Rect> {
        let drawn = self.mark != Mark::Dropped;
        self.line_box().intersect(&self.clip).filter(|_| drawn)
    }

    fn visible(&self) -> bool {
        self.shown().is_some()
    }
}

/// How a container places its children, which decides what they are held to.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Arrangement {
    /// Side by side or one under another, each inside the parent.
    Flow,
    /// A row whose texts share a baseline when it aligns them, centred or by
    /// baseline.
    Row { aligned: bool },
    /// Stacked over each other on purpose: layers, a modal over its page.
    Layers,
    /// Children may lie outside the parent, which shows a part of them.
    Scroll,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Rule {
    /// Text cut by an edge rather than by an ellipsis.
    Clipped,
    Overlap,
    Outside,
    Baseline,
    /// A focusable control whose focus does not show all the way round.
    Focus,
    /// A control smaller than a pointer can be relied on to find.
    Target,
    Contrast,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub rule: Rule,
    pub at: Rect,
    pub what: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = self.at;
        write!(
            f,
            "{:?} at {}x{}+{}+{}: {}",
            self.rule, r.width, r.height, r.x, r.y, self.what
        )
    }
}

/// Lays `root` out at `size` and answers what it breaks.
pub fn audit(root: &mut dyn Widget, size: Size, style: &StyleSheet) -> Vec<Finding> {
    crate::tree::layout_tree(root, size, style);
    let mut findings = Vec::new();
    let runs = paint_recording(root, size, style);
    for run in &runs {
        check_run(root, run, style, &mut findings);
    }
    check_text_over_text(&runs, &mut findings);
    check_layout(root, &runs, &mut findings);
    check_focus(root, size, style, &mut findings);
    findings
}

fn check_run(root: &dyn Widget, run: &TextRun, style: &StyleSheet, out: &mut Vec<Finding>) {
    let at = run.line_box();
    if run.mark == Mark::Dropped {
        out.push(Finding {
            rule: Rule::Clipped,
            at,
            what: format!("{:?} has no room, not even for an ellipsis", run.text),
        });
        return;
    }
    if let Some(edge) = cut_by(run, run.fixed) {
        out.push(Finding {
            rule: Rule::Clipped,
            at,
            what: format!("{:?} runs past its clip {edge}", run.text),
        });
        return;
    }
    if !run.visible() {
        return;
    }
    if let Some(owner) = drawn_by(root, run.origin(), run.layer) {
        let rect = owner.layout_rect();
        let scrolls_itself = run.viewport.is_some_and(|v| contains(rect, v));
        if let Some(edge) = cut_by(run, rect).filter(|_| !scrolls_itself) {
            out.push(Finding {
                rule: Rule::Clipped,
                at,
                what: format!("{:?} runs past the widget that drew it {edge}", run.text),
            });
        }
    }
    let floor = if run.fg == style.theme.text_disabled {
        contrast::NON_TEXT
    } else {
        contrast::TEXT
    };
    let ink = contrast::over(run.fg, run.bg);
    let ratio = contrast::ratio(ink, run.bg);
    if ratio < floor {
        out.push(Finding {
            rule: Rule::Contrast,
            at,
            what: format!(
                "{:?} is {ratio:.2}:1 on {:?}, under {floor}:1",
                run.text, run.bg
            ),
        });
    }
}

/// Which edge of `rect` the run's line box crosses; an elided line fits
/// across by construction.
fn cut_by(run: &TextRun, rect: Rect) -> Option<&'static str> {
    let b = run.line_box();
    if b.y < rect.y {
        Some("at the top")
    } else if b.y + b.height > rect.y + rect.height {
        Some("at the bottom")
    } else if run.mark == Mark::Elided {
        None
    } else if b.x < rect.x {
        Some("on the left")
    } else if b.x + b.width > rect.x + rect.width {
        Some("on the right")
    } else {
        None
    }
}

/// Text painted over other text of its own layer; the same text drawn again
/// in place, as in another ink over a bar's fill, is one line.
fn check_text_over_text(runs: &[TextRun], out: &mut Vec<Finding>) {
    let shown: Vec<(&TextRun, Rect)> = runs.iter().filter_map(|r| Some((r, r.shown()?))).collect();
    for (i, (a, a_box)) in shown.iter().enumerate() {
        for (b, b_box) in &shown[i + 1..] {
            let again = (a.x, a.top, &a.text) == (b.x, b.top, &b.text);
            if a.layer == b.layer && !again && a_box.intersect(b_box).is_some() {
                out.push(Finding {
                    rule: Rule::Overlap,
                    at: *b_box,
                    what: format!("{:?} is drawn over {:?}", b.text, a.text),
                });
            }
        }
    }
}

/// The topmost widget, deepest first, of the layer `layer` whose rect holds
/// `point`; the page itself when `layer` is none.
fn drawn_by(widget: &dyn Widget, point: (i32, i32), layer: WidgetId) -> Option<&dyn Widget> {
    if !widget.layout_rect().contains(point.0, point.1) {
        return None;
    }
    let stacks = widget.arrangement() == Arrangement::Layers && !widget.covers();
    for (i, child) in widget.children().iter().enumerate().rev() {
        let other_layer = (child.covers() || (stacks && i > 0))
            && child.id() != layer
            && !crate::focus::holds(child.as_ref(), layer);
        if other_layer {
            continue;
        }
        if let Some(found) = drawn_by(child.as_ref(), point, layer) {
            return Some(found);
        }
    }
    Some(widget)
}

fn has_area(r: Rect) -> bool {
    r.width > 0 && r.height > 0
}

fn contains(outer: Rect, inner: Rect) -> bool {
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner.x + inner.width <= outer.x + outer.width
        && inner.y + inner.height <= outer.y + outer.height
}

fn check_layout(widget: &dyn Widget, runs: &[TextRun], out: &mut Vec<Finding>) {
    let rect = widget.layout_rect();
    let arrangement = widget.arrangement();
    if matches!(arrangement, Arrangement::Flow | Arrangement::Row { .. }) {
        let children = widget.children();
        for (i, child) in children.iter().enumerate() {
            let r = child.layout_rect();
            if !has_area(r) {
                continue;
            }
            if !contains(rect, r) {
                out.push(Finding {
                    rule: Rule::Outside,
                    at: r,
                    what: format!(
                        "a child at {}x{}+{}+{} leaves its parent at {}x{}+{}+{}",
                        r.width, r.height, r.x, r.y, rect.width, rect.height, rect.x, rect.y
                    ),
                });
            }
            for other in &children[i + 1..] {
                let o = other.layout_rect();
                if has_area(o) && r.intersect(&o).is_some() {
                    out.push(Finding {
                        rule: Rule::Overlap,
                        at: r,
                        what: format!(
                            "siblings overlap: {}x{}+{}+{} and {}x{}+{}+{}",
                            r.width, r.height, r.x, r.y, o.width, o.height, o.x, o.y
                        ),
                    });
                }
            }
        }
    }
    if arrangement == (Arrangement::Row { aligned: true }) {
        check_baselines(widget, runs, out);
    }
    for child in widget.children() {
        check_layout(child.as_ref(), runs, out);
    }
}

fn check_baselines(row: &dyn Widget, runs: &[TextRun], out: &mut Vec<Finding>) {
    let mut first: Vec<(i32, &str)> = Vec::new();
    for child in row.children() {
        let r = child.layout_rect();
        let line = runs
            .iter()
            .filter(|run| run.visible())
            .filter(|run| {
                let (x, y) = run.origin();
                r.contains(x, y)
            })
            .min_by_key(|run| run.top);
        if let Some(run) = line {
            first.push((run.baseline, &run.text));
        }
    }
    if first.windows(2).any(|w| w[0].0 != w[1].0) {
        let lines: Vec<String> = first
            .iter()
            .map(|(b, text)| format!("{text:?} on {b}"))
            .collect();
        out.push(Finding {
            rule: Rule::Baseline,
            at: row.layout_rect(),
            what: lines.join(", "),
        });
    }
}

fn check_focus(root: &mut dyn Widget, size: Size, style: &StyleSheet, out: &mut Vec<Finding>) {
    let window = Rect::new(0, 0, size.width, size.height);
    let idle = paint_plain(root, size, style, window);
    let reach = style.focus_ring_offset + style.focus_ring_width + 1;
    for id in crate::focus::FocusManager::tab_chain_of(root) {
        let Some((rect, shown)) = place_of(root, id, window) else {
            continue;
        };
        if rect.width < style.min_target || rect.height < style.min_target {
            out.push(Finding {
                rule: Rule::Target,
                at: rect,
                what: format!("under {} px across", style.min_target),
            });
        }
        if !contains(shown, rect) {
            continue;
        }
        let around = rect.inflate(reach);
        let mut sink = MessageSink::new();
        crate::tree::send_to_id(root, id, &WidgetEvent::FocusGained, &mut sink);
        let focused = paint_plain(root, size, style, around);
        crate::tree::send_to_id(root, id, &WidgetEvent::FocusLost, &mut sink);
        if let Err(what) = frames(&idle, &focused, size, around, reach) {
            out.push(Finding {
                rule: Rule::Focus,
                at: rect,
                what: String::from(what),
            });
        }
    }
}

/// The rect of the widget `id` names, and the part of the window the
/// scrolling containers above it show.
fn place_of(widget: &dyn Widget, id: WidgetId, shown: Rect) -> Option<(Rect, Rect)> {
    if widget.id() == id {
        return Some((widget.layout_rect(), shown));
    }
    let shown = if widget.arrangement() == Arrangement::Scroll {
        shown.intersect(&widget.layout_rect())?
    } else {
        shown
    };
    widget
        .children()
        .iter()
        .find_map(|child| place_of(child.as_ref(), id, shown))
}

/// Whether what focus changed in `region` is a frame: closed on all four
/// sides and wider than a ring cut down to one of them.
fn frames(a: &[u8], b: &[u8], size: Size, region: Rect, reach: i32) -> Result<(), &'static str> {
    let Some(region) = region.intersect(&Rect::new(0, 0, size.width, size.height)) else {
        return Err("lies outside the window");
    };
    let pitch = size.width as usize * 4;
    let changed = |x: i32, y: i32| {
        let at = y as usize * pitch + x as usize * 4;
        a[at..at + 4] != b[at..at + 4]
    };
    let xs = region.x..region.x + region.width;
    let ys = region.y..region.y + region.height;
    let rows: Vec<i32> = ys
        .clone()
        .filter(|&y| xs.clone().any(|x| changed(x, y)))
        .collect();
    let cols: Vec<i32> = xs
        .clone()
        .filter(|&x| ys.clone().any(|y| changed(x, y)))
        .collect();
    let (Some(&top), Some(&bottom), Some(&left), Some(&right)) =
        (rows.first(), rows.last(), cols.first(), cols.last())
    else {
        return Err("looks the same focused as not");
    };
    let (w, h) = (right - left + 1, bottom - top + 1);
    if w < reach * 3 || h < reach * 2 {
        return Err("shows focus on one side only");
    }
    let across = |y: i32| (left..=right).filter(|&x| changed(x, y)).count() as i32 * 2 >= w;
    let down = |x: i32| (top..=bottom).filter(|&y| changed(x, y)).count() as i32 * 2 >= h;
    let side = |range: std::ops::RangeInclusive<i32>, line: &dyn Fn(i32) -> bool| {
        range.into_iter().any(line)
    };
    if !side(top..=top + reach, &across) || !side(bottom - reach..=bottom, &across) {
        return Err("shows no focus above or below");
    }
    if !side(left..=left + reach, &down) || !side(right - reach..=right, &down) {
        return Err("shows no focus at the left or right");
    }
    Ok(())
}

fn canvas(size: Size) -> Vec<u8> {
    vec![0u8; size.width.max(1) as usize * size.height.max(1) as usize * 4]
}

fn paint_into(
    data: &mut [u8],
    root: &dyn Widget,
    size: Size,
    style: &StyleSheet,
    runs: Option<&mut Vec<TextRun>>,
    clip: Rect,
) {
    let (w, h) = (size.width.max(1) as u32, size.height.max(1) as u32);
    let Some(mut buffer) = DrawBuffer::new(data, w, h, w as usize * 4, 4) else {
        return;
    };
    let fill = buffer.pixel_format().encode(style.theme.surface);
    buffer.clear_canvas(fill);
    let mut ctx = match runs {
        Some(runs) => PaintContext::recording(&mut buffer, style, runs),
        None => PaintContext::new(&mut buffer, style),
    };
    ctx.focus_visible = true;
    ctx.with_clip(clip, |ctx| crate::tree::paint_tree(root, ctx));
}

pub(crate) fn paint_recording(root: &dyn Widget, size: Size, style: &StyleSheet) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut data = canvas(size);
    let window = Rect::new(0, 0, size.width, size.height);
    paint_into(&mut data, root, size, style, Some(&mut runs), window);
    runs
}

/// `root` painted with focus shown, only `clip` of it.
fn paint_plain(root: &dyn Widget, size: Size, style: &StyleSheet, clip: Rect) -> Vec<u8> {
    let mut data = canvas(size);
    paint_into(&mut data, root, size, style, None, clip);
    data
}

/// Every finding of `node` built and laid out at each of `widths` by `height`,
/// with room round it for a focus ring, each prefixed with the width it was
/// found at.
pub fn audit_node<M: Clone + 'static>(
    node: &crate::Node<M>,
    widths: &[i32],
    height: i32,
) -> Vec<String> {
    let style = StyleSheet::with_theme(slopos_theme_core::current());
    let mut found = Vec::new();
    for &width in widths {
        let child = crate::tree::build_widget_tree(node);
        let room = slopos_theme_core::EdgeInsets::all(slopos_theme_core::space::SM);
        let mut root = crate::layout::PaddingWidget::new(room, child);
        for finding in audit(&mut root, Size::new(width, height), &style) {
            found.push(format!("at {width} px: {finding}"));
        }
    }
    found
}
