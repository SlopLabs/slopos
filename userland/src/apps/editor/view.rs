//! The editor's view: state in, `Node` tree out.
//!
//! Layout is computed in two places that must agree — here, where the panes are
//! placed, and in [`code_viewport`], which tells the document how many rows will
//! be drawn. They agree because the second is derived from the same constants
//! the first lays out with, and because the code surface renders exactly the
//! rows it is handed rather than as many as fit.

use slopos_abi::draw::Color32;
use slopos_appkit::widgets::code_view::{CodeLine, StyledSpan};
use slopos_appkit::widgets::editor_tabs::EditorTab;
use slopos_appkit::widgets::tree_view::TreeRow;
use slopos_appkit::{
    ButtonStyle, CrossAxisAlignment, EdgeInsets, IconKind, Length, MenuItem, MenuItemKind, Node,
    Orientation, TextAlignment,
};

use slopos_editor_core::document::{Document, file_name};
use slopos_editor_core::search::{self, SearchOptions};

use super::commands::{Command, MENUS, MenuEntry};
use super::prompt::Prompt;
use super::theme;
use super::{Dialog, EditorApp, EditorMsg, Focus};

pub const DEFAULT_WIDTH: i32 = 1180;
pub const DEFAULT_HEIGHT: i32 = 760;

/// Chrome heights. The style sheet owns the look; these are the pane sizes the
/// application composes with, and the viewport maths reads them back.
pub const MENU_BAR_HEIGHT: i32 = 30;
pub const TAB_BAR_HEIGHT: i32 = 34;
pub const STATUS_BAR_HEIGHT: i32 = 24;
pub const PROMPT_BAR_HEIGHT: i32 = 36;
pub const SIDEBAR_HEADER_HEIGHT: i32 = 30;
pub const SPLITTER_WIDTH: i32 = 6;
/// Narrowest the code surface may become when the sidebar is dragged wide.
pub const MIN_CODE_WIDTH: i32 = 320;

const PALETTE_WIDTH: i32 = 620;
const PALETTE_ROW_HEIGHT: i32 = 26;
pub const PALETTE_MAX_ROWS: usize = 12;

/// Rows the code surface will draw, and columns it can show.
pub fn code_viewport(
    window_width: i32,
    window_height: i32,
    sidebar_visible: bool,
    sidebar_width: i32,
    prompt_bar: bool,
    line_count: Option<usize>,
    show_line_numbers: bool,
) -> (usize, usize) {
    let cell_h = slopos_appkit::text::cell_height().max(1);
    let cell_w = slopos_appkit::text::cell_width().max(1);

    let mut height = window_height - MENU_BAR_HEIGHT - TAB_BAR_HEIGHT - STATUS_BAR_HEIGHT;
    if prompt_bar {
        height -= PROMPT_BAR_HEIGHT;
    }
    let lines = slopos_appkit::visible_line_count(height.max(0), cell_h);

    let mut width = window_width;
    if sidebar_visible {
        width -= sidebar_width + SPLITTER_WIDTH;
    }
    let gutter = slopos_appkit::gutter_width(line_count.unwrap_or(1), cell_w, show_line_numbers);
    let cols = ((width - gutter - 16).max(0) / cell_w) as usize;
    (lines, cols.max(1))
}

pub fn tree_rows_visible(window_height: i32) -> usize {
    // Read from the style sheet rather than restated here: the tree widget
    // measures its rows with the same value, and a copy would drift.
    let row_h = slopos_appkit::StyleSheet::dark().row_height.max(1);
    let height = window_height - MENU_BAR_HEIGHT - SIDEBAR_HEADER_HEIGHT - STATUS_BAR_HEIGHT;
    (height.max(0) / row_h) as usize
}

pub fn build(app: &EditorApp) -> Node<EditorMsg> {
    let mut layers: Vec<Node<EditorMsg>> = vec![main_column(app)];

    if let Some((menu, x, y)) = app.menu_open {
        layers.push(menu_popup(menu, x, y));
    }
    if app.prompt.is_overlay() {
        layers.push(overlay_prompt(app));
    }
    if let Some(dialog) = &app.dialog {
        layers.push(dialog_node(app, dialog));
    }

    Node::ZStack { children: layers }
}

