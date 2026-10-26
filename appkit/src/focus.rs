use slopos_theme_core::stroke;

use super::constraints::Rect;
use super::event::MessageSink;
use super::traits::{Role, Widget, WidgetId};

/// Room round a control for its focus ring, which a view showing it keeps.
pub(crate) const RING_ROOM: i32 = stroke::FOCUS * 2;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FocusScopeId(pub u32);

/// A focus scope traps Tab navigation within a subtree.
pub struct FocusScope {
    pub id: FocusScopeId,
    /// Widgets in this scope, in tab order.
    pub chain: Vec<WidgetId>,
    /// Where focus sat in the enclosing chain when this scope was entered, and
    /// how long that chain was.
    ///
    /// A position and not a `WidgetId`: ids come from a global counter and the
    /// tree is rebuilt on every message, so by the time a scope closes the id
    /// that held focus when it opened designates nothing.
    pub restore_to: Option<(usize, usize)>,
}

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

pub struct FocusManager {
    focused: Option<WidgetId>,
    /// Where `focused` sits in the tab chain.
    ///
    /// Ids come from a global counter and the tree is rebuilt on every
    /// message, so an id does not survive one. A position does: the chain is
    /// built by walking the same view in the same order.
    focused_index: Option<usize>,
    /// How long the chain was when `focused_index` was taken, so a view that
    /// changed shape is not silently re-focused on a different control.
    chain_len_at_focus: Option<usize>,
    /// Every focusable widget, in depth-first order.
    tab_chain: Vec<WidgetId>,
    tab_stops: Vec<Stop>,
    /// `focused`'s stop in the global chain, and how many like it precede it.
    focused_stop: Option<(Stop, usize)>,
    /// The modal layer the chain was built from, when one covers the page.
    layer: Option<Stop>,
    /// Where focus sat on the page when a layer covered it, to go back to.
    beneath: Option<Beneath>,
    /// Empty means the global chain is active.
    scope_stack: Vec<FocusScope>,
    /// Navigating by keyboard; drives focus-ring visibility.
    keyboard_active: bool,
    next_scope_id: u32,
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
            scope_stack: Vec::new(),
            keyboard_active: false,
            next_scope_id: 1,
        }
    }

    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    pub fn is_focused(&self, id: WidgetId) -> bool {
        self.focused == Some(id)
    }

    pub fn is_focus_visible(&self) -> bool {
        self.keyboard_active
    }

    pub fn set_focused(&mut self, id: Option<WidgetId>) {
        let index = id.and_then(|id| self.active_chain().iter().position(|&c| c == id));
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
        self.chain_len_at_focus = index.map(|_| self.active_chain().len());
        self.focused_stop = index.filter(|_| self.scope_stack.is_empty()).and_then(|i| {
            let stop = self.tab_stops.get(i)?;
            let nth = self.tab_stops[..i].iter().filter(|s| *s == stop).count();
            Some((stop.clone(), nth))
        });
    }

    /// Record a non-modifier key press.
    pub fn note_keyboard_input(&mut self) {
        self.keyboard_active = true;
    }

    pub fn note_pointer_input(&mut self) {
        self.keyboard_active = false;
    }

    /// Rebuilds the chain and re-derives `focused` from it: the stop of the
    /// same role and name, as many such stops along, or failing that the stop
    /// of the same role at the same position in a chain of the same length.
    ///
    /// Answers the id that should now be told it has focus, when that is a
    /// different widget from the one that held it before — which after a
    /// rebuild it always is, because every widget is new. A dialog that opens
    /// starts with nothing focused; one that closes gives the page its focus
    /// back.
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
            // `focused` names a widget the rebuild destroyed; leaving it makes
            // `move_focus_next` jump back to the first control.
            self.focused = None;
            return None;
        };
        // A chain that gained or lost a control holds another widget at the old
        // position, so a position is trusted only in a chain of the same length.
        let same_shape = self.chain_len_at_focus == Some(self.active_chain().len());
        let found = self
            .focused_stop
            .as_ref()
            .filter(|_| self.scope_stack.is_empty())
            .and_then(|(stop, nth)| {
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
        let restored = index.and_then(|i| self.active_chain().get(i).copied());
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

    fn active_chain(&self) -> &[WidgetId] {
        if let Some(scope) = self.scope_stack.last() {
            &scope.chain
        } else {
            &self.tab_chain
        }
    }

    pub fn move_focus_next(&mut self) {
        self.keyboard_active = true;
        let chain = self.active_chain();
        if chain.is_empty() {
            return;
        }
        let index = match self
            .focused
            .and_then(|current| chain.iter().position(|&id| id == current))
        {
            Some(pos) => (pos + 1) % chain.len(),
            None => 0,
        };
        self.focused = Some(chain[index]);
        self.set_index(Some(index));
    }

    pub fn move_focus_prev(&mut self) {
        self.keyboard_active = true;
        let chain = self.active_chain();
        if chain.is_empty() {
            return;
        }
        let index = match self
            .focused
            .and_then(|current| chain.iter().position(|&id| id == current))
        {
            Some(0) | None => chain.len() - 1,
            Some(pos) => pos - 1,
        };
        self.focused = Some(chain[index]);
        self.set_index(Some(index));
    }

    pub fn push_scope(&mut self, focusable_ids: Vec<WidgetId>) -> FocusScopeId {
        let id = FocusScopeId(self.next_scope_id);
        self.next_scope_id += 1;
        let scope = FocusScope {
            id,
            chain: focusable_ids,
            restore_to: self.focused_index.zip(self.chain_len_at_focus),
        };
        self.scope_stack.push(scope);
        // Through `set_focused`, so the index and chain length a rebuild
        // restores from describe the scope that is now active.
        if let Some(&first) = self.scope_stack.last().and_then(|s| s.chain.first()) {
            self.set_focused(Some(first));
        }
        id
    }

    /// Pop the topmost focus scope, restoring the focus it was entered with.
    ///
    /// By position, against the chain as it is now: the tree is rebuilt on
    /// every message while a scope is open, so the id that held focus when the
    /// scope was entered names a widget that no longer exists.
    pub fn pop_scope(&mut self) -> Option<FocusScopeId> {
        let scope = self.scope_stack.pop()?;
        let outer_len = self.active_chain().len();
        let restored = match scope.restore_to {
            // Same rule as a rebuild: a chain that gained or lost a control
            // indexes a different widget, and dropping focus cannot be wrong.
            Some((index, len)) if len == outer_len => self.active_chain().get(index).copied(),
            _ => None,
        };
        self.set_focused(restored);
        Some(scope.id)
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
    fn holds(widget: &dyn Widget, id: WidgetId) -> bool {
        widget.id() == id || widget.children().iter().any(|c| holds(c.as_ref(), id))
    }
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

/// [`modal_layer`], mutably.
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

pub(crate) fn reveal(
    widget: &mut dyn Widget,
    id: WidgetId,
    sink: &mut MessageSink,
) -> Option<Rect> {
    if widget.id() == id {
        return Some(widget.focus_rect().inflate(RING_ROOM));
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
