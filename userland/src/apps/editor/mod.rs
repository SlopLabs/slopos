//! Sloped — the SlopOS editor.
//!
//! A tab per open file, a file tree beside them, a menu bar above and a status
//! bar below: the shape every editor has had since the nineties, drawn with
//! appkit's code surface and tree, and backed by `editor-core` for everything
//! that is not pixels.
//!
//! The division of labour is deliberate and total. `editor-core` owns the
//! buffer, the cursor, the undo history, the search and the lexer, and is
//! tested on the host. `appkit` owns the rendering and the hit testing. This
//! module owns the filesystem, the clipboard, the keymap and the state machine
//! that ties them together — which is why it is the only part of the editor that
//! cannot be tested without a machine to run on.

mod commands;
mod files;
mod prompt;
mod theme;
mod view;

use slopos_appkit::widgets::code_view::CodeInput;
use slopos_appkit::widgets::drag_handle::DragInput;
use slopos_appkit::widgets::editor_tabs::TabInput;
use slopos_appkit::widgets::line_edit::LineEditInput;
use slopos_appkit::widgets::menu_bar::MenuBarInput;
use slopos_appkit::widgets::tree_view::TreeInput;
use slopos_appkit::{Action, App, Key, Modifiers, NamedKey, Node};

use slopos_editor_core::buffer::Position;
use slopos_editor_core::cursor::Motion;
use slopos_editor_core::document::{Document, file_name, parent_dir};
use slopos_editor_core::filetree::FileTree;
use slopos_editor_core::search::{self, SearchOptions};

pub use commands::Command;
use commands::{PALETTE_COMMANDS, menu_command};
use prompt::{LineInput, Prompt};

/// Rows of context kept between the caret and the edge of the viewport.
const SCROLL_MARGIN: usize = 3;
const MIN_SIDEBAR_WIDTH: i32 = 140;
const MAX_SIDEBAR_WIDTH: i32 = 520;
const DEFAULT_SIDEBAR_WIDTH: i32 = 240;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Focus {
    Editor,
    Tree,
    Prompt,
}

#[derive(Clone, Debug)]
pub enum Dialog {
    /// A tab with unsaved changes is being closed.
    ConfirmClose {
        index: usize,
    },
    /// The application is being closed with unsaved changes somewhere.
    ConfirmQuit,
    About,
}

#[derive(Clone, Debug)]
pub enum EditorMsg {
    Code(CodeInput),
    Tree(TreeInput),
    Tab(TabInput),
    MenuBar(MenuBarInput),
    MenuItem(usize),
    DismissMenu,
    /// A click outside the file finder or the command palette.
    DismissPrompt,
    Prompt(LineEditInput),
    /// The find bar's replacement field.
    PromptReplace(LineEditInput),
    SidebarDrag(DragInput),
    Run(Command),
    /// A row clicked in the file finder or the palette.
    Pick(usize),
    Dialog(usize),
}

pub struct EditorApp {
    docs: Vec<Document>,
    active: usize,
    tree: FileTree,
    tree_selected: Option<usize>,
    tree_scroll: usize,
    focus: Focus,
    prompt: Prompt,
    /// `(menu index, anchor x, anchor y)` while a menu is open.
    menu_open: Option<(usize, i32, i32)>,
    dialog: Option<Dialog>,
    sidebar_width: i32,
    sidebar_visible: bool,
    show_line_numbers: bool,
    window_width: i32,
    window_height: i32,
    status: String,
    /// Used when the compositor has no selection to give — a copy still has to
    /// come back on paste.
    local_clipboard: String,
    untitled_count: usize,
    search_options: SearchOptions,
    /// Rows the code surface can show, from the last layout.
    visible_lines: usize,
    visible_cols: usize,
    /// A selection drag is live in the code surface. Held here because the
    /// widget tree is rebuilt between the press and the moves that follow it.
    selecting: bool,
    /// The sidebar splitter is being dragged, for the same reason.
    sidebar_dragging: bool,
    /// Where the last click landed and how many have landed there, so the
    /// second and third select a word and a line.
    click_run: Option<(usize, usize, u8)>,
}

impl EditorApp {
    pub fn new(args: &[String]) -> Self {
        let arg = args.first().map(String::as_str);
        let root = files::start_directory(arg);
        let mut app = Self {
            docs: Vec::new(),
            active: 0,
            tree: FileTree::new(&root),
            tree_selected: None,
            tree_scroll: 0,
            focus: Focus::Editor,
            prompt: Prompt::None,
            menu_open: None,
            dialog: None,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_visible: true,
            show_line_numbers: true,
            window_width: view::DEFAULT_WIDTH,
            window_height: view::DEFAULT_HEIGHT,
            status: String::new(),
            local_clipboard: String::new(),
            untitled_count: 0,
            search_options: SearchOptions::default(),
            visible_lines: 24,
            visible_cols: 80,
            selecting: false,
            sidebar_dragging: false,
            click_run: None,
        };
        app.sync_tree();

        match arg {
            Some(path) if !files::is_dir(path) => {
                let absolute = files::absolutize(&files::start_directory(None), path);
                app.open_path(&absolute);
            }
            _ => {}
        }
        if app.docs.is_empty() {
            app.new_document();
        }
        app.sync_viewport();
        app
    }

    // ── accessors the view reads ────────────────────────────────────────────

    pub fn documents(&self) -> &[Document] {
        &self.docs
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub(super) fn line_numbers_visible(&self) -> bool {
        self.show_line_numbers
    }

    pub(super) fn search_options(&self) -> SearchOptions {
        self.search_options
    }

    /// The find query, when the find bar is open and non-empty — what the code
    /// surface highlights.
    pub(super) fn active_search_query(&self) -> Option<String> {
        self.find_query()
    }

    /// First result row the overlay list shows, scrolled to keep the selection
    /// inside the window it can draw.
    pub(super) fn overlay_first_row(&self) -> usize {
        let selected = match &self.prompt {
            Prompt::Palette { selected, .. } | Prompt::FileFinder { selected, .. } => *selected,
            _ => return 0,
        };
        selected.saturating_sub(view::PALETTE_MAX_ROWS.saturating_sub(1))
    }

    /// A result row the overlay clicked, in the result list's own indexing.
    fn overlay_pick(&mut self, row: usize) {
        let index = self.overlay_first_row() + row;
        match &mut self.prompt {
            Prompt::FileFinder {
                results, selected, ..
            } => {
                if index < results.len() {
                    *selected = index;
                }
            }
            Prompt::Palette {
                results, selected, ..
            } => {
                if index < results.len() {
                    *selected = index;
                }
            }
            _ => {}
        }
        self.accept_prompt();
    }

    /// The status bar's current message, if any.
    pub fn status(&self) -> &str {
        &self.status
    }

    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    pub fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
    }

