use super::audit::Arrangement;
use super::constraints::{BoxConstraints, Rect, Size};
use super::event::{EventResponse, Key, MessageSink, Modifiers, WidgetEvent};
use super::paint::PaintContext;
use super::style::StyleSheet;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct WidgetId(pub u32);

impl WidgetId {
    pub const NONE: Self = Self(0);
}

static NEXT_WIDGET_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);

pub fn next_widget_id() -> WidgetId {
    WidgetId(NEXT_WIDGET_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

pub struct MeasureCtx<'a> {
    pub style: &'a StyleSheet,
}

/// What every widget carries: `measured` written by [`measure_widget`] alone,
/// `rect` by [`place_widget`].
#[derive(Debug)]
pub struct WidgetCore {
    id: WidgetId,
    rect: Rect,
    measured: Size,
}

impl WidgetCore {
    pub fn new() -> Self {
        Self {
            id: next_widget_id(),
            rect: Rect::ZERO,
            measured: Size::ZERO,
        }
    }

    pub fn id(&self) -> WidgetId {
        self.id
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    pub fn measured(&self) -> Size {
        self.measured
    }
}

impl Default for WidgetCore {
    fn default() -> Self {
        Self::new()
    }
}

/// What a container measures a child with, so [`Widget::measured_size`] is
/// known before layout.
pub fn measure_widget(
    widget: &mut dyn Widget,
    constraints: BoxConstraints,
    ctx: &mut MeasureCtx,
) -> Size {
    let size = widget.measure(constraints, ctx);
    let size = constraints.constrain(size);
    widget.core_mut().measured = size;
    size
}

/// Gives `widget` its final rect; a size is probed with [`measure_widget`].
pub fn place_widget(widget: &mut dyn Widget, rect: Rect) {
    widget.core_mut().rect = rect;
    widget.layout(rect);
}

/// A widget in the retained tree.
///
/// Keys carry no position, so they are offered to every widget in turn until
/// one consumes: a widget that answers `KeyDown` without gating on its own
/// focus takes the key from whatever the user was aiming at.
pub trait Widget {
    fn core(&self) -> &WidgetCore;
    fn core_mut(&mut self) -> &mut WidgetCore;

    /// May run more than once a frame under other constraints, so it keeps no
    /// state but caches.
    fn measure(&mut self, constraints: BoxConstraints, ctx: &mut MeasureCtx) -> Size;

    /// Places the children within `rect`, which [`place_widget`] has recorded.
    fn layout(&mut self, rect: Rect) {
        let _ = rect;
    }

    fn paint(&self, ctx: &mut PaintContext);

    fn event(&mut self, event: &WidgetEvent, sink: &mut MessageSink) -> EventResponse;

    fn role(&self) -> Role {
        Role::None
    }

    fn accessible_name(&self) -> Option<&str> {
        None
    }

    fn focus_policy(&self) -> FocusPolicy {
        FocusPolicy::None
    }

    /// Whether the app built this widget focused, which wins over where the
    /// [`FocusManager`](crate::focus::FocusManager) thinks focus is.
    fn declares_focus(&self) -> bool {
        false
    }

    fn id(&self) -> WidgetId {
        self.core().id()
    }

    fn layout_rect(&self) -> Rect {
        self.core().rect()
    }

    fn measured_size(&self) -> Size {
        self.core().measured()
    }

    fn children(&self) -> &[Box<dyn Widget>] {
        &[]
    }

    fn children_mut(&mut self) -> &mut [Box<dyn Widget>] {
        &mut []
    }

    /// Scroll so `rect`, in window coordinates, shows; returns where it is
    /// then.
    fn reveal(&mut self, rect: Rect, _sink: &mut MessageSink) -> Rect {
        rect
    }

    /// From the top of the measured box to the first line's baseline; `None`
    /// for a widget without text, which a baseline row stands on its bottom.
    fn baseline(&self) -> Option<i32> {
        None
    }

    /// Its share of the room a stack has left; 0 takes none.
    fn flex_weight(&self) -> u16 {
        0
    }

    /// How this widget places its children, for [`crate::audit`].
    fn arrangement(&self) -> Arrangement {
        Arrangement::Flow
    }

    /// What the pointer resting at `(x, y)` on this widget shows: the whole of
    /// a text it had to cut short, or the name of a control that shows only an
    /// icon.
    fn tooltip(&self, x: i32, y: i32) -> Option<&str> {
        let _ = (x, y);
        None
    }

    /// A key no widget took, offered to each widget of the modal layer in turn
    /// before the application hears it: Enter for a default button, Escape
    /// for a cancel one. Answers whether it was taken.
    fn take_unhandled_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        sink: &mut MessageSink,
    ) -> bool {
        let _ = (key, modifiers, sink);
        false
    }

    /// What of this widget the keyboard is on, which a scroll view around it
    /// keeps in view.
    fn focus_rect(&self) -> Rect {
        self.layout_rect()
    }

    /// Which of its parts the pointer at `(x, y)` lights, as a list does a
    /// row, `0` for a control that lights whole; `None` leaves the point to
    /// its children. It paints what [`PaintContext::lit`] says.
    fn hover_part(&self, _x: i32, _y: i32) -> Option<usize> {
        None
    }

    /// Where this widget is, when the app does not keep it, so a rebuild can
    /// hand it to the widget built in its place to show the same.
    fn kept(&self) -> Option<Kept> {
        None
    }

    fn keep(&mut self, at: KeptAt) {
        let _ = at;
    }

    /// Whether this widget covers what is beneath it for the pointer: nothing
    /// under it hears a press or shows a tooltip.
    fn covers(&self) -> bool {
        self.is_modal()
    }

    /// Whether this widget covers what is beneath it for the keyboard as well
    /// as the pointer: Tab moves only among its own controls.
    fn is_modal(&self) -> bool {
        false
    }
}

/// Where a widget is, and what it shows there.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Kept {
    pub shows: u64,
    pub at: KeptAt,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum KeptAt {
    /// How far down a view has scrolled.
    Scroll(i32),
    /// The item the keyboard is on.
    Cursor(Option<usize>),
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Role {
    None,
    Button,
    TextField,
    Checkbox,
    Slider,
    Label,
    List,
    ScrollArea,
    Tab,
    Menu,
    Separator,
    Group,
    /// Buttons the arrow keys move along, as Tab does.
    Toolbar,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum FocusPolicy {
    #[default]
    None,
    TabFocus,
    ClickFocus,
    /// By Tab and by click.
    StrongFocus,
}

impl FocusPolicy {
    pub fn is_focusable(&self) -> bool {
        !matches!(self, FocusPolicy::None)
    }

    pub fn is_tab_focusable(&self) -> bool {
        matches!(self, FocusPolicy::TabFocus | FocusPolicy::StrongFocus)
    }

    pub fn is_click_focusable(&self) -> bool {
        matches!(self, FocusPolicy::ClickFocus | FocusPolicy::StrongFocus)
    }
}