fn main_column(app: &EditorApp) -> Node<EditorMsg> {
    let mut children = vec![menu_bar(app), body(app)];
    children.push(status_bar(app));
    Node::VStack {
        children,
        spacing: 0,
        align: CrossAxisAlignment::Stretch,
    }
}

fn menu_bar(app: &EditorApp) -> Node<EditorMsg> {
    let titles: Vec<String> = MENUS.iter().map(|m| m.title.to_string()).collect();
    let doc = app.doc();
    let title = super::window_title(doc.path(), doc.is_modified());

    Node::SizedBox {
        width: None,
        height: Some(Length::Px(MENU_BAR_HEIGHT)),
        child: Box::new(Node::Background {
            color: bg_titlebar(),
            child: Box::new(Node::HStack {
                spacing: 0,
                align: CrossAxisAlignment::Center,
                children: vec![
                    Node::MenuBar {
                        titles,
                        open: app.menu_open.map(|(index, _, _)| index),
                        on_input: Some(EditorMsg::MenuBar),
                    },
                    Node::Expand {
                        weight: 1,
                        child: Box::new(Node::StyledLabel {
                            text: title,
                            color: Color32::rgb(0xa9, 0xaf, 0xbc),
                            alignment: TextAlignment::Center,
                        }),
                    },
                ],
            }),
        }),
    }
}

fn body(app: &EditorApp) -> Node<EditorMsg> {
    let mut children: Vec<Node<EditorMsg>> = Vec::new();
    if app.sidebar_visible {
        children.push(Node::SizedBox {
            width: Some(Length::Px(app.sidebar_width)),
            height: None,
            child: Box::new(sidebar(app)),
        });
        children.push(Node::DragHandle {
            orientation: Orientation::Vertical,
            active: app.sidebar_dragging,
            on_drag: Some(EditorMsg::SidebarDrag),
        });
    }
    children.push(Node::Expand {
        weight: 1,
        child: Box::new(editor_column(app)),
    });

    Node::Expand {
        weight: 1,
        child: Box::new(Node::HStack {
            children,
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        }),
    }
}

fn sidebar(app: &EditorApp) -> Node<EditorMsg> {
    let root = app.tree.root_path().to_string();
    let label = file_name(&root).to_uppercase();

    let header = Node::SizedBox {
        width: None,
        height: Some(Length::Px(SIDEBAR_HEADER_HEIGHT)),
        child: Box::new(Node::Padding {
            padding: EdgeInsets::new(0, 10, 0, 10),
            child: Box::new(Node::HStack {
                spacing: 6,
                align: CrossAxisAlignment::Center,
                children: vec![
                    Node::StyledLabel {
                        text: label,
                        color: Color32::rgb(0xa9, 0xaf, 0xbc),
                        alignment: TextAlignment::Start,
                    },
                    Node::Expand {
                        weight: 1,
                        child: Box::new(Node::Empty),
                    },
                    Node::Button {
                        label: String::from("Open…"),
                        on_press: Some(EditorMsg::Run(Command::OpenFolder)),
                        style: ButtonStyle::Secondary,
                        enabled: true,
                    },
                ],
            }),
        }),
    };

    let visible = tree_rows_visible(app.window_height);
    let rows = visible_tree_rows(app, visible);

    Node::Background {
        color: bg_sidebar(),
        child: Box::new(Node::VStack {
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
            children: vec![
                header,
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::TreeView {
                        rows,
                        first_row: app.tree_scroll,
                        total_rows: app.tree_row_count(),
                        selected: app.tree_selected,
                        focused: app.focus == Focus::Tree,
                        on_input: Some(EditorMsg::Tree),
                    }),
                },
            ],
        }),
    }
}

fn visible_tree_rows(app: &EditorApp, visible: usize) -> Vec<TreeRow> {
    let open_paths: Vec<(&str, bool)> = app
        .documents()
        .iter()
        .filter_map(|d| d.path().map(|p| (p, d.is_modified())))
        .collect();
    let active_path = app.doc().path();

    app.tree_rows(app.tree_scroll, visible)
        .into_iter()
        .map(|(name, depth, is_dir, expanded, path)| {
            let modified = open_paths.iter().any(|(p, m)| *p == path.as_str() && *m);
            TreeRow {
                label: name,
                depth,
                is_dir,
                expanded,
                active: active_path == Some(path.as_str()),
                modified,
            }
        })
        .collect()
}

