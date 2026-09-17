use super::traits::{Widget, WidgetId};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FocusScopeId(pub u32);

/// A focus scope traps Tab navigation within a subtree.
pub struct FocusScope {
    pub id: FocusScopeId,
    /// Widgets in this scope, in tab order.
    pub chain: Vec<WidgetId>,
    /// Which widget was focused when this scope was entered.
    pub restore_to: Option<WidgetId>,
}

pub struct FocusManager {
    focused: Option<WidgetId>,
    /// Where `focused` sits in the tab chain.
    ///
    /// A `WidgetId` comes from a global counter and the whole tree is rebuilt
    /// on every message, so an id does not survive a rebuild — remembering only
    /// the id means focus is lost the moment anything happens. The position in
    /// the chain does survive, because the chain is built by walking the same
    /// view in the same order, so it is what focus is re-derived from.
    focused_index: Option<usize>,
    /// Every focusable widget, in depth-first order.
    tab_chain: Vec<WidgetId>,
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
            tab_chain: Vec::new(),
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
        self.focused_index = index;
    }

    /// Record a non-modifier key press.
    pub fn note_keyboard_input(&mut self) {
        self.keyboard_active = true;
    }

    pub fn note_pointer_input(&mut self) {
        self.keyboard_active = false;
    }

    /// Rebuilds the chain and re-derives `focused` from its position in it.
    ///
    /// Answers the id that should now be told it has focus, when that is a
    /// different widget from the one that held it before — which after a
    /// rebuild it always is, because every widget is new.
    pub fn rebuild_tab_chain(&mut self, root: &dyn Widget) -> Option<WidgetId> {
        self.tab_chain.clear();
        Self::collect_focusable(root, &mut self.tab_chain);
        let index = self.focused_index?;
        let restored = self.active_chain().get(index).copied();
        // A view that lost controls can leave the index past the end; dropping
        // focus is better than moving it somewhere the user did not put it.
        self.focused = restored;
        if restored.is_none() {
            self.focused_index = None;
        }
        restored
    }

    fn collect_focusable(widget: &dyn Widget, chain: &mut Vec<WidgetId>) {
        if widget.focus_policy().is_tab_focusable() {
            chain.push(widget.id());
        }
        for child in widget.children() {
            Self::collect_focusable(child.as_ref(), chain);
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
        let next = chain[index];
        self.focused = Some(next);
        self.focused_index = Some(index);
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
        let prev = chain[index];
        self.focused = Some(prev);
        self.focused_index = Some(index);
    }

    pub fn push_scope(&mut self, focusable_ids: Vec<WidgetId>) -> FocusScopeId {
        let id = FocusScopeId(self.next_scope_id);
        self.next_scope_id += 1;
        let scope = FocusScope {
            id,
            chain: focusable_ids,
            restore_to: self.focused,
        };
        self.scope_stack.push(scope);
        if let Some(scope) = self.scope_stack.last() {
            if let Some(&first) = scope.chain.first() {
                self.focused = Some(first);
            }
        }
        id
    }

    /// Pop the topmost focus scope, restoring the focus it was entered with.
    pub fn pop_scope(&mut self) -> Option<FocusScopeId> {
        if let Some(scope) = self.scope_stack.pop() {
            self.focused = scope.restore_to;
            Some(scope.id)
        } else {
            None
        }
    }
}

impl Default for FocusManager {
    fn default() -> Self {
        Self::new()
    }
}
