//! The editor's view. [`code_viewport`] counts the rows the code surface
//! draws from the same pane sizes the layout places, so the two agree.

use slopos_appkit::theme::space;
use slopos_appkit::widgets::code_view::{CodeLine, StyledSpan};
use slopos_appkit::widgets::editor_tabs::EditorTab;
use slopos_appkit::widgets::tree_view::TreeRow;
use slopos_appkit::{
    Button, ButtonRow, CrossAxisAlignment, EdgeInsets, ICON_BUTTON_SIDE, Icon, IconButton, Length,
    List, ListRow, MenuItem, MenuItemKind, Node, Orientation, SearchField, StatusBar, StyleSheet,
    Surface, Text, Tone, column, row,
};

use slopos_editor_core::document::{Document, file_name};
use slopos_editor_core::search::{self, SearchOptions};

use super::commands::{Command, MENUS, MenuEntry};
use super::prompt::Prompt;
use super::syntax;
use super::{Answer, Dialog, EditorApp, EditorMsg, Focus, Overwrite, said};

pub const DEFAULT_WIDTH: i32 = 1180;
pub const DEFAULT_HEIGHT: i32 = 760;

pub const MIN_CODE_WIDTH: i32 = 320;

const PALETTE_WIDTH: i32 = 620;
const PALETTE_MAX_ROWS: usize = 8;
const PROMPT_PADDING: EdgeInsets = EdgeInsets::symmetric(space::SM, space::XS);

fn prompt_bar_height(style: &StyleSheet) -> i32 {
    style.control_height.max(ICON_BUTTON_SIDE) + PROMPT_PADDING.vertical()
}

fn palette_top(style: &StyleSheet) -> i32 {
    style.menu_bar_height + space::XL + space::LG
}

/// Rows the finder and the palette show in a window `window_height` tall,
/// so the card ends above its foot.
pub fn overlay_rows(window_height: i32) -> usize {
    let style = StyleSheet::current();
    let above = palette_top(&style) + 2 * space::SM + style.control_height + space::SM;
    let room = window_height - above - space::XL;
    // Every finder row carries one line of detail.
    let row = ListRow::height(1);
    (room.max(0) / row).clamp(1, PALETTE_MAX_ROWS as i32) as usize
}

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

    let style = StyleSheet::current();
    let mut height = window_height - style.menu_bar_height - style.tab_height - StatusBar::HEIGHT;
    if prompt_bar {
        height -= prompt_bar_height(&style);
    }
    let lines = slopos_appkit::visible_line_count(height.max(0), cell_h);

    let mut width = window_width;
    if sidebar_visible {
        width -= sidebar_width + style.divider_width;
    }
    let gutter = slopos_appkit::gutter_width(line_count.unwrap_or(1), cell_w, show_line_numbers);
    let cols = ((width - gutter - slopos_appkit::text_area_reserved()).max(0) / cell_w) as usize;
    (lines, cols.max(1))
}

pub fn tree_rows_visible(window_height: i32) -> usize {
    let style = StyleSheet::current();
    let height = window_height - style.menu_bar_height - StatusBar::HEIGHT;
    (height.max(0) / style.row_height.max(1)) as usize
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
    column(0, vec![menu_bar(app), body(app), status_bar(app)])
}