fn editor_column(app: &EditorApp) -> Node<EditorMsg> {
    let tabs: Vec<EditorTab> = app
        .documents()
        .iter()
        .map(|d| EditorTab {
            title: d.title().to_string(),
            modified: d.is_modified(),
        })
        .collect();

    let mut children = vec![
        Node::SizedBox {
            width: None,
            height: Some(Length::Px(TAB_BAR_HEIGHT)),
            child: Box::new(Node::EditorTabs {
                tabs,
                active: app.active_index(),
                on_input: Some(EditorMsg::Tab),
            }),
        },
        Node::Expand {
            weight: 1,
            child: Box::new(code_view(app)),
        },
    ];

    if app.prompt.is_open() && !app.prompt.is_overlay() {
        children.push(Node::SizedBox {
            width: None,
            height: Some(Length::Px(PROMPT_BAR_HEIGHT)),
            child: Box::new(prompt_bar(app)),
        });
    }

    Node::VStack {
        children,
        spacing: 0,
        align: CrossAxisAlignment::Stretch,
    }
}

fn code_view(app: &EditorApp) -> Node<EditorMsg> {
    let doc = app.doc();
    let first = doc.viewport.first_line;
    let count = doc.viewport.visible_lines.max(1);
    let total = doc.buffer.line_count();
    let query = app.active_search_query();
    let options = app.search_options();

    let mut lines = Vec::with_capacity(count);
    for line in first..(first + count).min(total) {
        lines.push(code_line(doc, line, query.as_deref(), options));
    }

    let selection = doc.cursor.selection().map(|range| {
        (
            (range.start.line, range.start.col),
            (range.end.line, range.end.col),
        )
    });

    Node::CodeView {
        lines,
        first_line: first,
        total_lines: total,
        first_col: doc.viewport.first_col,
        tab_width: doc.indent().width(),
        cursor: Some((doc.cursor.position.line, doc.cursor.position.col)),
        selection,
        show_line_numbers: app.line_numbers_visible(),
        focused: app.focus == Focus::Editor,
        selecting: app.selecting,
        on_input: Some(EditorMsg::Code),
    }
}

fn code_line(doc: &Document, line: usize, query: Option<&str>, options: SearchOptions) -> CodeLine {
    let text = doc.buffer.line(line).to_string();
    let spans = doc
        .line_spans(line)
        .into_iter()
        .map(|span| StyledSpan {
            start: span.start,
            end: span.end,
            color: theme::token_color(span.kind),
        })
        .collect();

    let highlights = match query {
        Some(q) if !q.is_empty() => search::find_in_line(&doc.buffer, line, q, options)
            .into_iter()
            .map(|r| (r.start.col, r.end.col))
            .collect(),
        _ => Vec::new(),
    };

    CodeLine {
        number: line,
        text,
        spans,
        highlights,
    }
}