    /// `(total matches, index of the current one)` while the find bar is open.
    pub fn find_progress(&self) -> Option<(usize, usize)> {
        match &self.prompt {
            Prompt::Find { total, current, .. } => Some((*total, *current)),
            _ => None,
        }
    }

    /// Opens `path` as if the tree or the open prompt had asked for it.
    pub fn open(&mut self, path: &str) {
        self.open_path(path);
    }

    /// Empties whichever prompt is open, as Ctrl+U does.
    pub fn clear_prompt(&mut self) {
        if let Some(input) = self.prompt.input_mut() {
            input.clear();
        }
    }

    /// Types `text` into whichever prompt is open, one character at a time —
    /// the same path a keystroke takes.
    pub fn type_in_prompt(&mut self, text: &str) {
        for character in text.chars() {
            self.handle_prompt_input(LineEditInput::Text { character }, false);
        }
    }

    pub(super) fn tree_row_count(&self) -> usize {
        self.tree.rows().len()
    }

    /// `count` visible tree rows from `first`, as the sidebar draws them.
    pub fn tree_rows(
        &self,
        first: usize,
        count: usize,
    ) -> Vec<(String, usize, bool, bool, String)> {
        self.tree
            .rows()
            .iter()
            .skip(first)
            .take(count)
            .filter_map(|row| {
                let node = self.tree.node(row.node)?;
                Some((
                    node.name.clone(),
                    row.depth,
                    node.is_dir,
                    node.expanded,
                    node.path.clone(),
                ))
            })
            .collect()
    }

    // ── documents ───────────────────────────────────────────────────────────

    fn doc(&self) -> &Document {
        &self.docs[self.active]
    }

    fn doc_mut(&mut self) -> &mut Document {
        let index = self.active;
        &mut self.docs[index]
    }

    fn new_document(&mut self) {
        self.untitled_count += 1;
        let title = format!("untitled-{}", self.untitled_count);
        self.docs.push(Document::empty(title));
        self.active = self.docs.len() - 1;
        self.focus = Focus::Editor;
    }

    /// Opens `path`, or switches to it when it is already open.
    fn open_path(&mut self, path: &str) {
        if let Some(index) = self
            .docs
            .iter()
            .position(|d| d.path().is_some_and(|p| p == path))
        {
            self.active = index;
            self.focus = Focus::Editor;
            self.status = format!("{path}");
            return;
        }

        match files::read_file(path) {
            Ok(text) => match Document::from_text(Some(path.to_string()), &text) {
                Ok(doc) => {
                    // An untouched, unnamed, empty buffer is scaffolding, not
                    // work: opening a file replaces it rather than stacking a
                    // tab nobody asked for.
                    let replace_scratch = self.docs.len() == 1
                        && self.docs[0].path().is_none()
                        && !self.docs[0].is_modified()
                        && self.docs[0].buffer.is_empty();
                    if replace_scratch {
                        self.docs.clear();
                    }
                    self.docs.push(doc);
                    self.active = self.docs.len() - 1;
                    self.focus = Focus::Editor;
                    self.status = format!("Opened {path}");
                    self.reveal_in_tree(path);
                }
                Err(_) => {
                    self.status = format!("{path}: too many lines to open");
                }
            },
            Err(message) => self.status = message,
        }
        self.sync_viewport();
    }

    fn save_document(&mut self, index: usize, path: Option<String>) {
        let Some(doc) = self.docs.get(index) else {
            self.status = String::from("Nothing to save: that tab is gone");
            return;
        };
        let target = match path.or_else(|| doc.path().map(str::to_string)) {
            Some(p) => p,
            None => {
                let seed = self.default_save_path(index);
                self.prompt = Prompt::SaveAs {
                    input: LineInput::with_text(seed),
                    index,
                };
                self.focus = Focus::Prompt;
                self.menu_open = None;
                self.sync_viewport();
                return;
            }
        };
        let text = doc.text();
        match files::write_file(&target, &text) {
            Ok(()) => {
                let named = self.docs[index].path() != Some(target.as_str());
                self.docs[index].mark_saved(Some(target.clone()));
                self.status = format!("Saved {target}");
                if named {
                    self.refresh_tree_dir(parent_dir(&target));
                    self.reveal_in_tree(&target);
                }
            }
            Err(message) => self.status = message,
        }
    }

    /// Where `index`'s Save As prompt starts: the tree's root and that
    /// document's own title.
    fn default_save_path(&self, index: usize) -> String {
        let mut path = String::from(self.tree.root_path().trim_end_matches('/'));
        path.push('/');
        path.push_str(
            self.docs
                .get(index)
                .map(Document::title)
                .unwrap_or("untitled"),
        );
        path
    }

