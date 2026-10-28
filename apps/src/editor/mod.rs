//! Editor: the filesystem, the clipboard, the keymap and the state machine
//! over `editor-core`'s documents, which hold everything that is not pixels.

mod commands;
mod files;
mod prompt;
pub mod said;
mod syntax;
mod view;

use slopos_appkit::widgets::code_view::CodeInput;
use slopos_appkit::widgets::drag_handle::DragInput;
use slopos_appkit::widgets::editor_tabs::TabInput;
use slopos_appkit::widgets::line_edit::LineEditInput;
use slopos_appkit::widgets::menu_bar::MenuBarInput;
use slopos_appkit::widgets::tree_view::TreeInput;
use slopos_appkit::{Action, App, Key, LineInput, Modifiers, NamedKey, Node, StyleSheet, Tone};

use slopos_editor_core::buffer::Position;
use slopos_editor_core::cursor::Motion;
use slopos_editor_core::document::{Document, parent_dir};
use slopos_editor_core::filetree::FileTree;
use slopos_editor_core::search::{self, SearchOptions};

pub use commands::Command;
use commands::{PALETTE_COMMANDS, menu_command};
use prompt::Prompt;
pub use view::MIN_CODE_WIDTH;

const SCROLL_MARGIN: usize = 3;
/// Past this many matches nobody scrolls; they type more.
const FINDER_RESULTS: usize = 200;
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
    ConfirmClose {
        index: usize,
    },
    ConfirmQuit,
    /// The file a save would land on is not the one this tab read.
    ConfirmOverwrite {
        index: usize,
        path: String,
        found: Overwrite,
    },
    About,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overwrite {
    Changed,
    Gone,
    /// Another file has the name a Save as chose.
    Exists,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Save,
    Discard,
    Cancel,
}

#[derive(Clone, Debug)]
pub enum EditorMsg {
    Code(CodeInput),
    Tree(TreeInput),
    Tab(TabInput),
    MenuBar(MenuBarInput),
    MenuItem(usize),
    DismissMenu,
    DismissPrompt,
    Prompt(LineEditInput),
    PromptReplace(LineEditInput),
    SidebarDrag(DragInput),
    Run(Command),
    /// A row of the finder or the palette, counted from the first shown.
    Pick(usize),
    Dialog(Answer),
}

pub struct EditorApp {
    docs: Vec<Document>,
    active: usize,
    tree: FileTree,
    tree_selected: Option<usize>,
    tree_scroll: usize,
    focus: Focus,
    prompt: Prompt,
    overlay_scroll: usize,
    last_query: String,
    /// Kept as a pair: apart, a Replace all from the palette could pair one
    /// search's query with another's replacement, or with none.
    last_replace: Option<(String, String)>,
    finder_paths: Vec<String>,
    /// Every message ends in `refresh_find`, which this spares a rescan when
    /// nothing it counts changed.
    find_stamp: Option<FindStamp>,
    menu_open: Option<(usize, i32, i32)>,
    dialog: Option<Dialog>,
    sidebar_width: i32,
    sidebar_visible: bool,
    show_line_numbers: bool,
    window_width: i32,
    window_height: i32,
    status: String,
    status_tone: Tone,
    /// What a paste gives when the compositor has no selection to give.
    local_clipboard: String,
    untitled_count: usize,
    search_options: SearchOptions,
    visible_lines: usize,
    visible_cols: usize,
    // Drags live here, not in a widget: the tree is rebuilt between a press
    // and the moves that follow it.
    selecting: bool,
    sidebar_dragging: bool,
    code_scroll_dragging: bool,
    tree_scroll_dragging: bool,
    /// `(document id, line, column, clicks)`; the id, because opening a file
    /// over the scratch buffer reuses its tab's index.
    click_run: Option<(u64, usize, usize, u8)>,
}