fn prompt_bar(app: &EditorApp) -> Node<EditorMsg> {
    let mut fields: Vec<Node<EditorMsg>> = Vec::new();

    match &app.prompt {
        Prompt::Find {
            query,
            replace,
            on_replace,
            total,
            current,
        } => {
            let suffix = if query.text.is_empty() {
                String::new()
            } else if *total == 0 {
                String::from("No results")
            } else {
                format!("{current}/{total}")
            };
            fields.push(Node::Expand {
                weight: 1,
                child: Box::new(Node::LineEdit {
                    text: query.text.clone(),
                    placeholder: String::from("Find"),
                    caret: query.caret,
                    focused: app.focus == Focus::Prompt && !*on_replace,
                    icon: Some(IconKind::Search),
                    suffix,
                    invalid: !query.text.is_empty() && *total == 0,
                    on_input: Some(EditorMsg::Prompt),
                }),
            });
            if let Some(replace) = replace {
                fields.push(Node::Expand {
                    weight: 1,
                    child: Box::new(Node::LineEdit {
                        text: replace.text.clone(),
                        placeholder: String::from("Replace with"),
                        caret: replace.caret,
                        focused: app.focus == Focus::Prompt && *on_replace,
                        icon: None,
                        suffix: String::new(),
                        invalid: false,
                        on_input: Some(EditorMsg::PromptReplace),
                    }),
                });
                fields.push(Node::Button {
                    label: String::from("All"),
                    on_press: Some(EditorMsg::Run(Command::ReplaceAll)),
                    style: ButtonStyle::Secondary,
                    enabled: true,
                });
            }
            fields.push(Node::Button {
                label: String::from("<"),
                on_press: Some(EditorMsg::Run(Command::FindPrev)),
                style: ButtonStyle::Secondary,
                enabled: true,
            });
            fields.push(Node::Button {
                label: String::from(">"),
                on_press: Some(EditorMsg::Run(Command::FindNext)),
                style: ButtonStyle::Secondary,
                enabled: true,
            });
        }
        other => {
            let (text, caret) = match other.input_text() {
                Some(pair) => pair,
                None => (String::new(), 0),
            };
            fields.push(Node::StyledLabel {
                text: format!("{}:", other.title()),
                color: Color32::rgb(0xa9, 0xaf, 0xbc),
                alignment: TextAlignment::Start,
            });
            fields.push(Node::Expand {
                weight: 1,
                child: Box::new(Node::LineEdit {
                    text,
                    placeholder: other.placeholder().to_string(),
                    caret,
                    focused: app.focus == Focus::Prompt,
                    icon: None,
                    suffix: String::new(),
                    invalid: false,
                    on_input: Some(EditorMsg::Prompt),
                }),
            });
        }
    }

    Node::Background {
        color: bg_panel(),
        child: Box::new(Node::Padding {
            padding: EdgeInsets::new(4, 8, 4, 8),
            child: Box::new(Node::HStack {
                children: fields,
                spacing: 6,
                align: CrossAxisAlignment::Center,
            }),
        }),
    }
}

fn status_bar(app: &EditorApp) -> Node<EditorMsg> {
    let doc = app.doc();
    let position = doc.cursor.position;
    let selection = doc
        .cursor
        .selection()
        .map(|r| format!("  ({} selected)", doc.buffer.count_chars(r)))
        .unwrap_or_default();

    let left = if app.status.is_empty() {
        doc.path().unwrap_or(doc.title()).to_string()
    } else {
        app.status.clone()
    };

    let right = format!(
        "Ln {}, Col {}{}   {}   {}   {}",
        position.line + 1,
        position.col + 1,
        selection,
        doc.indent().label(),
        doc.line_ending().label(),
        doc.language().label(),
    );

    Node::SizedBox {
        width: None,
        height: Some(Length::Px(STATUS_BAR_HEIGHT)),
        child: Box::new(Node::Background {
            color: bg_titlebar(),
            child: Box::new(Node::Padding {
                padding: EdgeInsets::new(0, 10, 0, 10),
                child: Box::new(Node::HStack {
                    spacing: 8,
                    align: CrossAxisAlignment::Center,
                    children: vec![
                        Node::Expand {
                            weight: 1,
                            child: Box::new(Node::StyledLabel {
                                text: left,
                                color: Color32::rgb(0xa9, 0xaf, 0xbc),
                                alignment: TextAlignment::Start,
                            }),
                        },
                        Node::StyledLabel {
                            text: right,
                            color: Color32::rgb(0x8b, 0x92, 0x9e),
                            alignment: TextAlignment::End,
                        },
                    ],
                }),
            }),
        }),
    }
}

fn menu_popup(menu: usize, x: i32, y: i32) -> Node<EditorMsg> {
    let Some(def) = MENUS.get(menu) else {
        return Node::Empty;
    };
    let items: Vec<MenuItem> = def
        .entries
        .iter()
        .map(|entry| match entry {
            MenuEntry::Separator => MenuItem {
                label: "",
                shortcut: None,
                enabled: false,
                kind: MenuItemKind::Separator,
            },
            MenuEntry::Item(command) => MenuItem {
                label: command.label(),
                shortcut: command.shortcut(),
                enabled: true,
                kind: MenuItemKind::Action,
            },
        })
        .collect();

    Node::Popup {
        x,
        y,
        child: Box::new(Node::Menu {
            items,
            on_action: Some(EditorMsg::MenuItem),
        }),
        on_dismiss: Some(EditorMsg::DismissMenu),
    }
}

