use slopos_abi::draw::Color32;
use slopos_gfx::image::ImageSampling;
use std::sync::Arc;

use slopos_theme_core::{EdgeInsets, Icon, Surface, TextRole, Tone};

use super::components::{
    Button, ButtonRow, Disclosure, Graph, GraphKey, IconButton, List, PropertyList, Tag, Text,
    WindowSpec,
};
use super::constraints::{CrossAxisAlignment, ImageScale, Length, Orientation, Rect};
use super::event::{Key, Modifiers};
use super::sender::AppSender;
use super::widgets::code_view::{CodeInput, CodeLine};
use super::widgets::drag_handle::DragInput;
use super::widgets::editor_tabs::{EditorTab, TabInput};
use super::widgets::line_edit::LineEditInput;
use super::widgets::menu_bar::MenuBarInput;
use super::widgets::picture::PictureInput;
use super::widgets::slider::{SliderGrab, SliderInput};
use super::widgets::tree_view::{TreeInput, TreeRow};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ButtonStyle {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentFill {
    /// The theme's `n`th categorical colour.
    Series(usize),
    /// Diagonal stripes, as free space is drawn.
    Hatched,
}

#[derive(Clone, Debug)]
pub struct BarSegment {
    pub label: String,
    /// Its share of the bar, in any unit the bar's other segments share.
    pub size: u64,
    pub fill: SegmentFill,
}

#[derive(Clone, Debug)]
pub struct TableColumn {
    pub label: String,
    pub width: TableColumnWidth,
    pub sort_indicator: Option<SortIndicator>,
}

/// What an app's `view` builds, which the framework turns into a widget tree
/// anew after every rebuild.
pub enum Node<M> {
    Text(Text),
    /// A glyph of the icon font, at the size of text in `role`.
    Icon {
        icon: Icon,
        role: TextRole,
        tone: Tone,
    },
    IconButton(IconButton<M>),
    /// A person, by the initial of their name on a circle a colour of the
    /// theme's series, the same for a name wherever it is shown.
    Avatar {
        name: String,
    },
    Button(Button<M>),
    Checkbox {
        checked: bool,
        label: String,
        on_toggle: Option<M>,
        enabled: bool,
    },
    /// A horizontal slider over `min + k * step` up to `max`, and `max`.
    Slider {
        value: u64,
        min: u64,
        max: u64,
        step: u64,
        enabled: bool,
        /// While a drag is live, what its `Begin` carried, until its `End`.
        grab: Option<SliderGrab>,
        on_input: Option<fn(SliderInput) -> M>,
    },
    /// Orientation follows the parent: horizontal in a VStack, vertical in an HStack.
    Divider,
    Image {
        image: ImageData,
        scale: ImageScale,
        sampling: ImageSampling,
    },
    ProgressBar {
        /// A percentage; `None` sweeps a block to and fro by the clock, which
        /// the app animates by answering its tick with [`Action::Repaint`].
        value: Option<u32>,
        label: String,
        tone: Tone,
    },
    /// Segments of a whole in proportion, as a disk's partitions; a sliver of
    /// one still shows.
    PartitionBar {
        segments: Vec<BarSegment>,
        highlight: Option<usize>,
    },
    Graph(Graph),
    GraphKey(GraphKey),
    ButtonRow(ButtonRow<M>),
    PropertyList(PropertyList<M>),
    List(List<M>),
    Tag(Tag),
    Disclosure(Box<Disclosure<M>>),

    /// A view down a child taller than it.
    ScrollView {
        child: Box<Node<M>>,
        scroll_y: i32,
        /// Emitted with the new `offset_y` whenever it changes, for the app to
        /// give back as `scroll_y`; a rebuild in answer would end a thumb drag.
        on_scroll: Option<fn(i32) -> M>,
        /// Reports its end as `i32::MAX`, which given back keeps it there as
        /// it grows.
        follow_end: bool,
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
        selected: Option<usize>,
        on_select: Option<fn(usize) -> M>,
        /// Emitted by Enter and by a click on the row already chosen.
        on_activate: Option<fn(usize) -> M>,
        on_header_click: Option<fn(usize) -> M>,
        /// Emitted on secondary click and on Menu / Shift+F10, after selection
        /// has moved to the row.
        on_context_menu: Option<fn(ContextMenuAt) -> M>,
        /// What the rows are of: a table that shows another starts at the top.
        shows: u64,
    },
    /// A question over the page, which it covers for the pointer and the
    /// keyboard alike.
    Dialog {
        title: String,
        /// Drawn beside the title.
        icon: Option<Box<Node<M>>>,
        content: Box<Node<M>>,
        footer: ButtonRow<M>,
        on_dismiss: Option<M>,
        /// What the card centres over, where it fits; the page when none.
        anchor: Option<Rect>,
    },
    /// A child floated at a window position, kept on screen; a click outside it
    /// or Escape emits `on_dismiss`, and the app closes it.
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
        surface: Surface,
        child: Box<Node<M>>,
    },
    /// A rounded surface inside an edge, with a shadow when it floats above
    /// the page.
    Card {
        surface: Surface,
        shadow: bool,
        child: Box<Node<M>>,
    },
    SizedBox {
        width: Option<Length>,
        height: Option<Length>,
        child: Box<Node<M>>,
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
        /// A selection drag is live, from the press to the release, since the
        /// tree is rebuilt between them.
        selecting: bool,
        /// A scrollbar drag is live, likewise.
        scroll_dragging: bool,
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
        /// A scrollbar drag is live, from the press to the release.
        scroll_dragging: bool,
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
        /// In characters.
        caret: usize,
        focused: bool,
        icon: Option<Icon>,
        /// Drawn right-aligned inside the field: a match count, a hint.
        suffix: String,
        invalid: bool,
        /// Reached by Tab, and reports `Focus` and `Blur`.
        tab_stop: bool,
        on_input: Option<fn(LineEditInput) -> M>,
    },
    /// A picture at `zoom` times its size, centred and moved by `pan`, over
    /// the checks its transparent pixels are judged against; every gesture
    /// over it comes back as a [`PictureInput`].
    Picture {
        image: ImageData,
        zoom: f64,
        pan: (i32, i32),
        /// A drag is live, from the press to the release, since the tree is
        /// rebuilt between them.
        grabbing: bool,
        /// Typed characters come to it.
        focused: bool,
        on_input: Option<fn(PictureInput) -> M>,
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

    /// Called once before the first [`App::view`], with the handle a thread
    /// sends this app messages through.
    fn on_start(&mut self, _sender: AppSender<Self::Message>) -> Action {
        Action::None
    }

    /// Which page the view shows; a rebuild onto another page drops focus.
    fn page_key(&self) -> u64 {
        0
    }

    /// How often [`App::tick`] is called, if at all.
    fn tick_interval_ms(&self) -> Option<u64> {
        None
    }

    fn tick(&mut self) -> Action {
        Action::None
    }

    /// A key no widget took.
    fn on_key(&mut self, _key: Key, _modifiers: Modifiers) -> Action {
        Action::None
    }

    /// The selection, some time after [`crate::clipboard::request_paste`].
    fn on_paste(&mut self, _text: String) -> Action {
        Action::None
    }

    /// The close button was pressed: `false` refuses, to ask first, and the
    /// app then exits itself once the person has answered.
    fn on_close_request(&mut self) -> (bool, Action) {
        (true, Action::None)
    }

    /// The compositor resized the window; the tree rebuilds after.
    fn on_resize(&mut self, _width: u32, _height: u32) -> Action {
        Action::None
    }

    /// Read again after every rebuild; a window showing a document is titled
    /// by [`document_title`].
    fn title(&self) -> String;

    /// Read once, at start.
    fn window(&self) -> WindowSpec;

    /// The id the dock and the compositor know the app by, as
    /// `org.slopos.sysmon`.
    fn app_id(&self) -> &str;
}

/// A window's title that names its document: `notes.txt — Editor`.
pub fn document_title(document: &str, app: &str) -> String {
    format!("{document} — {app}")
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    None,
    /// Paint the tree again without rebuilding it, as a clock-driven widget
    /// needs, keeping what the widgets hold between messages.
    Repaint,
    Rebuild,
    Exit,
}
