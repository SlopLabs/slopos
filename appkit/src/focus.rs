use slopos_theme_core::stroke;

use super::constraints::Rect;
use super::event::MessageSink;
use super::traits::{Role, Widget, WidgetId};

const RING_GAP: i32 = 1;

/// How far outside a control its focus ring reaches, which a view showing the
/// control keeps room for.
pub(crate) const RING_REACH: i32 = RING_GAP + stroke::FOCUS;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Stop {
    role: Role,
    name: Option<String>,
}

struct Beneath {
    index: Option<usize>,
    chain_len: Option<usize>,
    stop: Option<(Stop, usize)>,
}

/// Where the keyboard is, kept across rebuilds by place rather than by id:
/// every widget is new after a rebuild, but the chain is built by walking the
/// same view in the same order.
pub struct FocusManager {
    focused: Option<WidgetId>,
    focused_index: Option<usize>,
    /// How long the chain was when `focused_index` was taken, so a view that
    /// changed shape is not re-focused on another control.
    chain_len_at_focus: Option<usize>,
    tab_chain: Vec<WidgetId>,
    tab_stops: Vec<Stop>,
    /// `focused`'s stop, and how many like it precede it.
    focused_stop: Option<(Stop, usize)>,
    /// The modal layer the chain was built from, when one covers the page.
    layer: Option<Stop>,
    /// Where focus sat on the page when a layer covered it, to go back to.
    beneath: Option<Beneath>,
    /// Navigating by keyboard, which shows the focus ring.
    keyboard_active: bool,
}

impl FocusManager {
    pub fn new() -> Self {
        Self {
            focused: None,
            focused_index: None,
            chain_len_at_focus: None,
            tab_chain: Vec::new(),
            tab_stops: Vec::new(),
            focused_stop: None,
            layer: None,
            beneath: None,
            keyboard_active: false,
        }
    }

    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    pub fn is_focus_visible(&self) -> bool {
        self.keyboard_active
    }

    pub fn set_focused(&mut self, id: Option<WidgetId>) {
        let index = id.and_then(|id| self.tab_chain.iter().position(|&c| c == id));
        self.focused = id;
        self.set_index(index);
    }

    /// Nothing focused, nor waiting beneath a dialog: the page is another.
    pub fn leave_page(&mut self) {
        self.set_focused(None);
        self.beneath = None;
    }

    fn set_index(&mut self, index: Option<usize>) {
        self.focused_index = index;
        self.chain_len_at_focus = index.map(|_| self.tab_chain.len());
        self.focused_stop = index.and_then(|i| {
            let stop = self.tab_stops.get(i)?;
            let nth = self.tab_stops[..i].iter().filter(|s| *s == stop).count();
            Some((stop.clone(), nth))
        });
    }

    pub fn note_keyboard_input(&mut self) {
        self.keyboard_active = true;
    }

    pub fn note_pointer_input(&mut self) {
        self.keyboard_active = false;
    }

    /// Rebuilds the chain and finds focus in it again: the stop of the same
    /// role and name, as many such stops along, or failing that the stop of
    /// the same role at the same position in a chain of the same length.
    ///
    /// Answers the widget now focused, which a rebuild always makes new. A
    /// dialog that opens starts with nothing focused; one that closes gives
    /// the page its focus back.
    pub fn rebuild_tab_chain(&mut self, root: &dyn Widget) -> Option<WidgetId> {
        self.tab_chain.clear();
        self.tab_stops.clear();
        let layer = modal_layer(root);
        Self::collect_focusable(layer, &mut self.tab_chain, &mut self.tab_stops);
        let layer = (layer.id() != root.id()).then(|| Stop {
            role: layer.role(),
            name: layer.accessible_name().map(String::from),
        });
        if layer != self.layer {
            if self.layer.is_none() {
                self.beneath = Some(Beneath {
                    index: self.focused_index,
                    chain_len: self.chain_len_at_focus,
                    stop: self.focused_stop.clone(),
                });
            }
            let closed = layer.is_none();
            self.layer = layer;
            self.focused = None;
            self.set_index(None);
            let back = self.beneath.take_if(|_| closed);
            let Some(back) = back else {
                return None;
            };
            self.focused_index = back.index;
            self.chain_len_at_focus = back.chain_len;
            self.focused_stop = back.stop;
        }
        let Some(index) = self.focused_index else {
            self.focused = None;
            return None;
        };
        let same_shape = self.chain_len_at_focus == Some(self.tab_chain.len());
        let found = self.focused_stop.as_ref().and_then(|(stop, nth)| {
            self.tab_stops
                .iter()
                .enumerate()
                .filter(|&(_, s)| s == stop)
                .nth(*nth)
                .map(|(i, _)| i)
        });
        let same_kind = self.focused_stop.as_ref().is_none_or(|(stop, _)| {
            self.tab_stops
                .get(index)
                .is_some_and(|s| s.role == stop.role)
        });
        let index = found.or((same_shape && same_kind).then_some(index));
        let restored = index.and_then(|i| self.tab_chain.get(i).copied());
        self.focused = restored;
        self.set_index(restored.and(index));
        restored
    }