fn overlay_prompt(app: &EditorApp) -> Node<EditorMsg> {
    let (text, caret) = app.prompt.input_text().unwrap_or((String::new(), 0));
    let width = PALETTE_WIDTH.min(app.window_width - 80).max(240);
    let x = (app.window_width - width) / 2;
    let y = MENU_BAR_HEIGHT + 40;

    // The list shows a window of the results, scrolled to keep the selection
    // in it: without that, arrowing past the twelfth match moves a selection
    // the list cannot show and Enter opens something invisible.
    let first = app.overlay_first_row();
    let rows: Vec<Node<EditorMsg>> = match &app.prompt {
        Prompt::Palette {
            results, selected, ..
        } => results
            .iter()
            .enumerate()
            .skip(first)
            .take(PALETTE_MAX_ROWS)
            .map(|(index, command)| palette_row(command, index == *selected))
            .collect(),
        Prompt::FileFinder {
            results, selected, ..
        } => results
            .iter()
            .enumerate()
            .skip(first)
            .take(PALETTE_MAX_ROWS)
            .map(|(index, path)| finder_row(app, path, index == *selected))
            .collect(),
        _ => Vec::new(),
    };

    let selected = match &app.prompt {
        Prompt::Palette { selected, .. } | Prompt::FileFinder { selected, .. } => {
            Some(selected.saturating_sub(first))
        }
        _ => None,
    };
    let row_count = rows.len() as i32;

    let content = Node::Card {
        color: bg_elevated(),
        border: Some(Color32::rgb(0x46, 0x4b, 0x57)),
        radius: 8,
        shadow: true,
        child: Box::new(Node::Padding {
            padding: EdgeInsets::all(8),
            child: Box::new(Node::VStack {
                spacing: 6,
                align: CrossAxisAlignment::Stretch,
                children: vec![
                    Node::LineEdit {
                        text,
                        placeholder: app.prompt.placeholder().to_string(),
                        caret,
                        focused: true,
                        icon: Some(IconKind::Search),
                        suffix: String::new(),
                        invalid: false,
                        on_input: Some(EditorMsg::Prompt),
                    },
                    Node::SizedBox {
                        width: None,
                        height: Some(Length::Px(row_count * PALETTE_ROW_HEIGHT)),
                        child: Box::new(Node::ListView {
                            item_height: PALETTE_ROW_HEIGHT,
                            selected,
                            on_select: Some(EditorMsg::Pick),
                            items: rows,
                        }),
                    },
                ],
            }),
        }),
    };

    Node::Popup {
        x,
        y,
        child: Box::new(Node::SizedBox {
            width: Some(Length::Px(width)),
            height: None,
            child: Box::new(content),
        }),
        on_dismiss: Some(EditorMsg::DismissPrompt),
    }
}

fn palette_row(command: &Command, selected: bool) -> Node<EditorMsg> {
    let color = if selected {
        Color32::rgb(0xdc, 0xe0, 0xe5)
    } else {
        Color32::rgb(0xa9, 0xaf, 0xbc)
    };
    Node::Padding {
        padding: EdgeInsets::new(0, 8, 0, 8),
        child: Box::new(Node::HStack {
            spacing: 8,
            align: CrossAxisAlignment::Center,
            children: vec![
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::StyledLabel {
                        text: command.label().to_string(),
                        color,
                        alignment: TextAlignment::Start,
                    }),
                },
                Node::StyledLabel {
                    text: command.shortcut().unwrap_or("").to_string(),
                    color: Color32::rgb(0x6b, 0x71, 0x7d),
                    alignment: TextAlignment::End,
                },
            ],
        }),
    }
}

