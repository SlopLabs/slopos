use slopos_abi::draw::Color32;
use slopos_gfx::image::ImageSampling;
use std::sync::Arc;

use super::constraints::Orientation;
use super::constraints::{
    CrossAxisAlignment, EdgeInsets, ImageScale, Length, ScrollDirection, ScrollbarVisibility,
    TextAlignment,
};
use super::event::{Key, Modifiers};
use super::widgets::code_view::{CodeInput, CodeLine};
use super::widgets::drag_handle::DragInput;
use super::widgets::editor_tabs::{EditorTab, TabInput};
use super::widgets::icon::IconKind;
use super::widgets::line_edit::LineEditInput;
use super::widgets::menu_bar::MenuBarInput;
use super::widgets::tree_view::{TreeInput, TreeRow};

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ButtonStyle {
    #[default]
    Primary,
    Secondary,
    Destructive,
}

#[derive(Clone, Debug)]
pub struct MenuItem {
    pub label: &'static str,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    pub kind: MenuItemKind,
}

#[derive(Clone, Debug)]
pub enum MenuItemKind {
    Action,
    Separator,
    Submenu(Vec<MenuItem>),
}

#[derive(Copy, Clone, Debug)]
pub enum TableColumnWidth {
    Fixed(i32),
    Flex(u16),
}

#[derive(Copy, Clone, Debug)]
pub enum SortIndicator {
    Ascending,
    Descending,
}

/// `x`/`y` are window coordinates; for a keyboard-raised request (Menu key,
/// Shift+F10) they are the selected row's left edge.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ContextMenuAt {
    pub row: usize,
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug)]
pub struct ImageData {
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<[Color32]>,
}

impl ImageData {
    pub fn new(width: u32, height: u32, pixels: Vec<Color32>) -> Option<Self> {
        let required = (width as usize).checked_mul(height as usize)?;
        if width == 0 || height == 0 || pixels.len() < required {
            return None;
        }
        Some(Self {
            width,
            height,
            pixels: Arc::from(pixels.into_boxed_slice()),
        })
    }
}

#[derive(Clone, Debug)]
pub struct TableColumn {
    pub label: String,
    pub width: TableColumnWidth,
    pub sort_indicator: Option<SortIndicator>,
}

/// Declarative tree an app builds in `view()`; the framework diffs it against
/// the previous one to update retained widgets.
pub enum Node<M> {
    Label {
        text: String,
        alignment: TextAlignment,
        wrap: bool,
        max_lines: Option<u32>,
    },
    Button {
        label: String,
        /// `None` leaves the button non-interactive; `Some` emits on click.
        on_press: Option<M>,
        style: ButtonStyle,
        enabled: bool,
    },
    TextField {
        text: String,
        placeholder: String,
        on_change: Option<fn(String) -> M>,
        max_length: Option<usize>,
        read_only: bool,
    },
    Checkbox {
        checked: bool,
        label: String,
        on_toggle: Option<M>,
        enabled: bool,
    },
    /// Orientation follows the parent: horizontal in a VStack, vertical in an HStack.
    Divider,
    Image {
        image: ImageData,
        scale: ImageScale,
        sampling: ImageSampling,
    },
    ProgressBar {
        value: u32,
        label: String,
        color: Option<Color32>,
    },
    /// Label with explicit foreground color (bypasses the theme).
    StyledLabel {
        text: String,
        color: Color32,
        alignment: TextAlignment,
    },

    ScrollView {
        child: Box<Node<M>>,
        direction: ScrollDirection,
        show_scrollbar: ScrollbarVisibility,
        /// Initial scroll offset (preserved across rebuilds).
        scroll_y: i32,
        /// Emitted with the new `offset_y` whenever it changes.
        on_scroll: Option<fn(i32) -> M>,
    },
    ListView {
        item_height: i32,
        selected: Option<usize>,
        on_select: Option<fn(usize) -> M>,
        items: Vec<Node<M>>,
    },
    TabBar {
        tabs: Vec<String>,
        active: usize,
        on_change: Option<fn(usize) -> M>,
        content: Vec<Node<M>>,
    },
    Menu {
        items: Vec<MenuItem>,
        on_action: Option<fn(usize) -> M>,
    },
    Table {
        columns: Vec<TableColumn>,
        rows: Vec<Vec<Node<M>>>,
        row_height: i32,
        selected: Option<usize>,
        on_select: Option<fn(usize) -> M>,
        /// `None` leaves headers unclickable; `Some` emits with the column index.
        on_header_click: Option<fn(usize) -> M>,
        /// Emitted on secondary click and on Menu / Shift+F10, after selection
        /// has moved to the row.
        on_context_menu: Option<fn(ContextMenuAt) -> M>,
    },
    Dialog {
        title: String,
        content: Box<Node<M>>,
        actions: Vec<Node<M>>,
        on_dismiss: Option<M>,
    },
    /// Child floated at an absolute window position, clamped on-screen. A click
    /// outside it or an Escape press emits `on_dismiss`, leaving the app's own
    /// state the single source of truth for whether the popup is open.
    Popup {
        x: i32,
        y: i32,
        child: Box<Node<M>>,
        on_dismiss: Option<M>,
    },

