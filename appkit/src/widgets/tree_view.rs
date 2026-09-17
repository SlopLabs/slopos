//! A virtualized tree of rows: the shape a file sidebar takes.
//!
//! The application hands over the rows that are visible and gets back which row
//! was hit and whether the hit landed on the disclosure triangle — so the tree's
//! shape, its expansion state and its filesystem stay in the application, and
//! the widget stays a renderer. A tree of a hundred thousand files costs the
//! rows on screen.

use std::any::Any;

use slopos_abi::draw::Color32;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, PointerButton, WidgetEvent,
};
use crate::paint::PaintContext;
use crate::traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetCore};

use super::icon::{IconKind, draw_icon};

/// Indent per depth level, in pixels.
const INDENT: i32 = 14;
/// Overview scrollbar: its width, and how far its right edge sits from the
/// widget's, so the track a press lands on is the bar that is drawn.
const SCROLLBAR_WIDTH: i32 = 4;
const SCROLLBAR_INSET: i32 = 5;
/// How wide the *grab* target is. Wider than the bar, which is a 4px line and
/// not something to ask a pointer to land on.
const SCROLLBAR_GRAB: i32 = 12;
/// Shortest the thumb gets, so a long tree still leaves something to grab.
const THUMB_MIN_HEIGHT: i32 = 24;
const PAD_LEFT: i32 = 8;
const ICON_GAP: i32 = 6;

#[derive(Clone, Debug)]
pub struct TreeRow {
    pub label: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
    /// Drawn in the accent colour: the file open in the editor.
    pub active: bool,
    /// Drawn with the modified dot: an open file with unsaved changes.
    pub modified: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeInput {
    /// A click on the row itself: open a file, or select a directory.
    Activate {
        row: usize,
    },
    /// A click on the disclosure triangle, or a keyboard expand/collapse.
    Toggle {
        row: usize,
    },
    Scroll {
        delta_rows: i32,
    },
    /// The scrollbar was pressed or dragged to an absolute position.
    ScrollTo {
        first_row: usize,
    },
    /// The pointer was released; ends a scrollbar drag.
    Release,
    Key {
        key: Key,
        modifiers: Modifiers,
    },
}

type InputCallback = Box<dyn Fn(TreeInput) -> Box<dyn Any>>;

pub struct TreeViewWidget {
    core: WidgetCore,
    rows: Vec<TreeRow>,
    /// Index (into the application's full row list) of `rows[0]`.
    first_row: usize,
    total_rows: usize,
    selected: Option<usize>,
    focused: bool,
    /// Whether a scrollbar drag is in progress. Given rather than remembered:
    /// the widget tree is rebuilt on every message, so a flag set on the press
    /// would be gone before the first move arrived.
    scroll_dragging: bool,
    row_height: i32,
    on_input: Option<InputCallback>,
    hovered: Option<usize>,
}

impl TreeViewWidget {
    pub fn new(
        rows: Vec<TreeRow>,
        first_row: usize,
        total_rows: usize,
        selected: Option<usize>,
        focused: bool,
        scroll_dragging: bool,
        on_input: Option<InputCallback>,
    ) -> Self {
        Self {
            core: WidgetCore::new(),
            rows,
            first_row,
            total_rows,
            selected,
            focused,
            scroll_dragging,
            row_height: 22,
            on_input,
            hovered: None,
        }
    }

    /// `(thumb rect, span)` for the overview scrollbar, or `None` when every
    /// row fits. One source for what is drawn and what a press lands on.
    fn scrollbar_thumb(&self) -> Option<(Rect, usize)> {
        let rect = self.layout_rect();
        let visible = (rect.height / self.row_height.max(1)).max(1) as usize;
        if self.total_rows <= visible {
            return None;
        }
        let track_h = rect.height;
        let thumb_h = ((visible as i64 * track_h as i64) / self.total_rows as i64)
            .max(THUMB_MIN_HEIGHT as i64)
            .min(track_h as i64) as i32;
        let span = self.total_rows - visible;
        let offset = ((self.first_row.min(span) as i64) * (track_h - thumb_h) as i64
            / span.max(1) as i64) as i32;
        let thumb = Rect::new(
            rect.x + rect.width - SCROLLBAR_INSET,
            rect.y + offset,
            SCROLLBAR_WIDTH,
            thumb_h,
        );
        Some((thumb, span))
    }