impl EditorApp {
    pub fn new(args: &[String]) -> Self {
        let arg = args.first().map(String::as_str);
        let root = files::start_directory(arg);
        let base = files::start_directory(None);
        let mut app = Self {
            docs: Vec::new(),
            active: 0,
            tree: FileTree::new(&root),
            tree_selected: None,
            tree_scroll: 0,
            focus: Focus::Editor,
            prompt: Prompt::None,
            overlay_scroll: 0,
            last_query: String::new(),
            last_replace: None,
            finder_paths: Vec::new(),
            find_stamp: None,
            menu_open: None,
            dialog: None,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_visible: true,
            show_line_numbers: true,
            window_width: view::DEFAULT_WIDTH,
            window_height: view::DEFAULT_HEIGHT,
            status: String::new(),
            status_tone: Tone::Secondary,
            local_clipboard: String::new(),
            untitled_count: 0,
            search_options: SearchOptions::default(),
            visible_lines: 24,
            visible_cols: 80,
            selecting: false,
            sidebar_dragging: false,
            code_scroll_dragging: false,
            tree_scroll_dragging: false,
            click_run: None,
        };
        app.sync_tree();

        // Only the first argument chose the root; the rest may sit outside it.
        for path in args {
            if files::is_dir(path) {
                continue;
            }
            let absolute = files::absolutize(&base, path);
            app.open_path(&absolute);
        }
        app.active = 0;
        if app.docs.is_empty() {
            app.new_document();
        }
        app.sync_viewport();
        app
    }

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

    /// What the code surface highlights: the open find bar's query alone, as
    /// `find_query`'s fallback would light every tab with no way to turn it off.
    pub(super) fn active_search_query(&self) -> Option<String> {
        match &self.prompt {
            Prompt::Find { query, .. } if !query.text.is_empty() => Some(query.text.clone()),
            _ => None,
        }
    }

    /// Remembered rather than derived from the selection, which would pin the
    /// selection to the last row drawn.
    pub fn overlay_first_row(&self) -> usize {
        self.overlay_scroll
    }

    pub fn overlay_rows(&self) -> usize {
        view::overlay_rows(self.window_height)
    }

    fn sync_overlay_scroll(&mut self) {
        let rows = self.overlay_rows();
        let Some((selected, total)) = self.prompt.overlay_mut().map(|(s, n)| (*s, n)) else {
            self.overlay_scroll = 0;
            return;
        };
        let max_first = total.saturating_sub(rows);
        let mut first = self.overlay_scroll.min(max_first);
        if selected < first {
            first = selected;
        } else if selected >= first + rows {
            first = selected + 1 - rows;
        }
        self.overlay_scroll = first;
    }

    fn overlay_pick(&mut self, row: usize) {
        let index = self.overlay_first_row() + row;
        match self.prompt.overlay_mut() {
            Some((selected, total)) if index < total => *selected = index,
            // Clicked on rows the results have since shrunk from under.
            _ => return,
        }
        self.accept_prompt();
    }

    pub fn status(&self) -> &str {
        &self.status
    }