fn menu_bar(app: &EditorApp) -> Node<EditorMsg> {
    let titles: Vec<String> = MENUS.iter().map(|m| m.title.to_string()).collect();

    Node::Background {
        surface: Surface::Chrome,
        child: Box::new(row(
            0,
            vec![
                Node::MenuBar {
                    titles,
                    open: app.menu_open.map(|(index, _, _)| index),
                    on_input: Some(EditorMsg::MenuBar),
                },
                Node::Expand {
                    weight: 1,
                    child: Box::new(Node::Empty),
                },
            ],
        )),
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
    let visible = tree_rows_visible(app.window_height);
    let rows = visible_tree_rows(app, visible);

    Node::Background {
        surface: Surface::Sunken,
        child: Box::new(Node::TreeView {
            rows,
            first_row: app.tree_scroll,
            total_rows: app.tree_row_count(),
            selected: app.tree_selected,
            focused: app.focus == Focus::Tree,
            scroll_dragging: app.tree_scroll_dragging,
            on_input: Some(EditorMsg::Tree),
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
        Node::EditorTabs {
            tabs,
            active: app.active_index(),
            on_input: Some(EditorMsg::Tab),
        },
        Node::Expand {
            weight: 1,
            child: Box::new(code_view(app)),
        },
    ];

    if app.prompt.is_open() && !app.prompt.is_overlay() {
        children.push(Node::SizedBox {
            width: None,
            height: Some(Length::Px(prompt_bar_height(&StyleSheet::current()))),
            child: Box::new(prompt_bar(app)),
        });
    }

    column(0, children)
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
        scroll_dragging: app.code_scroll_dragging,
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
            color: syntax::token_color(span.kind),
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
                child: Box::new(
                    SearchField::new(query.text.clone(), query.caret)
                        .focused(app.focus == Focus::Prompt && !*on_replace)
                        .placeholder("Find")
                        .count(suffix)
                        .invalid(!query.text.is_empty() && *total == 0)
                        .tab_stop(false)
                        .on_input(EditorMsg::Prompt)
                        .into(),
                ),
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
                        tab_stop: false,
                        on_input: Some(EditorMsg::PromptReplace),
                    }),
                });
                fields.push(
                    Button::new("Replace all")
                        .on_press(EditorMsg::Run(Command::ReplaceAll))
                        .into(),
                );
            }
            fields.push(
                IconButton::new(Icon::ChevronUp, "Previous match")
                    .on_press(EditorMsg::Run(Command::FindPrev))
                    .into(),
            );
            fields.push(
                IconButton::new(Icon::ChevronDown, "Next match")
                    .on_press(EditorMsg::Run(Command::FindNext))
                    .into(),
            );
        }
        other => {
            let (text, caret) = other.input_text().unwrap_or_default();
            fields.push(
                Text::new(format!("{}:", other.title()))
                    .tone(Tone::Secondary)
                    .into(),
            );
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
                    tab_stop: false,
                    on_input: Some(EditorMsg::Prompt),
                }),
            });
        }
    }

    Node::Background {
        surface: Surface::Raised,
        child: Box::new(Node::Padding {
            padding: PROMPT_PADDING,
            child: Box::new(row(space::SM, fields)),
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

    let (left, tone) = if app.status.is_empty() {
        (
            doc.path().unwrap_or(doc.title()).to_string(),
            Tone::Secondary,
        )
    } else {
        (app.status.clone(), app.status_tone)
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

    StatusBar::new(left).tone(tone).end(right).into()
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
    let (text, caret) = app.prompt.input_text().unwrap_or_default();
    let width = PALETTE_WIDTH.min(app.window_width - 80).max(240);
    let x = (app.window_width - width) / 2;

    // The rows around the selection, so Enter never runs one not shown.
    let (first, shown) = (app.overlay_first_row(), app.overlay_rows());
    let (rows, selected): (Vec<ListRow>, usize) = match &app.prompt {
        Prompt::Palette {
            results, selected, ..
        } => (
            results
                .iter()
                .skip(first)
                .take(shown)
                .map(palette_row)
                .collect(),
            *selected,
        ),
        Prompt::FileFinder {
            results, selected, ..
        } => (
            results
                .iter()
                .skip(first)
                .take(shown)
                .map(|path| finder_row(app, path))
                .collect(),
            *selected,
        ),
        _ => (Vec::new(), 0),
    };

    let content = Node::Card {
        surface: Surface::Raised,
        shadow: true,
        child: Box::new(Node::Padding {
            padding: EdgeInsets::all(space::SM),
            child: Box::new(column(
                space::SM,
                vec![
                    SearchField::new(text, caret)
                        .focused(true)
                        .placeholder(app.prompt.placeholder())
                        .tab_stop(false)
                        .on_input(EditorMsg::Prompt)
                        .into(),
                    List::new(rows)
                        .selected(Some(selected.saturating_sub(first)))
                        .active(true)
                        .on_activate(EditorMsg::Pick)
                        .into(),
                ],
            )),
        }),
    };

    Node::Popup {
        x,
        y: palette_top(&StyleSheet::current()),
        child: Box::new(Node::SizedBox {
            width: Some(Length::Px(width)),
            height: None,
            child: Box::new(content),
        }),
        on_dismiss: Some(EditorMsg::DismissPrompt),
    }
}

fn palette_row(command: &Command) -> ListRow {
    ListRow::new(command.label()).trailing(command.shortcut().unwrap_or_default())
}

/// The file's name over its folder, the root's own name at the root, on one
/// line so every row is as tall as [`overlay_rows`] counts.
fn finder_row(app: &EditorApp, path: &str) -> ListRow {
    let name = file_name(path);
    let root = app.tree.root_path();
    let relative = slopos_editor_core::filetree::relative_to(root, path).unwrap_or(path);
    let folder = match relative.strip_suffix(name).map(|f| f.trim_end_matches('/')) {
        Some("") | None => file_name(root),
        Some(folder) => folder,
    };
    ListRow::new(name).detail_tail(folder)
}

fn dialog_node(app: &EditorApp, dialog: &Dialog) -> Node<EditorMsg> {
    let question = match dialog {
        Dialog::About => slopos_appkit::Dialog::new(said::NAME)
            .content(
                Text::new(format!(
                    "The SlopOS text editor: tabs, a file tree, syntax highlighting, \
                     find and replace, a command palette, and every edit undoable.\n\n\
                     {} ({}) lists every command with its keys.",
                    said::choose(Command::CommandPalette),
                    Command::CommandPalette.shortcut().unwrap_or_default()
                ))
                .wrap(),
            )
            .footer(
                ButtonRow::new()
                    .confirm(Button::new("Close").on_press(EditorMsg::Dialog(Answer::Cancel))),
            ),
        Dialog::ConfirmClose { index } => {
            let name = app
                .documents()
                .get(*index)
                .map(|d| d.title().to_string())
                .unwrap_or_default();
            slopos_appkit::Dialog::new(format!("Save the changes to {name}?"))
                .content(Text::new("If you close it without saving, the changes are lost.").wrap())
                .footer(
                    ButtonRow::new()
                        .aside(
                            Button::new("Don't save")
                                .on_press(EditorMsg::Dialog(Answer::Discard))
                                .destructive(),
                        )
                        .cancel(Button::new("Cancel").on_press(EditorMsg::Dialog(Answer::Cancel)))
                        .confirm(Button::new("Save").on_press(EditorMsg::Dialog(Answer::Save))),
                )
        }
        Dialog::ConfirmOverwrite { path, found, .. } => {
            let (title, body) = said::overwrite(path, *found);
            let cancel = Button::new("Cancel").on_press(EditorMsg::Dialog(Answer::Cancel));
            let save = |label| Button::new(label).on_press(EditorMsg::Dialog(Answer::Save));
            slopos_appkit::Dialog::new(title)
                .content(Text::new(body).wrap())
                .footer(match found {
                    Overwrite::Gone => ButtonRow::new().cancel(cancel).confirm(save("Save")),
                    _ => ButtonRow::new().cancel(cancel).destructive(save("Replace")),
                })
        }
        Dialog::ConfirmQuit => slopos_appkit::Dialog::new("Save your changes before quitting?")
            .content(
                Text::new("Some files have changes that aren't saved. If you quit without saving, they're lost.")
                    .wrap(),
            )
            .footer(
                ButtonRow::new()
                    .aside(
                        Button::new("Quit without saving")
                            .on_press(EditorMsg::Dialog(Answer::Discard))
                            .destructive(),
                    )
                    .cancel(Button::new("Cancel").on_press(EditorMsg::Dialog(Answer::Cancel)))
                    .confirm(Button::new("Save all").on_press(EditorMsg::Dialog(Answer::Save))),
            ),
    };
    question
        .on_dismiss(EditorMsg::Dialog(Answer::Cancel))
        .into()
}
