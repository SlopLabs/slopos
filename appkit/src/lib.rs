//! SlopOS's application toolkit: an app implements [`App`] and [`run_app`]
//! opens its window. `GUIDE.md` is what to read before writing one.

pub mod app_icon;
pub mod audit;
pub mod catalogue;
pub mod clipboard;
pub mod components;
pub mod constraints;
pub mod embed;
pub mod event;
pub mod focus;
mod input;
pub mod layout;
pub mod node;
pub mod paint;
pub mod run;
pub mod sender;
pub mod style;
pub mod tests;
pub mod text;
mod tooltip;
pub mod traits;
pub mod tree;
pub mod widgets;

pub use components::{
    Button, ButtonRow, Dialog, Disclosure, EmptyState, IconButton, List, ListRow, Notice, Page,
    PropertyList, RowButton, SIDEBAR_WIDTH, SearchField, Section, Sidebar, StatusBar, Tag, Text,
    Toolbar, WindowSpec, Wrap, column, row,
};
pub use node::{
    Action, App, BarSegment, ButtonStyle, ContextMenuAt, ImageData, MenuItem, MenuItemKind, Node,
    SegmentFill, SortIndicator, TableColumn, TableColumnWidth, document_title,
};
pub use run::run_app;
pub use sender::AppSender;
pub use widgets::code_view::{
    CodeInput, CodeLine, StyledSpan, gutter_width, text_area_reserved, visible_line_count,
};
pub use widgets::drag_handle::DragInput;
pub use widgets::editor_tabs::{EditorTab, TabInput};
pub use widgets::icon_button::ICON_BUTTON_SIDE;
pub use widgets::line_edit::{LineEditInput, LineInput};
pub use widgets::menu_bar::MenuBarInput;
pub use widgets::slider::{SliderGrab, SliderInput};
pub use widgets::tree_view::{TreeInput, TreeRow};

pub use constraints::{
    BoxConstraints, CrossAxisAlignment, ImageScale, Length, Orientation, Rect, Size, TextAlignment,
};

pub use event::{EventResponse, Key, MessageSink, Modifiers, NamedKey, PointerButton, WidgetEvent};

pub use focus::FocusManager;
pub use paint::PaintContext;
pub use style::StyleSheet;
pub use traits::{FocusPolicy, MeasureCtx, Role, Widget, WidgetId};

pub use slopos_theme_core as theme;
pub use slopos_theme_core::{EdgeInsets, Icon, Surface, TextRole, Tone};

pub use slopos_gfx::image::ImageSampling;