    fn note(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_tone = Tone::Secondary;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.status_tone = Tone::Danger;
    }

    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    pub fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
    }

    /// `(matches, the current one)` while the find bar is open.
    pub fn find_progress(&self) -> Option<(usize, usize)> {
        match &self.prompt {
            Prompt::Find { total, current, .. } => Some((*total, *current)),
            _ => None,
        }
    }

    pub fn open(&mut self, path: &str) {
        self.open_path(path);
    }

    pub fn prompt_text(&self) -> String {
        self.prompt
            .input_text()
            .map(|(text, _)| text)
            .unwrap_or_default()
    }

    pub fn finder_results(&self) -> Vec<String> {
        match &self.prompt {
            Prompt::FileFinder { results, .. } => results.clone(),
            _ => Vec::new(),
        }
    }

    pub fn prompt_is_open(&self) -> bool {
        self.prompt.is_open()
    }

    pub fn clear_status(&mut self) {
        self.status.clear();
    }

    pub fn prompt_is_overlay(&self) -> bool {
        self.prompt.is_overlay()
    }

    pub fn clear_prompt(&mut self) {
        if let Some(input) = self.prompt.input_mut() {
            input.clear();
        }
    }

    /// Types `text` into the open prompt as keystrokes would.
    pub fn type_in_prompt(&mut self, text: &str) {
        for character in text.chars() {
            self.handle_prompt_input(LineEditInput::Text { character }, false);
        }
    }

    pub(super) fn tree_row_count(&self) -> usize {
        self.tree.rows().len()
    }

    /// A refresh rebuilds the rows, so a stale row index names another file,
    /// or none, and Enter looks dead.
    fn clamp_tree_selection(&mut self) {
        let rows = self.tree.rows().len();
        self.tree_selected = match self.tree_selected {
            Some(_) if rows == 0 => None,
            Some(row) => Some(row.min(rows - 1)),
            None => None,
        };
        self.tree_scroll = self.tree_scroll.min(rows.saturating_sub(1));
    }

    /// `(name, depth, is_dir, expanded, path)` of `count` rows from `first`.
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

    /// There is always one: closing the last tab opens a scratch buffer.
    fn doc(&self) -> &Document {
        &self.docs[self.active]
    }

    fn doc_mut(&mut self) -> &mut Document {
        let index = self.active;
        &mut self.docs[index]
    }

    fn new_document(&mut self) {
        self.untitled_count += 1;
        let title = format!("Untitled {}", self.untitled_count);
        self.docs.push(Document::empty(title));
        self.active = self.docs.len() - 1;
        self.focus = Focus::Editor;
    }

    fn open_path(&mut self, path: &str) {
        if let Some(index) = self
            .docs
            .iter()
            .position(|d| d.path().is_some_and(|p| p == path))
        {
            self.active = index;
            self.focus = Focus::Editor;
            self.note(said::opened(path));
            self.sync_viewport();
            return;
        }

        match files::read_file(path) {
            Ok(text) => match Document::from_text(Some(path.to_string()), &text) {
                Ok(doc) => {
                    let replace_scratch = self.docs.len() == 1
                        && self.docs[0].path().is_none()
                        && !self.docs[0].is_modified()
                        && self.docs[0].buffer.is_empty();
                    if replace_scratch {
                        // Its index is about to name this file instead.
                        if matches!(self.prompt, Prompt::SaveAs { .. }) {
                            self.prompt = Prompt::None;
                            self.focus = Focus::Editor;
                        }
                        self.docs.clear();
                    }
                    let mut doc = doc;
                    doc.set_disk_stamp(files::disk_stamp(path));
                    self.docs.push(doc);
                    self.active = self.docs.len() - 1;
                    self.focus = Focus::Editor;
                    self.note(said::opened(path));
                    self.reveal_in_tree(path);
                }
                Err(_) => self.fail(said::too_many_lines(path)),
            },
            Err(message) => self.fail(message),
        }
        self.sync_viewport();
    }

    fn save_document(&mut self, index: usize, path: Option<String>) {
        let Some(doc) = self.docs.get(index) else {
            self.fail(said::TAB_GONE);
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
        // Two tabs on one file: whichever saved last would win.
        if self
            .docs
            .iter()
            .enumerate()
            .any(|(i, d)| i != index && d.path().is_some_and(|p| p == target))
        {
            self.fail(said::open_elsewhere(&target));
            return;
        }
        let on_disk = files::disk_stamp(&target);
        let found = if doc.path() == Some(target.as_str()) {
            match (doc.disk_stamp(), on_disk) {
                (Some(known), Some(now)) if known != now => Some(Overwrite::Changed),
                (Some(_), None) => Some(Overwrite::Gone),
                _ => None,
            }
        } else {
            on_disk.map(|_| Overwrite::Exists)
        };
        if let Some(found) = found {
            self.dialog = Some(Dialog::ConfirmOverwrite {
                index,
                path: target,
                found,
            });
            return;
        }
        self.write_document(index, target);
    }

    fn write_document(&mut self, index: usize, target: String) {
        let Some(doc) = self.docs.get(index) else {
            self.fail(said::TAB_GONE);
            return;
        };
        let text = doc.text();
        match files::write_file(&target, &text) {
            Ok(()) => {
                let named = self.docs[index].path() != Some(target.as_str());
                self.docs[index].mark_saved(Some(target.clone()));
                self.docs[index].set_disk_stamp(files::disk_stamp(&target));
                self.note(said::saved(&target));
                if named {
                    self.refresh_tree_dir(parent_dir(&target));
                    self.reveal_in_tree(&target);
                }
            }
            Err(message) => self.fail(message),
        }
    }

    /// In the document's own folder: the tree's root would turn a Save as of a
    /// file from elsewhere into a copy beside the tree.
    fn default_save_path(&self, index: usize) -> String {
        let own = self
            .docs
            .get(index)
            .and_then(Document::path)
            .map(parent_dir)
            .filter(|d| !d.is_empty());
        let mut path = String::from(
            own.unwrap_or_else(|| self.tree.root_path())
                .trim_end_matches('/'),
        );
        path.push('/');
        path.push_str(
            self.docs
                .get(index)
                .map(Document::title)
                .unwrap_or("untitled"),
        );
        path
    }

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

    /// Ctrl+W reaches `close_tab` while a Save as is open, which would
    /// otherwise write another tab's text to the path typed for this one.
    fn retarget_document_index(&mut self, closed: usize) {
        if let Prompt::SaveAs { index, .. } = &mut self.prompt {
            match (*index).cmp(&closed) {
                core::cmp::Ordering::Equal => {
                    self.prompt = Prompt::None;
                    self.focus = Focus::Editor;
                    self.fail(said::TAB_GONE);
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

    fn sync_tree(&mut self) {
        self.sync_tree_inner();
        self.clamp_tree_selection();
    }

    fn sync_tree_inner(&mut self) {
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
                let complete = match files::read_dir(&path) {
                    Ok(entries) => self.tree.populate(index, entries),
                    Err(message) => {
                        self.fail(message);
                        self.tree.populate(index, Vec::new())
                    }
                };
                if !complete {
                    self.fail(said::tree_full(slopos_editor_core::filetree::MAX_NODES));
                }
            }
        }
    }

    /// Reads every folder under the root, bounded, so the finder offers more
    /// than the sidebar has open.
    fn index_tree(&mut self) {
        const MAX_DIRS: usize = 4_000;
        let mut read = 0usize;
        loop {
            let unread = self.tree.unread_dirs();
            if unread.is_empty() {
                return;
            }
            for index in unread {
                if read >= MAX_DIRS {
                    self.fail(said::index_full(MAX_DIRS));
                    return;
                }
                let Some(path) = self.tree.node(index).map(|n| n.path.clone()) else {
                    continue;
                };
                read += 1;
                // Populated even when unreadable, or the walk would retry it.
                let complete = match files::read_dir(&path) {
                    Ok(entries) => self.tree.populate(index, entries),
                    Err(_) => self.tree.populate(index, Vec::new()),
                };
                if !complete {
                    self.fail(said::tree_full(slopos_editor_core::filetree::MAX_NODES));
                    return;
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

    fn reveal_in_tree(&mut self, path: &str) {
        let root = self.tree.root_path().to_string();
        let Some(relative) =
            slopos_editor_core::filetree::relative_to(&root, path).map(str::to_string)
        else {
            return;
        };
        // Each ancestor is read as it is reached: below it is not in the tree yet.
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
            self.fail(said::not_a_folder(path));
            return;
        }
        self.tree = FileTree::new(path);
        self.tree_selected = None;
        self.tree_scroll = 0;
        self.sync_tree();
        self.note(said::opened(path));
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

    /// After anything that can move the caret or resize the window.
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
        // A refused first open leaves none inside `EditorApp::new`.
        if self.docs.is_empty() {
            return;
        }
        let doc = self.doc_mut();
        doc.viewport.visible_lines = lines;
        doc.viewport.visible_cols = cols;
        doc.scroll_to_cursor(SCROLL_MARGIN);
    }

    fn prompt_bar_visible(&self) -> bool {
        self.prompt.is_open() && !self.prompt.is_overlay()
    }

    fn sidebar_ceiling(&self) -> i32 {
        let divider = StyleSheet::current().divider_width;
        (self.window_width - divider - view::MIN_CODE_WIDTH).max(MIN_SIDEBAR_WIDTH)
    }

    fn find_bar_open(&self) -> bool {
        matches!(self.prompt, Prompt::Find { .. })
    }

    fn copy_selection(&mut self, cut: bool) {
        let Some(text) = self.doc().selected_text() else {
            return;
        };
        self.local_clipboard = text.clone();
        slopos_appkit::clipboard::copy(&text);
        if cut {
            self.doc_mut().delete_selection();
            self.sync_viewport();
        }
        self.note(if cut { said::CUT } else { said::COPIED });
    }

    fn paste(&mut self) {
        // Answered at `on_paste`, empty when nothing owns the selection.
        if !slopos_appkit::clipboard::request_paste() {
            self.paste_local();
        }
    }

    fn paste_local(&mut self) {
        if self.local_clipboard.is_empty() {
            self.note(said::EMPTY_CLIPBOARD);
            return;
        }
        let text = self.local_clipboard.clone();
        self.insert_text(&text);
    }

    fn insert_text(&mut self, text: &str) {
        if !self.doc_mut().insert_text(text) && !text.is_empty() {
            self.fail(said::line_limit());
        }
        self.sync_viewport();
    }

    /// The open find bar's query, else the last one, so F3 and the palette's
    /// Find next work with the bar closed.
    fn find_query(&self) -> Option<String> {
        match &self.prompt {
            Prompt::Find { query, .. } if !query.text.is_empty() => Some(query.text.clone()),
            Prompt::Find { .. } => None,
            _ => Some(self.last_query.clone()).filter(|q| !q.is_empty()),
        }
    }

    fn remember_query(&mut self) {
        if let Prompt::Find { query, .. } = &self.prompt {
            if !query.text.is_empty() {
                self.last_query = query.text.clone();
            }
        }
    }

    /// Recounts the find bar's matches; free with the bar closed.
    fn refresh_find(&mut self) {
        self.remember_query();
        self.remember_replacement();
        if !matches!(self.prompt, Prompt::Find { .. }) {
            self.find_stamp = None;
            return;
        }
        let Some(query) = self.find_query() else {
            if let Prompt::Find { total, current, .. } = &mut self.prompt {
                *total = 0;
                *current = 0;
            }
            return;
        };
        let options = self.search_options;
        // `find_step` parks the caret at a match's end, so the caret alone
        // names the match after it.
        let doc = &self.docs[self.active];
        let anchor = doc
            .cursor
            .selection()
            .map(|s| s.start)
            .unwrap_or(doc.cursor.position);
        let stamp = FindStamp {
            document: doc.id(),
            revision: doc.buffer.revision(),
            anchor,
            query: query.clone(),
            options,
        };
        if self.find_stamp.as_ref() == Some(&stamp) {
            return;
        }
        self.find_stamp = Some(stamp);
        let matches = search::find_all(&self.docs[self.active].buffer, &query, options);
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
            self.note(said::nothing_to_find(self.find_bar_open()));
            return;
        }
        let doc = &self.docs[self.active];
        // From the current match's start: forward one past it so Enter
        // advances, backward at it so it does not find it again.
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

    fn find_from(&mut self, forward: bool, from: Position) {
        let Some(query) = self.find_query() else {
            self.note(said::nothing_to_find(self.find_bar_open()));
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
            None => self.note(said::no_match(&query)),
        }
    }

    fn replace_current(&mut self) {
        let Some(query) = self.find_query() else {
            return;
        };
        let Some(replacement) = self.replace_text(&query) else {
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
                // At the end, not one past: advancing would skip what follows.
                let from = self.doc().cursor.position;
                self.find_from(true, from);
                return;
            }
        }
        self.find_step(true);
    }

    fn replace_all(&mut self) {
        let Some(query) = self.find_query() else {
            let bar_open = matches!(
                self.prompt,
                Prompt::Find {
                    replace: Some(_),
                    ..
                }
            );
            self.note(said::nothing_to_replace(bar_open));
            return;
        };
        let Some(replacement) = self.replace_text(&query) else {
            self.note(said::no_replacement());
            return;
        };
        let options = self.search_options;
        match self.doc_mut().replace_all(&query, &replacement, options) {
            0 => self.note(said::no_match(&query)),
            count => self.note(said::replaced(count)),
        }
        self.refresh_find();
        self.sync_viewport();
    }

    /// `None` unless a replacement was given for this very query.
    fn replace_text(&self, query: &str) -> Option<String> {
        match &self.prompt {
            Prompt::Find { replace, .. } => replace.as_ref().map(|r| r.text.clone()),
            _ => self
                .last_replace
                .as_ref()
                .filter(|(q, _)| q == query)
                .map(|(_, r)| r.clone()),
        }
    }

    fn remember_replacement(&mut self) {
        if let Prompt::Find {
            query,
            replace: Some(replace),
            ..
        } = &self.prompt
        {
            if !query.text.is_empty() {
                self.last_replace = Some((query.text.clone(), replace.text.clone()));
            }
        }
    }

    fn open_prompt(&mut self, prompt: Prompt) {
        self.find_stamp = None;
        self.prompt = prompt;
        self.overlay_scroll = 0;
        self.focus = Focus::Prompt;
        self.menu_open = None;
        self.sync_viewport();
    }

    fn close_prompt(&mut self) {
        self.find_stamp = None;
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
        self.sync_overlay_scroll();
    }

    fn refresh_finder(&mut self) {
        // Cloned only where it survives: this runs per keystroke over every path.
        let paths = &self.finder_paths;
        if let Prompt::FileFinder {
            input,
            results,
            selected,
        } = &mut self.prompt
        {
            let needle = input.text.as_str();
            let mut scored: Vec<(&str, i32)> = paths
                .iter()
                .filter_map(|p| search::fuzzy_score(p, needle).map(|s| (p.as_str(), s)))
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            scored.truncate(FINDER_RESULTS);
            *results = scored.into_iter().map(|(p, _)| p.to_string()).collect();
            *selected = (*selected).min(results.len().saturating_sub(1));
        }
        self.sync_overlay_scroll();
    }

    fn accept_prompt(&mut self) {
        let prompt = std::mem::replace(&mut self.prompt, Prompt::None);
        match prompt {
            Prompt::None => {}
            Prompt::Find { .. } => {
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
                    _ => {
                        let lines = self.doc().buffer.line_count();
                        self.fail(said::not_a_line(input.text.trim(), lines));
                    }
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
        if let Some((selected, total)) = self.prompt.overlay_mut()
            && total > 0
        {
            *selected = selected.saturating_add_signed(delta).min(total - 1);
        }
        self.sync_overlay_scroll();
    }

    fn run(&mut self, command: Command) {
        self.menu_open = None;
        // Another command abandons a question; the find bar is none.
        if !command.reads_prompt() && !matches!(self.prompt, Prompt::None | Prompt::Find { .. }) {
            self.close_prompt();
        }
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
                    self.note(said::NOTHING_TO_UNDO);
                }
                self.sync_viewport();
            }
            Command::Redo => {
                if !self.doc_mut().redo() {
                    self.note(said::NOTHING_TO_REDO);
                }
                self.sync_viewport();
            }
            Command::Cut => self.copy_selection(true),
            Command::Copy => self.copy_selection(false),
            Command::Paste => self.paste(),
            Command::SelectAll => {
                self.doc_mut().select_all();
                self.sync_viewport();
            }
            Command::SelectLine => {
                let line = self.doc().cursor.position.line;
                self.doc_mut().select_line_at(line);
                self.sync_viewport();
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
                let language = self.doc().language();
                if language.line_comment().is_none() {
                    self.note(said::no_comments(language.label()));
                    return;
                }
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
                        _ => self.last_query.clone(),
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
                self.index_tree();
                self.finder_paths = self
                    .tree
                    .file_paths()
                    .into_iter()
                    .map(str::to_string)
                    .collect();
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
                    self.fail(said::NOT_SAVED_YET);
                }
            }
            Command::KeyboardShortcuts => {
                let path = String::from(files::DOC_PATH);
                if files::is_file(&path) {
                    self.open_path(&path);
                } else {
                    self.fail(said::no_manual());
                }
            }
            Command::About => self.dialog = Some(Dialog::About),
        }
    }

    fn global_command(&self, key: Key, mods: Modifiers) -> Option<Command> {
        // Shift upper-cases a chord's letter.
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
            // Named so a focused button takes it; here it is a character.
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
            // A click into the code leaves a bar open without focus.
            Key::Named(NamedKey::Escape) => {
                if self.prompt.is_open() {
                    self.close_prompt();
                } else {
                    self.doc_mut().cursor.clear_selection();
                }
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
            Key::Named(NamedKey::Escape) => {
                if self.prompt.is_open() {
                    self.close_prompt();
                } else {
                    self.focus = Focus::Editor;
                }
            }
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
                    self.replace_current();
                } else {
                    self.accept_prompt();
                }
            }
            Key::Named(NamedKey::Up) => self.move_prompt_selection(-1),
            Key::Named(NamedKey::Down) => self.move_prompt_selection(1),
            Key::Named(NamedKey::Tab) => match &mut self.prompt {
                Prompt::Find {
                    replace: Some(_),
                    on_replace,
                    ..
                } => *on_replace = !*on_replace,
                _ if mods.shift => self.move_prompt_selection(-1),
                _ => self.move_prompt_selection(1),
            },
            _ => {}
        }
    }

    fn answer(&mut self, answer: Answer) {
        let Some(dialog) = self.dialog.take() else {
            return;
        };
        match (dialog, answer) {
            (_, Answer::Cancel)
            | (Dialog::About, _)
            | (Dialog::ConfirmOverwrite { .. }, Answer::Discard) => {}
            (Dialog::ConfirmClose { index }, Answer::Save) => {
                self.save_document(index, None);
                // Saving can ask about overwriting, or open a Save as, instead.
                if self.docs.get(index).is_some_and(|d| !d.is_modified()) {
                    self.close_tab(index, true);
                }
            }
            (Dialog::ConfirmClose { index }, Answer::Discard) => self.close_tab(index, true),
            (Dialog::ConfirmOverwrite { index, path, .. }, Answer::Save) => {
                self.write_document(index, path)
            }
            (Dialog::ConfirmQuit, Answer::Save) => {
                for i in 0..self.docs.len() {
                    if self.docs[i].is_modified() && self.docs[i].path().is_some() {
                        self.save_document(i, None);
                    }
                }
                if !self.any_modified() {
                    std::process::exit(0);
                }
                let unsaved_named = self
                    .docs
                    .iter()
                    .any(|d| d.is_modified() && d.path().is_some());
                if !unsaved_named && self.dialog.is_none() {
                    self.fail(said::unnamed_left());
                }
            }
            (Dialog::ConfirmQuit, Answer::Discard) => std::process::exit(0),
        }
    }

    fn handle_code_input(&mut self, input: CodeInput) {
        // Not on a release, which reaches every widget and would wipe what its
        // own press said.
        if !matches!(
            input,
            CodeInput::Scroll { .. } | CodeInput::ScrollTo { .. } | CodeInput::Release
        ) {
            self.status.clear();
        }
        match input {
            CodeInput::Click { line, col, extend } => {
                self.focus = Focus::Editor;
                self.menu_open = None;
                self.selecting = true;
                // No timer: a slow double click on one cell costs a word selected.
                let id = self.doc().id();
                let clicks = match self.click_run {
                    Some((d, l, c, n)) if d == id && l == line && c == col => (n % 3) + 1,
                    _ => 1,
                };
                self.click_run = Some((id, line, col, clicks));
                let position = Position::new(line, col);
                match clicks {
                    2 => self.doc_mut().select_word_at(position),
                    3 => self.doc_mut().select_line_at(line),
                    _ => self.doc_mut().place_cursor(position, extend),
                }
                self.sync_viewport();
            }
            CodeInput::Drag { line, col } => {
                self.click_run = None;
                self.doc_mut().place_cursor(Position::new(line, col), true);
                self.sync_viewport();
            }
            CodeInput::Release => {
                self.selecting = false;
                self.code_scroll_dragging = false;
            }
            CodeInput::Scroll { delta_lines } => {
                self.doc_mut().scroll_by(delta_lines as isize);
            }
            CodeInput::ScrollTo { first_line } => {
                self.code_scroll_dragging = true;
                let max = self.doc().buffer.line_count().saturating_sub(1);
                self.doc_mut().viewport.first_line = first_line.min(max);
            }
            CodeInput::Key { key, modifiers } => {
                if self.dialog.is_some() {
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
            TreeInput::Release => {
                self.tree_scroll_dragging = false;
            }
            TreeInput::ScrollTo { first_row } => {
                self.tree_scroll_dragging = true;
                let max = self
                    .tree
                    .rows()
                    .len()
                    .saturating_sub(self.tree_rows_visible());
                self.tree_scroll = first_row.min(max);
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
            LineEditInput::Focus | LineEditInput::Blur => {}
        }
    }
}

impl App for EditorApp {
    type Message = EditorMsg;

    fn view(&self) -> Node<EditorMsg> {
        view::build(self)
    }

    fn update(&mut self, msg: EditorMsg) -> Action {
        let action = self.dispatch(msg);
        self.refresh_find();
        action
    }

    fn on_key(&mut self, key: Key, mods: Modifiers) -> Action {
        let action = self.dispatch_key(key, mods);
        self.refresh_find();
        action
    }

    fn on_paste(&mut self, text: String) -> Action {
        let action = self.dispatch_paste(text);
        self.refresh_find();
        action
    }

    fn on_close_request(&mut self) -> (bool, Action) {
        if self.any_modified() {
            self.dialog = Some(Dialog::ConfirmQuit);
            return (false, Action::Rebuild);
        }
        (true, Action::None)
    }

    fn on_resize(&mut self, width: u32, height: u32) -> Action {
        self.window_width = width as i32;
        self.window_height = height as i32;
        self.sidebar_width = self.sidebar_width.min(self.sidebar_ceiling());
        self.sync_viewport();
        self.sync_overlay_scroll();
        Action::Rebuild
    }

    fn title(&self) -> String {
        slopos_appkit::document_title(self.doc().title(), said::NAME)
    }

    fn app_id(&self) -> &str {
        "org.slopos.editor"
    }

    fn window(&self) -> slopos_appkit::WindowSpec {
        slopos_appkit::WindowSpec::new(view::DEFAULT_WIDTH, view::DEFAULT_HEIGHT)
    }
}

impl EditorApp {
    fn dispatch(&mut self, msg: EditorMsg) -> Action {
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
                // So the divider's line, its middle, stays under the pointer.
                let width = x - StyleSheet::current().divider_width / 2;
                let ceiling = MAX_SIDEBAR_WIDTH.min(self.sidebar_ceiling());
                self.sidebar_width = width.clamp(MIN_SIDEBAR_WIDTH, ceiling);
                self.sync_viewport();
            }
            EditorMsg::SidebarDrag(DragInput::End) => self.sidebar_dragging = false,
            EditorMsg::Run(command) => self.run(command),
            EditorMsg::Pick(row) => self.overlay_pick(row),
            EditorMsg::Dialog(answer) => self.answer(answer),
        }
        Action::Rebuild
    }

    fn dispatch_key(&mut self, key: Key, mods: Modifiers) -> Action {
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

    fn dispatch_paste(&mut self, text: String) -> Action {
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
}

pub fn editor_main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let app = EditorApp::new(&args);
    slopos_appkit::run_app(app)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FindStamp {
    document: u64,
    revision: u64,
    anchor: slopos_editor_core::buffer::Position,
    query: String,
    options: SearchOptions,
}

fn normalize_chord(key: Key) -> Key {
    match key {
        Key::Char(c) => Key::Char(c.to_ascii_lowercase()),
        other => other,
    }
}
