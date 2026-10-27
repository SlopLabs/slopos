//! AppKit — SlopOS application toolkit.
//!
//! Widget apps implement [`App`], return a [`Node`] tree from `view()`, handle
//! messages in `update()`, and launch with [`run_app()`]. Raw-drawing apps use
//! `slopos-windowing` directly via [`WindowedApp`].
//!
//! # Quick Start
//!
//! ```rust,ignore
//! use slopos_appkit::{App, Action, Node, Text, run_app};
//!
//! struct MyApp;
//! impl App for MyApp {
//!     type Message = MyMsg;
//!     fn view(&self) -> Node<MyMsg> { Text::new("Hello").into() }
//!     fn update(&mut self, msg: MyMsg) -> Action { Action::None }
//! }
//!
//! pub fn main() -> ! { run_app(MyApp) }
//! ```

// Widget fields are read by run_app and tree reconciliation, not by the impls.
#![allow(dead_code)]

pub mod platform;
pub mod text;

pub mod app_icon;
pub mod audit;
pub mod catalogue;
pub mod clipboard;
pub mod components;
pub mod constraints;
pub mod dirty;
pub mod embed;
pub mod event;
pub mod focus;
pub mod input;
pub mod layout;
pub mod node;
pub mod overlay;
pub mod paint;
pub mod run;
pub mod sender;
pub mod style;
pub mod tests;
mod tooltip;
pub mod traits;
pub mod tree;
pub mod widgets;

pub use components::{
    Button, ButtonRow, Disclosure, EmptyState, IconButton, List, ListRow, Notice, Page,
    PropertyList, SearchField, Section, Sidebar, Tag, Text, Toolbar, WindowSpec, Wrap,
};
pub use node::{
    Action, App, BarSegment, ButtonStyle, ContextMenuAt, ImageData, MenuItem, MenuItemKind, Node,
    RadioOption, SegmentFill, SortIndicator, TableColumn, TableColumnWidth,
};
pub use run::run_app;
pub use sender::AppSender;
pub use widgets::code_view::{
    CodeInput, CodeLine, StyledSpan, gutter_width, text_area_reserved, visible_line_count,
};
pub use widgets::drag_handle::DragInput;
pub use widgets::editor_tabs::{EditorTab, TabInput};
pub use widgets::line_edit::{LineEditInput, LineInput};
pub use widgets::menu_bar::MenuBarInput;
pub use widgets::slider::{SliderGrab, SliderInput};
pub use widgets::tree_view::{TreeInput, TreeRow};

pub use constraints::{
    BoxConstraints, CrossAxisAlignment, EdgeInsets, ImageScale, Length, Orientation, Rect,
    ScrollDirection, ScrollbarVisibility, Size, SizePolicy, TextAlignment,
};

pub use event::{
    EventPhase, EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent,
};

pub use dirty::DirtyFlags;
pub use focus::FocusManager;
pub use paint::PaintContext;
pub use style::StyleSheet;
pub use traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId};

pub use slopos_windowing::{ControlFlow, WindowedApp};
pub use slopos_windowing::{ProtocolHandle, UiQueue, UiSender};

pub use slopos_theme_core as theme;
pub use slopos_theme_core::{Icon, Surface, TextRole, Tone};

pub use slopos_gfx::image::ImageSampling;
pub use slopos_gfx::{RenderError, RenderSurface};
