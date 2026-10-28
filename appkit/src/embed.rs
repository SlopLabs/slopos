//! A widget tree the process holding it lays out, paints and gives the pointer,
//! as the compositor draws its dialogs; the holder keeps the keyboard.

use slopos_gfx::DrawBuffer;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{EventResponse, MessageSink, PointerButton, WidgetEvent, lit_at};
use crate::node::Node;
use crate::paint::PaintContext;
use crate::style::StyleSheet;
use crate::traits::{MeasureCtx, Role, Widget, WidgetId, measure_widget, place_widget};
use crate::tree;

pub struct Embedded {
    root: Box<dyn Widget>,
    style: StyleSheet,
    rect: Rect,
    pointer: (i32, i32),
    focused: Option<WidgetId>,
    hovered: Option<(WidgetId, usize)>,
}

impl Embedded {
    pub fn new<M: Clone + 'static>(node: &Node<M>, rect: Rect) -> Self {
        let mut embedded = Self::unplaced(node);
        embedded.rect = rect;
        embedded.place();
        embedded
    }

    fn unplaced<M: Clone + 'static>(node: &Node<M>) -> Self {
        Self {
            root: tree::build_widget_tree(node),
            style: StyleSheet::current(),
            rect: Rect::ZERO,
            pointer: (-1, -1),
            focused: None,
            hovered: None,
        }
    }

    /// `node` laid out at the origin at the size it takes within `max`.
    pub fn natural<M: Clone + 'static>(node: &Node<M>, max: Size) -> Self {
        let mut embedded = Self::unplaced(node);
        let mut ctx = MeasureCtx {
            style: &embedded.style,
        };
        let size = measure_widget(embedded.root.as_mut(), BoxConstraints::loose(max), &mut ctx);
        embedded.rect = Rect::new(0, 0, size.width, size.height);
        embedded.place();
        embedded
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// `node` in place of the tree, laid out in `rect`, each view that shows
    /// what one before it showed kept where it was scrolled to, and each
    /// menu's keyboard row. Nothing shows focus until [`focus`](Self::focus)
    /// says what does.
    pub fn rebuild<M: Clone + 'static>(&mut self, node: &Node<M>, rect: Rect) {
        let mut kept = Vec::new();
        tree::kept(self.root.as_ref(), &mut kept);
        self.root = tree::build_widget_tree(node);
        tree::restore_kept(self.root.as_mut(), &kept);
        self.rect = rect;
        self.focused = None;
        self.place();
    }

    fn place(&mut self) {
        let mut ctx = MeasureCtx { style: &self.style };
        let size = Size::new(self.rect.width, self.rect.height);
        measure_widget(self.root.as_mut(), BoxConstraints::tight(size), &mut ctx);
        place_widget(self.root.as_mut(), self.rect);
        self.hovered = self.lit();
    }

    fn lit(&self) -> Option<(WidgetId, usize)> {
        lit_at(self.root.as_ref(), self.pointer.0, self.pointer.1)
    }

    pub(crate) fn root(&self) -> &dyn Widget {
        self.root.as_ref()
    }

    /// Shows focus on the first widget in tree order of `role` named `name`.
    pub fn focus(&mut self, role: Role, name: &str) {
        let found = find(self.root.as_ref(), &|w| {
            w.role() == role && w.accessible_name() == Some(name)
        });
        let mut sink = MessageSink::new();
        if let Some(old) = self.focused.take() {
            tree::send_to_id(self.root.as_mut(), old, &WidgetEvent::FocusLost, &mut sink);
        }
        if let Some(new) = found {
            tree::send_to_id(
                self.root.as_mut(),
                new,
                &WidgetEvent::FocusGained,
                &mut sink,
            );
            crate::focus::reveal(self.root.as_mut(), new, &mut sink);
        }
        self.focused = found;
        self.hovered = self.lit();
    }

    /// A turn of the wheel at `(x, y)`, in 120ths of a notch, which scrolls
    /// the innermost view under it that can still move.
    pub fn wheel<M: 'static>(&mut self, x: i32, y: i32, value_v120: i32) -> Vec<M> {
        self.send(&WidgetEvent::Scroll {
            x,
            y,
            delta_x: 0,
            delta_y: crate::input::wheel_px(value_v120),
        })
    }

    /// A press or release of the primary button at `(x, y)`, and what the
    /// widgets it reached sent.
    pub fn click<M: 'static>(&mut self, x: i32, y: i32, pressed: bool) -> Vec<M> {
        self.pointer = (x, y);
        let button = PointerButton::Left;
        self.send(&match pressed {
            true => WidgetEvent::PointerDown {
                x,
                y,
                button,
                modifiers: Default::default(),
            },
            false => WidgetEvent::PointerUp { x, y, button },
        })
    }

    /// The primary button went up where the tree is not shown, which ends a
    /// press on it without answering.
    pub fn release_unseen(&mut self) {
        let (x, y) = (self.rect.x - 1, self.rect.y - 1);
        let _: Vec<()> = self.click(x, y, false);
    }

    /// What the widgets `event` reached sent; it may have moved what lies
    /// under the pointer.
    fn send<M: 'static>(&mut self, event: &WidgetEvent) -> Vec<M> {
        let mut sink = MessageSink::new();
        self.root.event(event, &mut sink);
        self.hovered = self.lit();
        sink.drain_typed::<M>()
    }

    /// The pointer moved to `(x, y)`, which drags what a press holds:
    /// whether that, or the part lit, changed what is drawn. What a move
    /// sends is dropped, so a slider here does not drag.
    pub fn hover(&mut self, x: i32, y: i32) -> bool {
        self.pointer = (x, y);
        let before = self.hovered;
        let mut sink = MessageSink::new();
        let moved = self
            .root
            .event(&WidgetEvent::PointerMove { x, y }, &mut sink);
        self.hovered = self.lit();
        moved == EventResponse::Consumed || self.hovered != before
    }

    /// Paints what lies in `clip` into `buffer`, which holds the whole of the
    /// area the tree was laid out in.
    pub fn paint(&self, buffer: &mut DrawBuffer<'_>, clip: Rect) {
        let mut ctx = PaintContext::new(buffer, &self.style);
        ctx.focus_visible = true;
        ctx.hovered = self.lit();
        ctx.with_clip(clip, |ctx| self.root.paint(ctx));
    }
}

fn find(widget: &dyn Widget, matches: &dyn Fn(&dyn Widget) -> bool) -> Option<WidgetId> {
    if matches(widget) {
        return Some(widget.id());
    }
    widget
        .children()
        .iter()
        .find_map(|child| find(child.as_ref(), matches))
}
