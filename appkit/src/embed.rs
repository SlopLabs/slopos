//! A widget tree laid out, painted and given the pointer by the process
//! that holds it, without a window of its own: how the compositor draws its
//! dialogs with an app's components. The holder keeps the keyboard and says
//! which widget shows focus; the tree answers the pointer with its widgets'
//! messages.

use slopos_gfx::DrawBuffer;

use crate::constraints::{BoxConstraints, Rect, Size};
use crate::event::{self, MessageSink, PointerButton, WidgetEvent};
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
    hovered: Option<(WidgetId, Option<usize>)>,
}

impl Embedded {
    pub fn new<M: Clone + 'static>(node: &Node<M>, rect: Rect) -> Self {
        let mut embedded = Self {
            root: tree::build_widget_tree(node),
            style: StyleSheet::with_theme(slopos_theme_core::current()),
            rect,
            pointer: (-1, -1),
            focused: None,
            hovered: None,
        };
        embedded.place();
        embedded
    }

    /// `node` in place of the tree, laid out in `rect`, each scroll view
    /// that shows what one before it showed kept where it was. Nothing
    /// shows focus until [`focus`](Self::focus) says what does.
    pub fn rebuild<M: Clone + 'static>(&mut self, node: &Node<M>, rect: Rect) {
        let mut kept = Vec::new();
        tree::kept_scrolls(self.root.as_ref(), &mut kept);
        self.root = tree::build_widget_tree(node);
        tree::restore_scrolls(self.root.as_mut(), &kept);
        self.rect = rect;
        self.focused = None;
        self.hovered = None;
        self.place();
    }

    fn place(&mut self) {
        let mut ctx = MeasureCtx { style: &self.style };
        let size = Size::new(self.rect.width, self.rect.height);
        let constraints = BoxConstraints::tight(size);
        measure_widget(self.root.as_mut(), constraints, &mut ctx);
        place_widget(self.root.as_mut(), self.rect);
    }

    /// The size `node` lays out at within `max`, for a holder that sizes a
    /// surface to what it holds.
    pub fn natural_size<M: Clone + 'static>(node: &Node<M>, max: Size) -> Size {
        let mut root = tree::build_widget_tree(node);
        let style = StyleSheet::with_theme(slopos_theme_core::current());
        let mut ctx = MeasureCtx { style: &style };
        measure_widget(root.as_mut(), BoxConstraints::loose(max), &mut ctx)
    }

    pub(crate) fn root(&self) -> &dyn Widget {
        self.root.as_ref()
    }

    /// Show focus on the first widget in tree order of `role` named `name`.
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
    }

    /// A turn of the wheel at `(x, y)`, in 120ths of a notch, which scrolls
    /// the innermost view under it that can still move.
    pub fn wheel<M: 'static>(&mut self, x: i32, y: i32, value_v120: i32) -> Vec<M> {
        let event = WidgetEvent::Scroll {
            x,
            y,
            delta_x: 0,
            delta_y: crate::input::wheel_px(value_v120),
        };
        self.send(x, y, &event)
    }

    /// A press or release of the primary button at `(x, y)`, and what the
    /// widgets it reached sent.
    pub fn click<M: 'static>(&mut self, x: i32, y: i32, pressed: bool) -> Vec<M> {
        self.pointer = (x, y);
        let event = match pressed {
            true => WidgetEvent::PointerDown {
                x,
                y,
                button: PointerButton::Left,
                modifiers: Default::default(),
            },
            false => WidgetEvent::PointerUp {
                x,
                y,
                button: PointerButton::Left,
            },
        };
        self.send(x, y, &event)
    }

    /// The primary button went up where the tree is not shown, which ends a
    /// press on it without answering.
    pub fn release_unseen(&mut self) {
        let (x, y) = (self.rect.x - 1, self.rect.y - 1);
        let _: Vec<()> = self.click(x, y, false);
    }

    fn send<M: 'static>(&mut self, x: i32, y: i32, event: &WidgetEvent) -> Vec<M> {
        let mut sink = MessageSink::new();
        let hit = event::hit_test(self.root.as_ref(), x, y).unwrap_or(event::HitTestResult {
            target: WidgetId::NONE,
            chain: Vec::new(),
        });
        event::dispatch_event(self.root.as_mut(), &hit, event, &mut sink);
        sink.drain_typed::<M>()
    }

    /// The pointer is at `(x, y)`: whether the control under it, or the
    /// part of it lit, changed, which changes what is drawn.
    pub fn hover(&mut self, x: i32, y: i32) -> bool {
        self.pointer = (x, y);
        let root = self.root.as_ref();
        let lit = event::hit_test(root, x, y).and_then(|hit| {
            hit.chain
                .into_iter()
                .filter_map(|id| widget(root, id))
                .find(|w| w.focus_policy().is_focusable())
                .map(|w| (w.id(), w.hover_part(x, y)))
        });
        let changed = lit != self.hovered;
        self.hovered = lit;
        changed
    }

    /// Paint what lies in `clip` into `buffer`, which holds the whole of
    /// the area the tree was laid out in.
    pub fn paint(&self, buffer: &mut DrawBuffer<'_>, clip: Rect) {
        let mut ctx = PaintContext::new(buffer, &self.style);
        ctx.focus_visible = true;
        ctx.pointer = self.pointer;
        ctx.with_clip(clip, |ctx| tree::paint_tree(self.root.as_ref(), ctx));
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

fn widget(root: &dyn Widget, id: WidgetId) -> Option<&dyn Widget> {
    if root.id() == id {
        return Some(root);
    }
    root.children()
        .iter()
        .find_map(|child| widget(child.as_ref(), id))
}
