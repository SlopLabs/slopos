use super::traits::{Widget, WidgetId};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    /// Set by AltGr too, which sets `altgr` beside it.
    pub alt: bool,
    pub altgr: bool,
    pub super_key: bool,
    pub caps_lock: bool,
}

impl Modifiers {
    pub fn from_raw(raw: u8) -> Self {
        Self {
            shift: raw & slopos_abi::input::MODIFIER_SHIFT != 0,
            ctrl: raw & slopos_abi::input::MODIFIER_CTRL != 0,
            alt: raw & slopos_abi::input::MODIFIER_ALT != 0,
            altgr: raw & slopos_abi::input::MODIFIER_ALTGR != 0,
            super_key: raw & slopos_abi::input::MODIFIER_SUPER != 0,
            caps_lock: raw & slopos_abi::input::MODIFIER_CAPS_LOCK != 0,
        }
    }

    /// Alt as a shortcut modifier rather than AltGr composing text.
    pub fn plain_alt(&self) -> bool {
        self.alt && !self.altgr
    }
}

pub use slopos_keymap_core::keycode::NamedKey;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Key {
    Char(char),
    Named(NamedKey),
    Unknown,
}

#[derive(Clone, Debug)]
pub enum WidgetEvent {
    PointerDown {
        x: i32,
        y: i32,
        button: PointerButton,
        /// The last key event's: the compositor sends none with a pointer
        /// event, as Wayland does not.
        modifiers: Modifiers,
    },
    PointerUp {
        x: i32,
        y: i32,
        button: PointerButton,
    },
    PointerMove {
        x: i32,
        y: i32,
    },
    Scroll {
        x: i32,
        y: i32,
        delta_x: i32,
        delta_y: i32,
    },
    KeyDown {
        key: Key,
        modifiers: Modifiers,
    },
    KeyUp {
        key: Key,
        modifiers: Modifiers,
    },
    TextInput {
        character: char,
    },
    FocusGained,
    FocusLost,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EventResponse {
    Ignored,
    Consumed,
    /// Not taken, and not to reach what is beneath: a modal layer's answer to
    /// what none of its controls took, which still reaches Tab and its footer.
    Blocked,
}

impl EventResponse {
    pub fn is_consumed(&self) -> bool {
        !matches!(self, EventResponse::Ignored)
    }
}

/// The innermost focusable widget under `(x, y)`, through the topmost child
/// at each level.
pub(crate) fn control_at(widget: &dyn Widget, x: i32, y: i32) -> Option<&dyn Widget> {
    if !widget.layout_rect().contains(x, y) {
        return None;
    }
    let inner = widget
        .children()
        .iter()
        .rev()
        .find(|child| child.layout_rect().contains(x, y))
        .and_then(|child| control_at(child.as_ref(), x, y));
    inner.or_else(|| widget.focus_policy().is_focusable().then_some(widget))
}

/// What the pointer at `(x, y)` lights: the first widget from the root down,
/// through the topmost child under the point, that answers a hover part.
pub(crate) fn lit_at(widget: &dyn Widget, x: i32, y: i32) -> Option<(WidgetId, usize)> {
    if !widget.layout_rect().contains(x, y) {
        return None;
    }
    if let Some(part) = widget.hover_part(x, y) {
        return Some((widget.id(), part));
    }
    let child = widget
        .children()
        .iter()
        .rev()
        .find(|child| child.layout_rect().contains(x, y))?;
    lit_at(child.as_ref(), x, y)
}

/// Type-erased, so the object-safe `Widget` need not know the app's message
/// type.
pub struct MessageSink {
    messages: Vec<Box<dyn std::any::Any>>,
}

impl MessageSink {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
        }
    }

    pub fn emit_raw(&mut self, msg: Box<dyn std::any::Any>) {
        self.messages.push(msg);
    }

    /// The messages of type `M`, leaving the others.
    pub fn drain_typed<M: 'static>(&mut self) -> Vec<M> {
        let mut typed = Vec::new();
        let mut remaining = Vec::new();
        for msg in self.messages.drain(..) {
            match msg.downcast::<M>() {
                Ok(m) => typed.push(*m),
                Err(other) => remaining.push(other),
            }
        }
        self.messages = remaining;
        typed
    }

    pub fn has_messages(&self) -> bool {
        !self.messages.is_empty()
    }
}

impl Default for MessageSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Routes `event` among children that do not overlap: a press or a wheel to
/// the one under the pointer; a move or a release to every one, since a drag
/// leaves the child it began in; anything else to each in turn until one
/// takes it.
pub(crate) fn route(
    children: &mut [Box<dyn Widget>],
    event: &WidgetEvent,
    sink: &mut MessageSink,
) -> EventResponse {
    let at = match event {
        WidgetEvent::PointerDown { x, y, .. } | WidgetEvent::Scroll { x, y, .. } => Some((*x, *y)),
        WidgetEvent::PointerMove { .. } | WidgetEvent::PointerUp { .. } => {
            let mut response = EventResponse::Ignored;
            for child in children {
                if child.event(event, sink).is_consumed() {
                    response = EventResponse::Consumed;
                }
            }
            return response;
        }
        _ => None,
    };
    for child in children {
        if at.is_some_and(|(x, y)| !child.layout_rect().contains(x, y)) {
            continue;
        }
        let r = child.event(event, sink);
        if r.is_consumed() {
            return r;
        }
    }
    EventResponse::Ignored
}