fn finder_row(app: &EditorApp, path: &str, selected: bool) -> Node<EditorMsg> {
    let color = if selected {
        Color32::rgb(0xdc, 0xe0, 0xe5)
    } else {
        Color32::rgb(0xa9, 0xaf, 0xbc)
    };
    let root = app.tree.root_path();
    let name = file_name(path);
    let relative = slopos_editor_core::filetree::relative_to(root, path).unwrap_or(path);
    // The directory the name sits in, not the whole relative path: a file at
    // the root of the tree would otherwise have its name printed twice, once
    // in each column.
    let context = relative.strip_suffix(name).unwrap_or(relative);
    let context = context.trim_end_matches('/');
    Node::Padding {
        padding: EdgeInsets::new(0, 8, 0, 8),
        child: Box::new(Node::HStack {
            spacing: 8,
            align: CrossAxisAlignment::Center,
            children: vec![
                Node::StyledLabel {
                    text: name.to_string(),
                    color,
                    alignment: TextAlignment::Start,
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::StyledLabel {
                        text: context.to_string(),
                        color: Color32::rgb(0x6b, 0x71, 0x7d),
                        alignment: TextAlignment::Start,
                    }),
                },
            ],
        }),
    }
}

fn dialog_node(app: &EditorApp, dialog: &Dialog) -> Node<EditorMsg> {
    match dialog {
        Dialog::About => Node::Dialog {
            title: String::from("Sloped"),
            content: Box::new(Node::Label {
                text: String::from(
                    "The SlopOS editor.\n\nTabs, a file tree, syntax highlighting, \
                     find and replace, a command palette — and every edit undoable.\n\n\
                     Ctrl+Shift+P opens the command palette; every command lists its chord.",
                ),
                alignment: TextAlignment::Start,
                wrap: true,
                max_lines: None,
            }),
            actions: vec![Node::Button {
                label: String::from("Close"),
                on_press: Some(EditorMsg::Dialog(0)),
                style: ButtonStyle::Primary,
                enabled: true,
            }],
            on_dismiss: Some(EditorMsg::Dialog(2)),
        },
        Dialog::ConfirmClose { index } => {
            let name = app
                .documents()
                .get(*index)
                .map(|d| d.title().to_string())
                .unwrap_or_default();
            Node::Dialog {
                title: String::from("Unsaved changes"),
                content: Box::new(Node::Label {
                    text: format!("{name} has changes that have not been saved."),
                    alignment: TextAlignment::Start,
                    wrap: true,
                    max_lines: None,
                }),
                actions: vec![
                    Node::Button {
                        label: String::from("Save"),
                        on_press: Some(EditorMsg::Dialog(0)),
                        style: ButtonStyle::Primary,
                        enabled: true,
                    },
                    Node::Button {
                        label: String::from("Discard"),
                        on_press: Some(EditorMsg::Dialog(1)),
                        style: ButtonStyle::Destructive,
                        enabled: true,
                    },
                    Node::Button {
                        label: String::from("Cancel"),
                        on_press: Some(EditorMsg::Dialog(2)),
                        style: ButtonStyle::Secondary,
                        enabled: true,
                    },
                ],
                on_dismiss: Some(EditorMsg::Dialog(2)),
            }
        }
        Dialog::ConfirmQuit => Node::Dialog {
            title: String::from("Unsaved changes"),
            content: Box::new(Node::Label {
                text: String::from("Some buffers have changes that have not been saved."),
                alignment: TextAlignment::Start,
                wrap: true,
                max_lines: None,
            }),
            actions: vec![
                Node::Button {
                    label: String::from("Save all"),
                    on_press: Some(EditorMsg::Dialog(0)),
                    style: ButtonStyle::Primary,
                    enabled: true,
                },
                Node::Button {
                    label: String::from("Quit anyway"),
                    on_press: Some(EditorMsg::Dialog(1)),
                    style: ButtonStyle::Destructive,
                    enabled: true,
                },
                Node::Button {
                    label: String::from("Cancel"),
                    on_press: Some(EditorMsg::Dialog(2)),
                    style: ButtonStyle::Secondary,
                    enabled: true,
                },
            ],
            on_dismiss: Some(EditorMsg::Dialog(2)),
        },
    }
}

fn bg_titlebar() -> Color32 {
    Color32::rgb(0x3b, 0x41, 0x4d)
}

fn bg_sidebar() -> Color32 {
    Color32::rgb(0x2f, 0x34, 0x3e)
}

fn bg_panel() -> Color32 {
    Color32::rgb(0x2f, 0x34, 0x3e)
}

fn bg_elevated() -> Color32 {
    Color32::rgb(0x32, 0x38, 0x43)
}