    /// Closes tab `index`, asking first when it holds unsaved changes.
    fn close_tab(&mut self, index: usize, force: bool) {
        let Some(doc) = self.docs.get(index) else {
            return;
        };
        if doc.is_modified() && !force {
            self.dialog = Some(Dialog::ConfirmClose { index });
            return;
        }
        self.docs.remove(index);
        self.retarget_document_index(index);
        if self.docs.is_empty() {
            self.new_document();
        } else if self.active >= self.docs.len() {
            self.active = self.docs.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        self.sync_viewport();
    }

    /// Follows the document a pending Save As prompt or close dialog names
    /// across the removal of tab `closed`. A tab position is not a stable
    /// identity: Ctrl+W reaches `close_tab` while the Save As field has focus,
    /// and without this the prompt would go on to write a different document's
    /// text to the path the user typed for this one.
    fn retarget_document_index(&mut self, closed: usize) {
        if let Prompt::SaveAs { index, .. } = &mut self.prompt {
            match (*index).cmp(&closed) {
                core::cmp::Ordering::Equal => {
                    self.prompt = Prompt::None;
                    self.focus = Focus::Editor;
                    self.status = String::from("Save As cancelled: that tab was closed");
                }
                core::cmp::Ordering::Greater => *index -= 1,
                core::cmp::Ordering::Less => {}
            }
        }
        if let Some(Dialog::ConfirmClose { index }) = &mut self.dialog {
            match (*index).cmp(&closed) {
                core::cmp::Ordering::Equal => self.dialog = None,
                core::cmp::Ordering::Greater => *index -= 1,
                core::cmp::Ordering::Less => {}
            }
        }
    }

    fn any_modified(&self) -> bool {
        self.docs.iter().any(Document::is_modified)
    }

    // ── tree ────────────────────────────────────────────────────────────────

    /// Reads whatever directories the tree is waiting on.
    fn sync_tree(&mut self) {
        for _ in 0..64 {
            let pending = self.tree.pending();
            if pending.is_empty() {
                break;
            }
            for index in pending {
                let Some(node) = self.tree.node(index) else {
                    continue;
                };
                let path = node.path.clone();
                match files::read_dir(&path) {
                    Ok(entries) => self.tree.populate(index, entries),
                    Err(message) => {
                        // A directory that cannot be read is an empty one here,
                        // with the reason on the status bar; the alternative is
                        // a sidebar that refuses to draw.
                        self.status = message;
                        self.tree.populate(index, Vec::new());
                    }
                }
            }
        }
    }

    fn refresh_tree_dir(&mut self, path: &str) {
        if let Some(index) = self.tree.find_path(path) {
            self.tree.invalidate(index);
            self.sync_tree();
        }
    }

    /// Expands the tree down to `path` and selects it, when it is under the root.
    fn reveal_in_tree(&mut self, path: &str) {
        let root = self.tree.root_path().to_string();
        let Some(relative) =
            slopos_editor_core::filetree::relative_to(&root, path).map(str::to_string)
        else {
            return;
        };
        // Expand each ancestor in turn, reading it as we go: a path deeper than
        // what has been read is not in the tree yet.
        let mut current = root;
        for part in relative.split('/') {
            if part.is_empty() {
                continue;
            }
            current = slopos_editor_core::filetree::join_path(&current, part);
            if let Some(index) = self.tree.find_path(&current) {
                if self.tree.node(index).is_some_and(|n| n.is_dir) {
                    self.tree.set_expanded(index, true);
                    self.sync_tree();
                }
            }
        }
        if let Some(index) = self.tree.find_path(path) {
            self.tree.reveal(index);
            self.sync_tree();
            if let Some(row) = self.tree.row_of(index) {
                self.tree_selected = Some(row);
                self.scroll_tree_to(row);
            }
        }
    }

    fn open_folder(&mut self, path: &str) {
        if !files::is_dir(path) {
            self.status = format!("{path} is not a folder");
            return;
        }
        self.tree = FileTree::new(path);
        self.tree_selected = None;
        self.tree_scroll = 0;
        self.sync_tree();
        self.status = format!("Folder: {path}");
    }

    fn tree_rows_visible(&self) -> usize {
        view::tree_rows_visible(self.window_height)
    }

    fn scroll_tree_to(&mut self, row: usize) {
        let height = self.tree_rows_visible().max(1);
        if row < self.tree_scroll {
            self.tree_scroll = row;
        } else if row >= self.tree_scroll + height {
            self.tree_scroll = row + 1 - height;
        }
    }

    // ── viewport ────────────────────────────────────────────────────────────

    /// Recomputes how much of a document is on screen, then keeps the caret in
    /// it. Called after anything that can move the caret or resize the window.
    fn sync_viewport(&mut self) {
        let (lines, cols) = view::code_viewport(
            self.window_width,
            self.window_height,
            self.sidebar_visible,
            self.sidebar_width,
            self.prompt_bar_visible(),
            self.docs.get(self.active).map(|d| d.buffer.line_count()),
            self.show_line_numbers,
        );
        self.visible_lines = lines;
        self.visible_cols = cols;
        let doc = self.doc_mut();
        doc.viewport.visible_lines = lines;
        doc.viewport.visible_cols = cols;
        doc.scroll_to_cursor(SCROLL_MARGIN);
    }

    fn prompt_bar_visible(&self) -> bool {
        self.prompt.is_open() && !self.prompt.is_overlay()
    }

    // ── clipboard ───────────────────────────────────────────────────────────

    fn copy_selection(&mut self, cut: bool) {
        let Some(text) = self.doc().selected_text() else {
            return;
        };
        self.local_clipboard = text.clone();
        slopos_appkit::clipboard::copy(&text);
        if cut {
            self.doc_mut().insert_text("");
            // `insert_text("")` deletes the selection and inserts nothing, which
            // is exactly a cut; the empty insert is skipped by the buffer.
        }
        self.status = if cut {
            String::from("Cut")
        } else {
            String::from("Copied")
        };
    }

    fn paste(&mut self) {
        // Ask the compositor; its answer arrives at `on_paste`, empty when
        // nothing owns the selection — which is when this editor's own last
        // copy is the best answer available.
        if !slopos_appkit::clipboard::request_paste() {
            self.paste_local();
        }
    }

    fn paste_local(&mut self) {
        if self.local_clipboard.is_empty() {
            self.status = String::from("The clipboard is empty");
            return;
        }
        let text = self.local_clipboard.clone();
        self.insert_text(&text);
    }

    fn insert_text(&mut self, text: &str) {
        if !self.doc_mut().insert_text(text) && !text.is_empty() {
            self.status = String::from("Refused: that would take the file past its line limit");
        }
        self.sync_viewport();
    }

    // ── search ──────────────────────────────────────────────────────────────

    fn find_query(&self) -> Option<String> {
        match &self.prompt {
            Prompt::Find { query, .. } if !query.text.is_empty() => Some(query.text.clone()),
            _ => None,
        }
    }

    fn refresh_find(&mut self) {
        let Some(query) = self.find_query() else {
            if let Prompt::Find { total, current, .. } = &mut self.prompt {
                *total = 0;
                *current = 0;
            }
            return;
        };
        let options = self.search_options;
        let matches = search::find_all(&self.docs[self.active].buffer, &query, options);
        // The *selection's* start, because that is where a walk left the
        // caret: `find_step` selects a match and parks the caret at its end, so
        // reading the caret would always name the match after the one the user
        // is looking at.
        let doc = &self.docs[self.active];
        let anchor = doc
            .cursor
            .selection()
            .map(|s| s.start)
            .unwrap_or(doc.cursor.position);
        let current = matches
            .iter()
            .position(|m| m.start >= anchor)
            .unwrap_or(0)
            .min(matches.len().saturating_sub(1));
        if let Prompt::Find {
            total: t,
            current: c,
            ..
        } = &mut self.prompt
        {
            *t = matches.len();
            *c = if matches.is_empty() { 0 } else { current + 1 };
        }
    }

    fn find_step(&mut self, forward: bool) {
        if self.find_query().is_none() {
            self.status = String::from("Nothing to find");
            return;
        }
        let doc = &self.docs[self.active];
        // Both directions start from the current match's *start*: forward one
        // past it so Enter advances, backward at it so the search does not find
        // the match it is standing on again.
        let anchor = doc
            .cursor
            .selection()
            .map(|s| s.start)
            .unwrap_or(doc.cursor.position);
        let from = if forward {
            doc.buffer.next_position(anchor)
        } else {
            anchor
        };
        self.find_from(forward, from);
    }

    /// Selects the first match at or after (or, backward, at or before) `from`.
    fn find_from(&mut self, forward: bool, from: Position) {
        let Some(query) = self.find_query() else {
            self.status = String::from("Nothing to find");
            return;
        };
        let options = self.search_options;
        let doc = &self.docs[self.active];
        let hit = if forward {
            search::find_next(&doc.buffer, from, &query, options)
        } else {
            search::find_prev(&doc.buffer, from, &query, options)
        };
        match hit {
            Some(range) => {
                let doc = self.doc_mut();
                doc.cursor.anchor = Some(range.start);
                doc.cursor.position = range.end;
                doc.cursor.goal_col = None;
                self.sync_viewport();
                self.refresh_find();
            }
            None => self.status = format!("No match for \"{query}\""),
        }
    }

    fn replace_current(&mut self) {
        let (Some(query), Some(replacement)) = (self.find_query(), self.replace_text()) else {
            return;
        };
        let options = self.search_options;
        let selected = self.doc().selected_text();
        let matches_selection = selected.as_deref().is_some_and(|s| {
            if options.case_sensitive {
                s == query
            } else {
                s.eq_ignore_ascii_case(&query)
            }
        });
        if matches_selection {
            if let Some(range) = self.doc().cursor.selection() {
                self.doc_mut().replace_range(range, &replacement);
                // From exactly the end of the replacement, not one past it: the
                // replacement is not a match, so `find_step`'s "advance past
                // the match you are standing on" would skip whatever begins
                // immediately after it.
                let from = self.doc().cursor.position;
                self.find_from(true, from);
                return;
            }
        }
        self.find_step(true);
    }

    fn replace_all(&mut self) {
        let (Some(query), Some(replacement)) = (self.find_query(), self.replace_text()) else {
            self.status = String::from("Open Replace first (Ctrl+H)");
            return;
        };
        let options = self.search_options;
        let count = self.doc_mut().replace_all(&query, &replacement, options);
        self.status = format!("Replaced {count}");
        self.refresh_find();
        self.sync_viewport();
    }

    fn replace_text(&self) -> Option<String> {
        match &self.prompt {
            Prompt::Find { replace, .. } => replace.as_ref().map(|r| r.text.clone()),
            _ => None,
        }
    }

    // ── prompts ─────────────────────────────────────────────────────────────

    fn open_prompt(&mut self, prompt: Prompt) {
        self.prompt = prompt;
        self.focus = Focus::Prompt;
        self.menu_open = None;
        self.sync_viewport();
    }

    fn close_prompt(&mut self) {
        self.prompt = Prompt::None;
        self.focus = Focus::Editor;
        self.sync_viewport();
    }

    fn refresh_palette(&mut self) {
        if let Prompt::Palette {
            input,
            results,
            selected,
        } = &mut self.prompt
        {
            let needle = input.text.clone();
            let mut scored: Vec<(Command, i32)> = PALETTE_COMMANDS
                .iter()
                .filter_map(|c| search::fuzzy_score(c.label(), &needle).map(|s| (*c, s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            *results = scored.into_iter().map(|(c, _)| c).collect();
            *selected = (*selected).min(results.len().saturating_sub(1));
        }
    }

    fn refresh_finder(&mut self) {
        let paths: Vec<String> = self
            .tree
            .file_paths()
            .into_iter()
            .map(str::to_string)
            .collect();
        if let Prompt::FileFinder {
            input,
            results,
            selected,
        } = &mut self.prompt
        {
            let needle = input.text.clone();
            let mut scored: Vec<(String, i32)> = paths
                .into_iter()
                .filter_map(|p| search::fuzzy_score(&p, &needle).map(|s| (p, s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            scored.truncate(200);
            *results = scored.into_iter().map(|(p, _)| p).collect();
            *selected = (*selected).min(results.len().saturating_sub(1));
        }
    }

    /// Enter in whichever prompt is open.
    fn accept_prompt(&mut self) {
        let prompt = std::mem::replace(&mut self.prompt, Prompt::None);
        match prompt {
            Prompt::None => {}
            Prompt::Find { .. } => {
                // Find stays open: Enter walks matches.
                self.prompt = prompt;
                self.find_step(true);
                return;
            }
            Prompt::Goto { input } => {
                match input.text.trim().parse::<usize>() {
                    Ok(line) if line > 0 => {
                        self.doc_mut().goto_line(line);
                        self.sync_viewport();
                    }
                    _ => self.status = format!("Not a line number: {}", input.text),
                }
                self.focus = Focus::Editor;
            }
            Prompt::OpenPath { input } => {
                let path = files::absolutize(self.tree.root_path(), input.text.trim());
                self.focus = Focus::Editor;
                if files::is_dir(&path) {
                    self.open_folder(&path);
                } else {
                    self.open_path(&path);
                }
            }
            Prompt::OpenFolder { input } => {
                let path = files::absolutize(self.tree.root_path(), input.text.trim());
                self.focus = Focus::Editor;
                self.open_folder(&path);
            }
            Prompt::SaveAs { input, index } => {
                let path = files::absolutize(self.tree.root_path(), input.text.trim());
                self.focus = Focus::Editor;
                self.save_document(index, Some(path));
            }
            Prompt::FileFinder {
                results, selected, ..
            } => {
                self.focus = Focus::Editor;
                if let Some(path) = results.get(selected) {
                    let path = path.clone();
                    self.open_path(&path);
                }
            }
            Prompt::Palette {
                results, selected, ..
            } => {
                self.focus = Focus::Editor;
                if let Some(command) = results.get(selected).copied() {
                    self.run(command);
                }
            }
        }
        self.sync_viewport();
    }

    fn move_prompt_selection(&mut self, delta: isize) {
        match &mut self.prompt {
            Prompt::FileFinder {
                results, selected, ..
            } => {
                if results.is_empty() {
                    return;
                }
                let last = results.len() - 1;
                *selected = (*selected as isize + delta).clamp(0, last as isize) as usize;
            }
            Prompt::Palette {
                results, selected, ..
            } => {
                if results.is_empty() {
                    return;
                }
                let last = results.len() - 1;
                *selected = (*selected as isize + delta).clamp(0, last as isize) as usize;
            }
            _ => {}
        }
    }

    // ── commands ────────────────────────────────────────────────────────────

    fn run(&mut self, command: Command) {
        self.menu_open = None;
        match command {
            Command::NewFile => {
                self.new_document();
                self.sync_viewport();
            }
            Command::OpenFile => self.open_prompt(Prompt::OpenPath {
                input: LineInput::with_text(format!("{}/", self.tree.root_path())),
            }),
            Command::OpenFolder => self.open_prompt(Prompt::OpenFolder {
                input: LineInput::with_text(self.tree.root_path().to_string()),
            }),
            Command::Save => {
                let index = self.active;
                self.save_document(index, None);
            }
            Command::SaveAs => {
                let index = self.active;
                self.open_prompt(Prompt::SaveAs {
                    input: LineInput::with_text(self.default_save_path(index)),
                    index,
                })
            }
            Command::CloseTab => {
                let index = self.active;
                self.close_tab(index, false);
            }
            Command::Quit => {
                if self.any_modified() {
                    self.dialog = Some(Dialog::ConfirmQuit);
                } else {
                    std::process::exit(0);
                }
            }

            Command::Undo => {
                if !self.doc_mut().undo() {
                    self.status = String::from("Nothing to undo");
                }
                self.sync_viewport();
            }
            Command::Redo => {
                if !self.doc_mut().redo() {
                    self.status = String::from("Nothing to redo");
                }
                self.sync_viewport();
            }
            Command::Cut => self.copy_selection(true),
            Command::Copy => self.copy_selection(false),
            Command::Paste => self.paste(),
            Command::SelectAll => self.doc_mut().select_all(),
            Command::SelectLine => {
                let line = self.doc().cursor.position.line;
                self.doc_mut().select_line_at(line);
            }
            Command::DuplicateLine => {
                self.doc_mut().duplicate_line();
                self.sync_viewport();
            }
            Command::DeleteLine => {
                self.doc_mut().delete_line();
                self.sync_viewport();
            }
            Command::MoveLineUp => {
                self.doc_mut().move_lines(false);
                self.sync_viewport();
            }
            Command::MoveLineDown => {
                self.doc_mut().move_lines(true);
                self.sync_viewport();
            }
            Command::ToggleComment => {
                self.doc_mut().toggle_comment();
                self.sync_viewport();
            }
            Command::Indent => {
                self.doc_mut().indent_selection();
                self.sync_viewport();
            }
            Command::Outdent => {
                self.doc_mut().outdent();
                self.sync_viewport();
            }

            Command::Find | Command::Replace => {
                let seed = self
                    .doc()
                    .selected_text()
                    .filter(|s| !s.contains('\n'))
                    .unwrap_or_else(|| match &self.prompt {
                        Prompt::Find { query, .. } => query.text.clone(),
                        _ => String::new(),
                    });
                let replace = if command == Command::Replace {
                    Some(match &self.prompt {
                        Prompt::Find {
                            replace: Some(r), ..
                        } => r.clone(),
                        _ => LineInput::new(),
                    })
                } else {
                    match &self.prompt {
                        Prompt::Find { replace, .. } => replace.clone(),
                        _ => None,
                    }
                };
                // Both Find and Replace land in the query field: what you
                // reach for first is what you are searching for.
                self.open_prompt(Prompt::Find {
                    query: LineInput::with_text(seed),
                    replace,
                    on_replace: false,
                    total: 0,
                    current: 0,
                });
                self.refresh_find();
            }
            Command::FindNext => self.find_step(true),
            Command::FindPrev => self.find_step(false),
            Command::ReplaceAll => self.replace_all(),
            Command::GotoLine => self.open_prompt(Prompt::Goto {
                input: LineInput::new(),
            }),
            Command::FileFinder => {
                self.open_prompt(Prompt::FileFinder {
                    input: LineInput::new(),
                    results: Vec::new(),
                    selected: 0,
                });
                self.refresh_finder();
            }
            Command::CommandPalette => {
                self.open_prompt(Prompt::Palette {
                    input: LineInput::new(),
                    results: Vec::new(),
                    selected: 0,
                });
                self.refresh_palette();
            }

            Command::NextTab => {
                if !self.docs.is_empty() {
                    self.active = (self.active + 1) % self.docs.len();
                    self.sync_viewport();
                }
            }
            Command::PrevTab => {
                if !self.docs.is_empty() {
                    self.active = (self.active + self.docs.len() - 1) % self.docs.len();
                    self.sync_viewport();
                }
            }
            Command::ToggleSidebar => {
                self.sidebar_visible = !self.sidebar_visible;
                if !self.sidebar_visible && self.focus == Focus::Tree {
                    self.focus = Focus::Editor;
                }
                self.sync_viewport();
            }
            Command::ToggleLineNumbers => {
                self.show_line_numbers = !self.show_line_numbers;
                self.sync_viewport();
            }
            Command::RevealInSidebar => {
                if let Some(path) = self.doc().path().map(str::to_string) {
                    self.sidebar_visible = true;
                    self.reveal_in_tree(&path);
                    self.sync_viewport();
                } else {
                    self.status = String::from("This buffer has no file yet");
                }
            }
            Command::KeyboardShortcuts => {
                let path = String::from(files::DOC_PATH);
                if files::is_file(&path) {
                    self.open_path(&path);
                } else {
                    self.status = format!("{path} is not installed");
                }
            }
            Command::About => self.dialog = Some(Dialog::About),
        }
    }

    // ── keyboard ────────────────────────────────────────────────────────────

    /// Chords that work wherever the focus is.
    fn global_command(&self, key: Key, mods: Modifiers) -> Option<Command> {
        // Shift folds a control chord's letter to upper case on the way through
        // the keymap, so a chord table written in lower case would miss every
        // shifted one.
        let key = normalize_chord(key);
        if !mods.ctrl {
            return match key {
                Key::Named(NamedKey::F3) if mods.shift => Some(Command::FindPrev),
                Key::Named(NamedKey::F3) => Some(Command::FindNext),
                _ => None,
            };
        }
        let command = match key {
            Key::Char('n') => Command::NewFile,
            Key::Char('o') if mods.shift => Command::OpenFolder,
            Key::Char('o') => Command::OpenFile,
            Key::Char('s') if mods.shift => Command::SaveAs,
            Key::Char('s') => Command::Save,
            Key::Char('w') => Command::CloseTab,
            Key::Char('q') => Command::Quit,
            Key::Char('f') => Command::Find,
            Key::Char('h') => Command::Replace,
            Key::Char('g') => Command::GotoLine,
            Key::Char('p') if mods.shift => Command::CommandPalette,
            Key::Char('p') => Command::FileFinder,
            Key::Char('b') => Command::ToggleSidebar,
            Key::Named(NamedKey::Tab) if mods.shift => Command::PrevTab,
            Key::Named(NamedKey::Tab) => Command::NextTab,
            _ => return None,
        };
        Some(command)
    }

    /// Chords that edit, which only apply while the code surface has focus.
    fn editor_command(&self, key: Key, mods: Modifiers) -> Option<Command> {
        let key = normalize_chord(key);
        if mods.ctrl {
            return Some(match key {
                Key::Char('z') if mods.shift => Command::Redo,
                Key::Char('z') => Command::Undo,
                Key::Char('y') => Command::Redo,
                Key::Char('x') => Command::Cut,
                Key::Char('c') => Command::Copy,
                Key::Char('v') => Command::Paste,
                Key::Char('a') => Command::SelectAll,
                Key::Char('l') => Command::SelectLine,
                Key::Char('d') => Command::DuplicateLine,
                Key::Char('k') => Command::DeleteLine,
                Key::Char('/') => Command::ToggleComment,
                _ => return None,
            });
        }
        if mods.plain_alt() {
            return match key {
                Key::Named(NamedKey::Up) => Some(Command::MoveLineUp),
                Key::Named(NamedKey::Down) => Some(Command::MoveLineDown),
                _ => None,
            };
        }
        None
    }

    fn handle_editor_key(&mut self, key: Key, mods: Modifiers) {
        if let Some(command) = self.editor_command(key, mods) {
            self.run(command);
            return;
        }

        let extend = mods.shift;
        let page = self.visible_lines.max(1).saturating_sub(1);
        let motion = match key {
            Key::Named(NamedKey::Left) if mods.ctrl => Some(Motion::WordLeft),
            Key::Named(NamedKey::Right) if mods.ctrl => Some(Motion::WordRight),
            Key::Named(NamedKey::Left) => Some(Motion::Left),
            Key::Named(NamedKey::Right) => Some(Motion::Right),
            Key::Named(NamedKey::Up) => Some(Motion::Up),
            Key::Named(NamedKey::Down) => Some(Motion::Down),
            Key::Named(NamedKey::Home) if mods.ctrl => Some(Motion::DocStart),
            Key::Named(NamedKey::End) if mods.ctrl => Some(Motion::DocEnd),
            Key::Named(NamedKey::Home) => Some(Motion::LineStart),
            Key::Named(NamedKey::End) => Some(Motion::LineEnd),
            Key::Named(NamedKey::PageUp) => Some(Motion::PageUp(page)),
            Key::Named(NamedKey::PageDown) => Some(Motion::PageDown(page)),
            _ => None,
        };
        if let Some(motion) = motion {
            self.doc_mut().move_cursor(motion, extend);
            self.sync_viewport();
            return;
        }

        match key {
            // Space arrives as a named key, so a focused button can be pressed
            // with it; in the code surface it is a character.
            Key::Named(NamedKey::Space) if !mods.ctrl && !mods.plain_alt() => {
                self.insert_text(" ");
            }
            Key::Named(NamedKey::Enter) => {
                self.doc_mut().insert_newline();
                self.sync_viewport();
            }
            Key::Named(NamedKey::Backspace) if mods.ctrl => {
                self.doc_mut().delete_word_left();
                self.sync_viewport();
            }
            Key::Named(NamedKey::Backspace) => {
                self.doc_mut().backspace();
                self.sync_viewport();
            }
            Key::Named(NamedKey::Delete) if mods.ctrl => {
                self.doc_mut().delete_word_right();
                self.sync_viewport();
            }
            Key::Named(NamedKey::Delete) => {
                self.doc_mut().delete_forward();
                self.sync_viewport();
            }
            Key::Named(NamedKey::Tab) if mods.shift => self.run(Command::Outdent),
            Key::Named(NamedKey::Tab) => self.run(Command::Indent),
            Key::Named(NamedKey::Escape) => {
                self.doc_mut().cursor.clear_selection();
            }
            _ => {}
        }
    }

    fn handle_tree_key(&mut self, key: Key, mods: Modifiers) {
        let rows = self.tree.rows().len();
        if rows == 0 {
            return;
        }
        let selected = self.tree_selected.unwrap_or(0);
        match key {
            Key::Named(NamedKey::Up) => {
                let row = selected.saturating_sub(1);
                self.tree_selected = Some(row);
                self.scroll_tree_to(row);
            }
            Key::Named(NamedKey::Down) => {
                let row = (selected + 1).min(rows - 1);
                self.tree_selected = Some(row);
                self.scroll_tree_to(row);
            }
            Key::Named(NamedKey::Home) => {
                self.tree_selected = Some(0);
                self.tree_scroll = 0;
            }
            Key::Named(NamedKey::End) => {
                self.tree_selected = Some(rows - 1);
                self.scroll_tree_to(rows - 1);
            }
            Key::Named(NamedKey::PageUp) => {
                let page = self.tree_rows_visible().max(1);
                let row = selected.saturating_sub(page);
                self.tree_selected = Some(row);
                self.scroll_tree_to(row);
            }
            Key::Named(NamedKey::PageDown) => {
                let page = self.tree_rows_visible().max(1);
                let row = (selected + page).min(rows - 1);
                self.tree_selected = Some(row);
                self.scroll_tree_to(row);
            }
            Key::Named(NamedKey::Right) => self.set_row_expanded(selected, true),
            Key::Named(NamedKey::Left) => self.set_row_expanded(selected, false),
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                self.activate_tree_row(selected)
            }
            Key::Named(NamedKey::Escape) => self.focus = Focus::Editor,
            Key::Named(NamedKey::Tab) if !mods.ctrl => self.focus = Focus::Editor,
            _ => {}
        }
    }

    fn set_row_expanded(&mut self, row: usize, expanded: bool) {
        let Some(entry) = self.tree.rows().get(row).copied() else {
            return;
        };
        let Some(node) = self.tree.node(entry.node) else {
            return;
        };
        if node.is_dir {
            self.tree.set_expanded(entry.node, expanded);
            self.sync_tree();
        }
    }

    fn activate_tree_row(&mut self, row: usize) {
        let Some(entry) = self.tree.rows().get(row).copied() else {
            return;
        };
        self.tree_selected = Some(row);
        let Some(node) = self.tree.node(entry.node) else {
            return;
        };
        let path = node.path.clone();
        if node.is_dir {
            self.tree.toggle(entry.node);
            self.sync_tree();
        } else {
            self.open_path(&path);
        }
    }

    fn handle_prompt_key(&mut self, key: Key, mods: Modifiers) {
        if let Some(input) = self.prompt.input_mut() {
            if input.handle_key(key, mods) {
                match &self.prompt {
                    Prompt::Find { .. } => self.refresh_find(),
                    Prompt::FileFinder { .. } => self.refresh_finder(),
                    Prompt::Palette { .. } => self.refresh_palette(),
                    _ => {}
                }
                return;
            }
        }

        match key {
            Key::Named(NamedKey::Escape) => self.close_prompt(),
            Key::Named(NamedKey::Enter) if mods.shift => {
                if matches!(self.prompt, Prompt::Find { .. }) {
                    self.find_step(false);
                } else {
                    self.accept_prompt();
                }
            }
            Key::Named(NamedKey::Enter) => {
                let on_replace = matches!(
                    self.prompt,
                    Prompt::Find {
                        on_replace: true,
                        ..
                    }
                );
                if mods.ctrl && matches!(self.prompt, Prompt::Find { .. }) {
                    self.replace_all();
                } else if on_replace {
                    // Enter in the replacement field replaces this match and
                    // moves to the next, which is what makes a replace walk.
                    self.replace_current();
                } else {
                    self.accept_prompt();
                }
            }
            Key::Named(NamedKey::Up) => self.move_prompt_selection(-1),
            Key::Named(NamedKey::Down) => self.move_prompt_selection(1),
            Key::Named(NamedKey::Tab) => match &mut self.prompt {
                Prompt::Find {
                    replace,
                    on_replace,
                    ..
                } => {
                    if replace.is_some() {
                        *on_replace = !*on_replace;
                    }
                }
                _ => self.move_prompt_selection(1),
            },
            _ => {}
        }
    }

    fn handle_dialog_key(&mut self, key: Key) -> bool {
        let Some(dialog) = self.dialog.clone() else {
            return false;
        };
        match key {
            Key::Named(NamedKey::Escape) => {
                self.dialog = None;
                true
            }
            Key::Named(NamedKey::Enter) => {
                match dialog {
                    Dialog::About => self.dialog = None,
                    Dialog::ConfirmClose { .. } | Dialog::ConfirmQuit => self.dialog_action(0),
                }
                true
            }
            _ => false,
        }
    }

    fn dialog_action(&mut self, index: usize) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match dialog {
            Dialog::About => {}
            Dialog::ConfirmClose { index: tab } => match index {
                0 => {
                    self.save_document(tab, None);
                    // Saving can open a Save As prompt instead of writing, and
                    // the tab may have gone in the meantime; only a tab that is
                    // there and clean gets closed.
                    if self.docs.get(tab).is_some_and(|d| !d.is_modified()) {
                        self.close_tab(tab, true);
                    }
                }
                1 => self.close_tab(tab, true),
                _ => {}
            },
            Dialog::ConfirmQuit => match index {
                0 => {
                    for i in 0..self.docs.len() {
                        if self.docs[i].is_modified() && self.docs[i].path().is_some() {
                            self.save_document(i, None);
                        }
                    }
                    if !self.any_modified() {
                        std::process::exit(0);
                    }
                    self.status = String::from("Some buffers have no file yet — save them first");
                }
                1 => std::process::exit(0),
                _ => {}
            },
        }
    }

    // ── widget input ────────────────────────────────────────────────────────

    fn handle_code_input(&mut self, input: CodeInput) {
        // Any interaction with the text supersedes whatever the status bar was
        // reporting, which is what keeps "Saved …" from outliving the save.
        if !matches!(input, CodeInput::Scroll { .. }) {
            self.status.clear();
        }
        match input {
            CodeInput::Click { line, col, extend } => {
                self.focus = Focus::Editor;
                self.menu_open = None;
                self.selecting = true;
                // A click on the cell the last one landed on continues the run.
                // No timer: a slow double click is still two clicks on one cell,
                // and the cost of reading it that way is a word selected rather
                // than a caret placed.
                let clicks = match self.click_run {
                    Some((l, c, n)) if l == line && c == col => (n % 3) + 1,
                    _ => 1,
                };
                self.click_run = Some((line, col, clicks));
                let position = Position::new(line, col);
                match clicks {
                    2 => self.doc_mut().select_word_at(position),
                    3 => self.doc_mut().select_line_at(line),
                    _ => self.doc_mut().place_cursor(position, extend),
                }
                self.sync_viewport();
            }
            CodeInput::Drag { line, col } => {
                // A drag moves the caret and leaves the anchor where the press
                // put it, which is what makes the click run above collapse to a
                // single click the moment the pointer moves.
                self.click_run = None;
                self.doc_mut().place_cursor(Position::new(line, col), true);
                self.sync_viewport();
            }
            CodeInput::Release => {
                self.selecting = false;
            }
            CodeInput::Scroll { delta_lines } => {
                self.doc_mut().scroll_by(delta_lines as isize);
            }
            CodeInput::Key { key, modifiers } => {
                if self.dialog.is_some() {
                    self.handle_dialog_key(key);
                    return;
                }
                if let Some(command) = self.global_command(key, modifiers) {
                    self.run(command);
                    return;
                }
                self.handle_editor_key(key, modifiers);
            }
            CodeInput::Text { character } => {
                if self.dialog.is_some() {
                    return;
                }
                if !character.is_control() {
                    let mut buf = [0u8; 4];
                    let text = character.encode_utf8(&mut buf).to_string();
                    self.insert_text(&text);
                }
            }
        }
    }

    fn handle_tree_input(&mut self, input: TreeInput) {
        match input {
            TreeInput::Activate { row } => {
                self.focus = Focus::Tree;
                self.menu_open = None;
                self.tree_selected = Some(row);
                // A directory toggles on a single click, as a sidebar does; a
                // file opens on the first click too, because a tree is a list of
                // things to open.
                self.activate_tree_row(row);
            }
            TreeInput::Toggle { row } => {
                self.focus = Focus::Tree;
                self.tree_selected = Some(row);
                if let Some(entry) = self.tree.rows().get(row).copied() {
                    self.tree.toggle(entry.node);
                    self.sync_tree();
                }
            }
            TreeInput::Scroll { delta_rows } => {
                let rows = self.tree.rows().len();
                let height = self.tree_rows_visible();
                let max = rows.saturating_sub(height);
                let next = self.tree_scroll as isize + delta_rows as isize;
                self.tree_scroll = next.clamp(0, max as isize) as usize;
            }
            TreeInput::Key { key, modifiers } => {
                if let Some(command) = self.global_command(key, modifiers) {
                    self.run(command);
                    return;
                }
                self.handle_tree_key(key, modifiers);
            }
        }
    }

    fn handle_prompt_input(&mut self, input: LineEditInput, replace_field: bool) {
        if let Prompt::Find { on_replace, .. } = &mut self.prompt {
            *on_replace = replace_field;
        }
        self.focus = Focus::Prompt;
        match input {
            LineEditInput::Text { character } => {
                if let Some(line) = self.prompt.input_mut() {
                    line.insert(character);
                }
                match &self.prompt {
                    Prompt::Find { .. } => self.refresh_find(),
                    Prompt::FileFinder { .. } => self.refresh_finder(),
                    Prompt::Palette { .. } => self.refresh_palette(),
                    _ => {}
                }
            }
            LineEditInput::Key { key, modifiers } => {
                if let Some(command) = self.global_command(key, modifiers) {
                    self.run(command);
                    return;
                }
                self.handle_prompt_key(key, modifiers);
            }
            LineEditInput::Caret { col } => {
                if let Some(line) = self.prompt.input_mut() {
                    line.set_caret(col);
                }
            }
            LineEditInput::Focus => {}
        }
    }
}

impl App for EditorApp {
    type Message = EditorMsg;

    fn view(&self) -> Node<EditorMsg> {
        view::build(self)
    }

    fn update(&mut self, msg: EditorMsg) -> Action {
        match msg {
            EditorMsg::Code(input) => self.handle_code_input(input),
            EditorMsg::Tree(input) => self.handle_tree_input(input),
            EditorMsg::Tab(TabInput::Select(index)) => {
                if index < self.docs.len() {
                    self.active = index;
                    self.focus = Focus::Editor;
                    self.sync_viewport();
                }
            }
            EditorMsg::Tab(TabInput::Close(index)) => self.close_tab(index, false),
            EditorMsg::MenuBar(MenuBarInput::Open { index, x, y }) => {
                self.menu_open = Some((index, x, y));
            }
            EditorMsg::MenuBar(MenuBarInput::Hover { index, x, y }) => {
                if self.menu_open.is_some() {
                    self.menu_open = Some((index, x, y));
                }
            }
            EditorMsg::MenuBar(MenuBarInput::Close) => self.menu_open = None,
            EditorMsg::MenuItem(item) => {
                let command = self
                    .menu_open
                    .and_then(|(menu, _, _)| menu_command(menu, item));
                self.menu_open = None;
                if let Some(command) = command {
                    self.run(command);
                }
            }
            EditorMsg::DismissMenu => self.menu_open = None,
            EditorMsg::DismissPrompt => self.close_prompt(),
            EditorMsg::Prompt(input) => self.handle_prompt_input(input, false),
            EditorMsg::PromptReplace(input) => self.handle_prompt_input(input, true),
            EditorMsg::SidebarDrag(DragInput::Begin) => self.sidebar_dragging = true,
            EditorMsg::SidebarDrag(DragInput::Move(x)) => {
                let ceiling = (self.window_width - view::MIN_CODE_WIDTH).max(MIN_SIDEBAR_WIDTH);
                self.sidebar_width = x.clamp(MIN_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH.min(ceiling));
                self.sync_viewport();
            }
            EditorMsg::SidebarDrag(DragInput::End) => self.sidebar_dragging = false,
            EditorMsg::Run(command) => self.run(command),
            EditorMsg::Pick(row) => self.overlay_pick(row),
            EditorMsg::Dialog(index) => self.dialog_action(index),
        }
        Action::Rebuild
    }

    fn on_key(&mut self, key: Key, mods: Modifiers) -> Action {
        // Reached only when no widget claimed the key: the dialog, the open
        // menu and whichever pane has focus are all here.
        if self.handle_dialog_key(key) {
            return Action::Rebuild;
        }
        if self.dialog.is_some() {
            return Action::None;
        }
        if self.menu_open.is_some() && matches!(key, Key::Named(NamedKey::Escape)) {
            self.menu_open = None;
            return Action::Rebuild;
        }
        if let Some(command) = self.global_command(key, mods) {
            self.run(command);
            return Action::Rebuild;
        }
        match self.focus {
            Focus::Editor => self.handle_editor_key(key, mods),
            Focus::Tree => self.handle_tree_key(key, mods),
            Focus::Prompt => self.handle_prompt_key(key, mods),
        }
        Action::Rebuild
    }

    fn on_paste(&mut self, text: String) -> Action {
        if text.is_empty() {
            self.paste_local();
            return Action::Rebuild;
        }
        match self.focus {
            Focus::Prompt => {
                if let Some(line) = self.prompt.input_mut() {
                    line.insert_str(&text);
                }
            }
            _ => self.insert_text(&text),
        }
        Action::Rebuild
    }

    fn on_resize(&mut self, width: u32, height: u32) -> Action {
        self.window_width = width as i32;
        self.window_height = height as i32;
        self.sidebar_width = self
            .sidebar_width
            .min((self.window_width - view::MIN_CODE_WIDTH).max(MIN_SIDEBAR_WIDTH));
        self.sync_viewport();
        Action::Rebuild
    }

    fn title(&self) -> &str {
        "Sloped"
    }

    fn app_id(&self) -> &str {
        "org.slopos.editor"
    }
}

pub fn editor_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app = EditorApp::new(&args);
    slopos_appkit::run_app(app, view::DEFAULT_WIDTH as u32, view::DEFAULT_HEIGHT as u32)
}

/// A chord's key with its letter folded to lower case.
fn normalize_chord(key: Key) -> Key {
    match key {
        Key::Char(c) => Key::Char(c.to_ascii_lowercase()),
        other => other,
    }
}

/// The window title an editor shows: the file, its state, and the application.
pub fn window_title(path: Option<&str>, modified: bool) -> String {
    let name = path.map(file_name).unwrap_or("untitled");
    if modified {
        format!("● {name} — Sloped")
    } else {
        format!("{name} — Sloped")
    }
}