    /// Whether a point is on the scrollbar's column — the track, not just the
    /// thumb, since pressing the track is how a list jumps.
    fn point_on_scrollbar(&self, x: i32, y: i32) -> bool {
        let rect = self.layout_rect();
        rect.contains(x, y) && x >= rect.x + rect.width - SCROLLBAR_GRAB
    }

    /// The first row that puts the thumb's middle under `y`.
    fn first_row_at(&self, y: i32) -> usize {
        let Some((thumb, span)) = self.scrollbar_thumb() else {
            return 0;
        };
        let rect = self.layout_rect();
        let usable = rect.height - thumb.height;
        if usable <= 0 {
            return 0;
        }
        let top = (y - rect.y - thumb.height / 2).clamp(0, usable);
        ((top as i64 * span as i64) / usable as i64) as usize
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        let rect = self.layout_rect();
        if self.row_height <= 0 || !rect.contains(rect.x, y) {
            return None;
        }
        let offset = y - rect.y;
        if offset < 0 {
            return None;
        }
        let index = (offset / self.row_height) as usize;
        (index < self.rows.len()).then(|| self.first_row + index)
    }

    /// Whether `x` lands on the disclosure triangle of a row at `depth`.
    fn on_twisty(&self, x: i32, depth: usize) -> bool {
        let rect = self.layout_rect();
        let start = rect.x + PAD_LEFT + depth as i32 * INDENT;
        x >= start - 2 && x < start + self.icon_size() + 2
    }

    fn icon_size(&self) -> i32 {
        (self.row_height - 8).clamp(8, 16)
    }

    fn emit(&self, input: TreeInput, sink: &mut MessageSink) -> EventResponse {
        match &self.on_input {
            Some(cb) => {
                sink.emit_raw(cb(input));
                EventResponse::Consumed
            }
            None => EventResponse::Ignored,
        }
    }
}

impl Widget for TreeViewWidget {
    fn core(&self) -> &WidgetCore {
        &self.core
    }
    fn core_mut(&mut self) -> &mut WidgetCore {
        &mut self.core
    }

    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size {
        self.row_height = ctx.style.row_height;
        constraints.constrain(constraints.max_size())
    }

    fn paint(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        // A deeply indented label has a negative budget left and elides to
        // nothing, but a long one at shallow depth can still reach the edge;
        // the clip is what keeps the sidebar's text out of the code surface.
        ctx.with_clip(rect, |ctx| self.paint_rows(ctx));
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
                if !self.layout_rect().contains(*x, *y) || *button != PointerButton::Left {
                    return EventResponse::Ignored;
                }
                // The scrollbar first: a press on it scrolls rather than
                // opening whatever row happens to be behind it.
                if self.point_on_scrollbar(*x, *y) && self.scrollbar_thumb().is_some() {
                    let first_row = self.first_row_at(*y);
                    self.emit(TreeInput::ScrollTo { first_row }, sink);
                    return EventResponse::CapturePointer;
                }
                let Some(row) = self.row_at(*y) else {
                    return EventResponse::Ignored;
                };
                let entry = &self.rows[row - self.first_row];
                if entry.is_dir && self.on_twisty(*x, entry.depth) {
                    return self.emit(TreeInput::Toggle { row }, sink);
                }
                self.emit(TreeInput::Activate { row }, sink)
            }

            WidgetEvent::PointerMove { x, y } => {
                if self.scroll_dragging {
                    let first_row = self.first_row_at(*y);
                    return self.emit(TreeInput::ScrollTo { first_row }, sink);
                }
                let inside = self.layout_rect().contains(*x, *y);
                let hovered = if inside { self.row_at(*y) } else { None };
                if hovered != self.hovered {
                    self.hovered = hovered;
                    return EventResponse::Consumed;
                }
                EventResponse::Ignored
            }

            WidgetEvent::PointerUp { .. } => {
                // Unconditional, as in the code surface: a press and its
                // release can arrive in one batch with no rebuild between, so
                // this widget's `scroll_dragging` is still the pre-press value.
                // The application ignores a release it did not start.
                self.emit(TreeInput::Release, sink);
                EventResponse::ReleasePointer
            }

            WidgetEvent::PointerLeave => {
                self.hovered = None;
                EventResponse::Ignored
            }

            WidgetEvent::Scroll { delta_y, .. } => {
                let rows = -delta_y / self.row_height.max(1);
                let rows = if rows == 0 {
                    if *delta_y > 0 { -1 } else { 1 }
                } else {
                    rows
                };
                self.emit(TreeInput::Scroll { delta_rows: rows }, sink)
            }