    /// The tab stops of `root`'s modal layer, in order.
    pub fn tab_chain_of(root: &dyn Widget) -> Vec<WidgetId> {
        let mut chain = Vec::new();
        Self::collect_focusable(modal_layer(root), &mut chain, &mut Vec::new());
        chain
    }

    fn collect_focusable(widget: &dyn Widget, chain: &mut Vec<WidgetId>, stops: &mut Vec<Stop>) {
        if widget.focus_policy().is_tab_focusable() {
            chain.push(widget.id());
            stops.push(Stop {
                role: widget.role(),
                name: widget.accessible_name().map(String::from),
            });
        }
        for child in widget.children() {
            Self::collect_focusable(child.as_ref(), chain, stops);
        }
    }

    pub fn move_focus_next(&mut self) {
        self.step(true);
    }

    pub fn move_focus_prev(&mut self) {
        self.step(false);
    }

    fn step(&mut self, forward: bool) {
        self.keyboard_active = true;
        let len = self.tab_chain.len();
        if len == 0 {
            return;
        }
        let at = self
            .focused
            .and_then(|current| self.tab_chain.iter().position(|&id| id == current));
        let index = match (at, forward) {
            (Some(at), true) => (at + 1) % len,
            (None, true) => 0,
            (Some(at), false) => (at + len - 1) % len,
            (None, false) => len - 1,
        };
        self.focused = Some(self.tab_chain[index]);
        self.set_index(Some(index));
    }
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new()
    }
}

/// The last modal widget in depth-first order, or `root` when none is: the
/// layer the keyboard reaches.
pub(crate) fn modal_layer(root: &dyn Widget) -> &dyn Widget {
    fn last<'w>(widget: &'w dyn Widget, found: &mut Option<&'w dyn Widget>) {
        if widget.is_modal() {
            *found = Some(widget);
        }
        for child in widget.children() {
            last(child.as_ref(), found);
        }
    }
    let mut found = None;
    last(root, &mut found);
    found.unwrap_or(root)
}

/// The control beside `id` along the toolbar that holds it, wrapping round,
/// when a toolbar does.
pub(crate) fn beside_in_toolbar(
    root: &dyn Widget,
    id: WidgetId,
    forward: bool,
) -> Option<WidgetId> {
    fn toolbar(widget: &dyn Widget, id: WidgetId) -> Option<&dyn Widget> {
        let inner = widget
            .children()
            .iter()
            .find_map(|c| toolbar(c.as_ref(), id));
        inner.or_else(|| (widget.role() == Role::Toolbar && holds(widget, id)).then_some(widget))
    }
    let toolbar = toolbar(modal_layer(root), id)?;
    let mut stops = Vec::new();
    FocusManager::collect_focusable(toolbar, &mut stops, &mut Vec::new());
    let at = stops.iter().position(|&stop| stop == id)?;
    let step = if forward { 1 } else { stops.len() - 1 };
    Some(stops[(at + step) % stops.len()])
}

pub(crate) fn holds(widget: &dyn Widget, id: WidgetId) -> bool {
    widget.id() == id || widget.children().iter().any(|c| holds(c.as_ref(), id))
}

pub(crate) fn modal_layer_mut(root: &mut dyn Widget) -> &mut dyn Widget {
    fn path(widget: &dyn Widget, at: &mut Vec<usize>, best: &mut Option<Vec<usize>>) {
        if widget.is_modal() {
            *best = Some(at.clone());
        }
        for (i, child) in widget.children().iter().enumerate() {
            at.push(i);
            path(child.as_ref(), at, best);
            at.pop();
        }
    }
    let mut best = None;
    path(root, &mut Vec::new(), &mut best);
    let mut layer = root;
    for i in best.unwrap_or_default() {
        layer = layer.children_mut()[i].as_mut();
    }
    layer
}

/// Scrolls every view holding `id` so it shows with its ring; answers where it
/// is then.
pub(crate) fn reveal(
    widget: &mut dyn Widget,
    id: WidgetId,
    sink: &mut MessageSink,
) -> Option<Rect> {
    if widget.id() == id {
        return Some(widget.focus_rect().inflate(RING_REACH));
    }
    let mut found = None;
    for child in widget.children_mut() {
        found = reveal(child.as_mut(), id, sink);
        if found.is_some() {
            break;
        }
    }
    Some(widget.reveal(found?, sink))
}