    VStack {
        children: Vec<Node<M>>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    HStack {
        children: Vec<Node<M>>,
        spacing: i32,
        align: CrossAxisAlignment,
    },
    ZStack {
        children: Vec<Node<M>>,
    },
    Padding {
        padding: EdgeInsets,
        child: Box<Node<M>>,
    },
    Spacer {
        size: Length,
    },
    Expand {
        weight: u16,
        child: Box<Node<M>>,
    },

    Background {
        color: Color32,
        child: Box<Node<M>>,
    },
    /// A floating surface: filled, rounded, optionally bordered, with a shadow.
    Card {
        color: Color32,
        border: Option<Color32>,
        radius: i32,
        shadow: bool,
        child: Box<Node<M>>,
    },
    SizedBox {
        width: Option<Length>,
        height: Option<Length>,
        child: Box<Node<M>>,
    },

    Canvas {
        width: i32,
        height: i32,
    },

    /// A viewport of code: the lines given are the lines drawn, and every
    /// gesture comes back in document coordinates.
    CodeView {
        lines: Vec<CodeLine>,
        /// Document index of `lines[0]`.
        first_line: usize,
        total_lines: usize,
        /// Leftmost visible display column.
        first_col: usize,
        tab_width: usize,
        /// `(line, col)` of the caret, in document coordinates.
        cursor: Option<(usize, usize)>,
        selection: Option<((usize, usize), (usize, usize))>,
        show_line_numbers: bool,
        focused: bool,
        /// A selection drag is live; the application sets this between the press
        /// and the release.
        selecting: bool,
        on_input: Option<fn(CodeInput) -> M>,
    },
    /// A virtualized tree of rows, as a file sidebar shows them.
    TreeView {
        rows: Vec<TreeRow>,
        /// Index of `rows[0]` in the application's full row list.
        first_row: usize,
        total_rows: usize,
        selected: Option<usize>,
        focused: bool,
        on_input: Option<fn(TreeInput) -> M>,
    },
    /// Open-document tabs, with a modified marker and a close affordance.
    EditorTabs {
        tabs: Vec<EditorTab>,
        active: usize,
        on_input: Option<fn(TabInput) -> M>,
    },
    /// Menu titles; the application opens the dropdown as a [`Node::Popup`].
    MenuBar {
        titles: Vec<String>,
        open: Option<usize>,
        on_input: Option<fn(MenuBarInput) -> M>,
    },
    /// A single-line input whose text and caret the application owns.
    LineEdit {
        text: String,
        placeholder: String,
        /// Caret position, in characters.
        caret: usize,
        focused: bool,
        icon: Option<IconKind>,
        /// Drawn right-aligned inside the field: a match count, a hint.
        suffix: String,
        invalid: bool,
        on_input: Option<fn(LineEditInput) -> M>,
    },
    /// A draggable divider; reports the start, the pointer's absolute position
    /// and the end of a drag.
    DragHandle {
        orientation: Orientation,
        active: bool,
        on_drag: Option<fn(DragInput) -> M>,
    },

    Empty,
}

pub trait App {
    type Message: Clone + 'static;

    fn view(&self) -> Node<Self::Message>;

    fn update(&mut self, msg: Self::Message) -> Action;

    /// Return `Some(ms)` to receive [`App::tick`] calls at that interval.
    fn tick_interval_ms(&self) -> Option<u64> {
        None
    }

    fn tick(&mut self) -> Action {
        Action::None
    }

    /// Called when a key event is not consumed by any widget.
    fn on_key(&mut self, _key: Key, _modifiers: Modifiers) -> Action {
        Action::None
    }

    /// Called with the selection some time after
    /// [`crate::clipboard::request_paste`].
    fn on_paste(&mut self, _text: String) -> Action {
        Action::None
    }

    /// Called when the compositor resizes the window, before the tree rebuilds.
    ///
    /// An app that sizes its own content in rows — an editor's viewport, a
    /// list's page — needs the window's size, and the size it was launched with
    /// is only the first one it has.
    fn on_resize(&mut self, _width: u32, _height: u32) -> Action {
        Action::None
    }

    /// Read once at startup.
    fn title(&self) -> &str {
        "SlopOS App"
    }

    /// App ID for the compositor (e.g. "org.slopos.sysmon").
    fn app_id(&self) -> &str {
        ""
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    Rebuild,
    Exit,
}