            WidgetEvent::KeyDown { key, modifiers, .. } => {
                if !self.focused {
                    return EventResponse::Ignored;
                }
                self.emit(
                    TreeInput::Key {
                        key: *key,
                        modifiers: *modifiers,
                    },
                    sink,
                )
            }

            _ => EventResponse::Ignored,
        }
    }

    fn role(&self) -> Role {
        Role::List
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::ClickFocus
    }

    fn declares_focus(&self) -> bool {
        self.focused
    }
}

/// `text` shortened with a leading ellipsis until it fits `budget` pixels,
/// measured by `width`.
///
/// Leading, because what distinguishes one long file name from another is
/// usually its end. The measurement is a parameter rather than a paint context
/// so a tab can elide at measure time, when there is no context yet.
pub fn elide(text: &str, budget: i32, width: impl Fn(&str) -> i32) -> String {
    if budget <= 0 {
        // No room is not "all the room": returning the whole string here drew
        // a deeply indented label straight across whatever is to the right.
        return String::new();
    }
    if width(text) <= budget {
        return text.to_string();
    }
    let ellipsis = "…";
    let ell_w = width(ellipsis);
    let mut start = text.len();
    for (index, _) in text.char_indices() {
        if width(&text[index..]) + ell_w <= budget {
            start = index;
            break;
        }
    }
    let mut out = String::from(ellipsis);
    out.push_str(&text[start..]);
    out
}

/// The rows, painted inside whatever clip the caller set.
impl TreeViewWidget {
    fn paint_rows(&self, ctx: &mut PaintContext) {
        let rect = self.layout_rect();
        let style = ctx.style;
        let icon = self.icon_size();
        let text_h = ctx.text_height();

        for (index, row) in self.rows.iter().enumerate() {
            let absolute = self.first_row + index;
            let y = rect.y + index as i32 * self.row_height;
            if y >= rect.y + rect.height {
                break;
            }

            let selected = self.selected == Some(absolute);
            if selected {
                let bg = if self.focused {
                    style.bg_selected
                } else {
                    style.bg_hover
                };
                ctx.fill_rect(rect.x, y, rect.width, self.row_height, bg);
            } else if self.hovered == Some(absolute) {
                ctx.fill_rect(rect.x, y, rect.width, self.row_height, style.bg_hover);
            }

            let indent = rect.x + PAD_LEFT + row.depth as i32 * INDENT;
            let icon_y = y + (self.row_height - icon) / 2;
            let mut x = indent;

            if row.is_dir {
                draw_icon(
                    ctx,
                    if row.expanded {
                        IconKind::ChevronDown
                    } else {
                        IconKind::ChevronRight
                    },
                    x,
                    icon_y,
                    icon,
                    style.text_secondary,
                );
            }
            x += icon + ICON_GAP / 2;

            let glyph_color = if row.is_dir {
                style.text_accent
            } else {
                style.text_secondary
            };
            draw_icon(
                ctx,
                if row.is_dir {
                    if row.expanded {
                        IconKind::FolderOpen
                    } else {
                        IconKind::Folder
                    }
                } else {
                    IconKind::File
                },
                x,
                icon_y,
                icon,
                glyph_color,
            );
            x += icon + ICON_GAP;

            let label_color = if row.active {
                style.text_primary
            } else if row.is_dir {
                style.text_primary
            } else {
                style.text_secondary
            };
            let label_y = y + (self.row_height - text_h) / 2;
            let budget = rect.x + rect.width - x - 16;
            let label = elide(&row.label, budget, |t| ctx.text_width(t));
            ctx.draw_text_transparent(x, label_y, &label, label_color);

            if row.modified {
                draw_icon(
                    ctx,
                    IconKind::Dot,
                    rect.x + rect.width - icon - 6,
                    icon_y,
                    icon,
                    style.text_accent,
                );
            }
        }

        // Overview scrollbar, matching the code surface's.
        if let Some((thumb, _)) = self.scrollbar_thumb() {
            let alpha = if self.scroll_dragging { 0x80 } else { 0x40 };
            ctx.fill_rect_blended(
                thumb.x,
                thumb.y,
                thumb.width,
                thumb.height,
                Color32::new(0xc8, 0xcc, 0xd4, alpha),
            );
        }
    }
}
