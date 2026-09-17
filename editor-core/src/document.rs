//! One open file: its buffer, cursor, undo history, highlight cache and
//! viewport, plus every operation the UI can name.
//!
//! Every edit goes through here, so every edit is recorded for undo exactly
//! once and the highlight cache is invalidated at exactly the line it changed.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::buffer::{IndentStyle, LineEnding, LoadError, Position, Range, TextBuffer};
use crate::cursor::{Cursor, Motion, line_range, word_at};
use crate::history::{Change, History};
use crate::syntax::{Highlighter, Language, LineState, Span, detect_language};

/// Where the viewport starts, in lines and columns. The view sets the extent;
/// the document owns the origin so a tab remembers where it was scrolled to.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    pub first_line: usize,
    /// **Display** column, not a character one: it is subtracted from an
    /// already-expanded column by the renderer, so a tab-indented line would
    /// otherwise scroll by the wrong amount for every tab before the caret.
    pub first_col: usize,
    pub visible_lines: usize,
    pub visible_cols: usize,
}

/// Hands out a fresh [`Document::id`]. Monotonic and never reused, so a stale
/// id designates nothing rather than designating a stranger.
fn next_document_id() -> u64 {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct Document {
    /// Stable for this document's whole life, unlike its position among the
    /// open tabs. What an application keys per-document state on.
    id: u64,
    pub buffer: TextBuffer,
    pub cursor: Cursor,
    pub viewport: Viewport,
    history: History,
    /// `None` for a buffer that has never been saved.
    path: Option<String>,
    /// Display name, which is the file name for a saved file and "untitled-N"
    /// for one that has no path yet.
    title: String,
    language: Language,
    indent: IndentStyle,
    /// Buffer revision the file on disk holds; `modified` is a comparison, not
    /// a flag that has to be cleared everywhere.
    saved_revision: u64,
    highlighter: Highlighter,
    /// Lexer state per line start, behind a cell: highlighting is a *read* of
    /// the document that happens to memoize, and a view that had to borrow the
    /// document mutably to draw it could not be a view.
    states: RefCell<StateCache>,
}

/// `states[i]` is the lexer state at the start of line `i`; `states[0]` is
/// always `Normal`. Entries past `valid` have not been computed yet.
struct StateCache {
    states: Vec<LineState>,
    valid: usize,
}

impl Document {
    pub fn empty(title: String) -> Self {
        Self::with_buffer(TextBuffer::new(), None, title)
    }

    /// Loads `text` as the contents of `path`.
    pub fn from_text(path: Option<String>, text: &str) -> Result<Self, LoadError> {
        let buffer = TextBuffer::from_str(text)?;
        let title = path
            .as_deref()
            .map(file_name)
            .unwrap_or("untitled")
            .to_string();
        Ok(Self::with_buffer(buffer, path, title))
    }

    fn with_buffer(buffer: TextBuffer, path: Option<String>, title: String) -> Self {
        let language = path
            .as_deref()
            .map(detect_language)
            .unwrap_or(Language::PlainText);
        let indent = buffer.detect_indent(IndentStyle::default());
        let saved_revision = buffer.revision();
        Self {
            id: next_document_id(),
            buffer,
            cursor: Cursor::default(),
            viewport: Viewport::default(),
            history: History::new(),
            path,
            title,
            language,
            indent,
            saved_revision,
            highlighter: Highlighter::new(language),
            states: RefCell::new(StateCache {
                states: alloc::vec![LineState::Normal],
                valid: 1,
            }),
        }
    }

    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// Undo steps held. The cap is a memory bound, so it is worth asserting.
    pub fn undo_depth(&self) -> usize {
        self.history.undo_depth()
    }

    /// This document's stable identity, which its tab position is not.
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn language(&self) -> Language {
        self.language
    }

    pub fn indent(&self) -> IndentStyle {
        self.indent
    }

    pub fn set_indent(&mut self, indent: IndentStyle) {
        self.indent = indent;
    }

    pub fn line_ending(&self) -> LineEnding {
        self.buffer.line_ending()
    }

    pub fn is_modified(&self) -> bool {
        self.buffer.revision() != self.saved_revision
    }

    pub fn can_undo(&self) -> bool {
        self.history.can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.history.can_redo()
    }

    /// Records that the buffer as it stands is what the file holds.
    pub fn mark_saved(&mut self, path: Option<String>) {
        if let Some(path) = path {
            self.title = file_name(&path).to_string();
            self.language = detect_language(&path);
            self.highlighter.set_language(self.language);
            // Only on a rename. Re-reading it on every plain Ctrl+S lets a
            // pasted block flip the Tab key of a file whose own convention has
            // not changed.
            if self.path.as_deref() != Some(path.as_str()) {
                self.indent = self.buffer.detect_indent(self.indent);
            }
            self.invalidate_from(0);
            self.path = Some(path);
        }
        self.saved_revision = self.buffer.revision();
        self.history.seal();
    }

    /// The text to write to disk.
    pub fn text(&self) -> String {
        self.buffer.to_text()
    }

    // ── highlighting ────────────────────────────────────────────────────────

    fn invalidate_from(&self, line: usize) {
        let mut cache = self.states.borrow_mut();
        cache.valid = cache.valid.min(line + 1).max(1);
        let valid = cache.valid;
        cache.states.truncate(valid);
    }

    /// Lexer state at the start of `line`, computing (and caching) the lines
    /// above it that have not been lexed yet.
    fn state_at(&self, line: usize) -> LineState {
        let mut cache = self.states.borrow_mut();
        while cache.valid <= line && cache.valid <= self.buffer.line_count() {
            let idx = cache.valid - 1;
            let state = cache.states[idx];
            let (_, next) = self.highlighter.line(self.buffer.line(idx), state);
            cache.states.push(next);
            cache.valid += 1;
        }
        cache.states.get(line).copied().unwrap_or(LineState::Normal)
    }

    /// Spans for one line, lexing whatever is needed above it first.
    pub fn line_spans(&self, line: usize) -> Vec<Span> {
        if line >= self.buffer.line_count() {
            return Vec::new();
        }
        let state = self.state_at(line);
        let (spans, _) = self.highlighter.line(self.buffer.line(line), state);
        spans
    }

    // ── editing ─────────────────────────────────────────────────────────────

    /// Applies an insert as one undoable step, moving the cursor past it.
    fn apply_insert(&mut self, at: Position, text: &str, coalesce: bool) -> bool {
        if text.is_empty() {
            return false;
        }
        let at = self.buffer.clamp(at);
        let before = self.cursor;
        let revision = self.buffer.revision();
        let end = self.buffer.insert(at, text);
        if self.buffer.revision() == revision {
            // Refused — the line ceiling. Recording it would leave an undo step
            // that deletes a range the buffer does not have.
            return false;
        }
        self.cursor.set_position(end, false);
        self.invalidate_from(at.line);
        self.history.record(
            Change::Insert {
                at,
                text: text.to_string(),
            },
            before,
            self.cursor,
            coalesce,
        );
        true
    }

    /// Applies a delete as one undoable step, leaving the cursor at its start.
    fn apply_delete(&mut self, range: Range) -> String {
        let range = Range::new(self.buffer.clamp(range.start), self.buffer.clamp(range.end));
        if range.is_empty() {
            return String::new();
        }
        let before = self.cursor;
        let text = self.buffer.delete(range);
        self.cursor.set_position(range.start, false);
        self.invalidate_from(range.start.line);
        self.history.record(
            Change::Delete {
                range,
                text: text.clone(),
            },
            before,
            self.cursor,
            false,
        );
        text
    }

    /// Deletes the selection if there is one; true when something went.
    fn delete_selection(&mut self) -> bool {
        match self.cursor.selection() {
            Some(range) => {
                self.apply_delete(range);
                true
            }
            None => false,
        }
    }

    /// Returns whether the text landed: an insert that would take the buffer
    /// past its line ceiling is refused, and the caller is the only place that
    /// can say so.
    pub fn insert_text(&mut self, text: &str) -> bool {
        // Replacing a selection is a delete and an insert, and one Ctrl+Z has
        // to take back both — and so is the re-indent a closing brace triggers.
        let had_any_selection = self.cursor.has_selection();
        let transactional = had_any_selection || self.closing_brace_dedent(text) > 0;
        if transactional {
            self.history.begin();
        }
        // The selection goes first. `apply_delete` parks the caret at the
        // range's start and clears the anchor, so a dedent computed before this
        // would delete the indent, lose the selection with it, and leave the
        // brace welded to the text the user meant to replace.
        // Refuse before anything is destroyed. The selection goes first (so the
        // dedent below reads the line the text will land on), which means a
        // ceiling check afterwards would report "refused" over a buffer that
        // had already lost the selection.
        if self.buffer.would_exceed_line_limit(text) {
            if transactional {
                self.history.end(self.cursor);
            }
            return false;
        }
        let had_selection = self.delete_selection();
        let dedent = self.closing_brace_dedent(text);
        if dedent > 0 {
            let pos = self.cursor.position;
            self.apply_delete(Range::new(
                Position::new(pos.line, 0),
                Position::new(pos.line, dedent),
            ));
            // A delete leaves the caret at the range's start, which here is
            // column 0; the brace belongs one indent level in from where the
            // caret was, not at the margin with the rest of the indent trailing
            // behind it.
            self.cursor
                .set_position(Position::new(pos.line, pos.col - dedent), false);
        }
        let at = self.cursor.position;
        // A typed character coalesces with the run before it; a paste does not,
        // and neither does the first insert after a selection was replaced.
        let coalesce = !had_selection && text.chars().count() == 1 && !text.contains('\n');
        let inserted = self.apply_insert(at, text, coalesce);
        if transactional {
            self.history.end(self.cursor);
        }
        inserted
    }

    /// Characters to remove from the front of the current line before inserting
    /// `text`, so a closing brace typed on an otherwise blank line lands one
    /// level out — the "electric" dedent every editor does, and the reason a
    /// block closes at the column it opened at.
    fn closing_brace_dedent(&self, text: &str) -> usize {
        let mut chars = text.chars();
        let (Some(first), None) = (chars.next(), chars.next()) else {
            return 0;
        };
        if !matches!(first, '}' | ')' | ']') {
            return 0;
        }
        let pos = self.cursor.position;
        let line = self.buffer.line(pos.line);
        let before: alloc::string::String = line.chars().take(pos.col).collect();
        if !before.chars().all(|c| c == ' ' || c == '\t') || before.is_empty() {
            return 0;
        }
        match self.indent {
            IndentStyle::Tabs(_) => {
                if before.ends_with('\t') {
                    1
                } else {
                    0
                }
            }
            IndentStyle::Spaces(width) => {
                let width = width.max(1);
                let count = before.chars().count();
                let removable = count % width;
                if removable == 0 {
                    width.min(count)
                } else {
                    removable
                }
            }
        }
    }

    /// Enter: a new line carrying the current line's indentation, plus one level
    /// when the line opens a block.
    pub fn insert_newline(&mut self) {
        self.history.begin();
        self.insert_newline_inner();
        self.history.end(self.cursor);
    }

    fn insert_newline_inner(&mut self) {
        self.delete_selection();
        let at = self.cursor.position;
        let line = self.buffer.line(at.line);
        let indent = self.buffer.indent_of(at.line);
        let before_cursor: String = line.chars().take(at.col).collect();
        let after_cursor: String = line.chars().skip(at.col).collect();

        let opens = before_cursor
            .trim_end()
            .chars()
            .next_back()
            .is_some_and(|c| matches!(c, '{' | '(' | '[' | ':'));
        let closes_next = after_cursor
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '}' | ')' | ']'));

        let mut text = String::from("\n");
        text.push_str(&indent);
        if opens {
            text.push_str(&self.indent.unit());
        }

        // Splitting `{|}` puts the closing brace on a third line, indented back
        // to where the opening line started.
        if opens && closes_next {
            let inner_end = crate::history::advance(at, &text);
            let mut closing = String::from("\n");
            closing.push_str(&indent);
            self.apply_insert(at, &text, false);
            self.apply_insert(inner_end, &closing, false);
            self.cursor.set_position(inner_end, false);
            return;
        }

        self.apply_insert(at, &text, false);
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.position;
        if pos == Position::ZERO {
            return;
        }
        // Inside leading whitespace, one Backspace takes back one indent level
        // rather than one space.
        if let IndentStyle::Spaces(width) = self.indent {
            let width = width.max(1);
            let indent_len = self.buffer.indent_len(pos.line);
            if pos.col > 0 && pos.col <= indent_len && pos.col % width == 0 {
                let target = pos.col - width;
                self.apply_delete(Range::new(
                    Position::new(pos.line, target),
                    Position::new(pos.line, pos.col),
                ));
                return;
            }
        }
        let prev = self.buffer.prev_position(pos);
        self.apply_delete(Range::new(prev, pos));
    }

    pub fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.position;
        let next = self.buffer.next_position(pos);
        if next != pos {
            self.apply_delete(Range::new(pos, next));
        }
    }

    pub fn delete_word_left(&mut self) {
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.position;
        let start = crate::cursor::word_left(&self.buffer, pos);
        if start != pos {
            self.apply_delete(Range::new(start, pos));
        }
    }

    pub fn delete_word_right(&mut self) {
        if self.delete_selection() {
            return;
        }
        let pos = self.cursor.position;
        let end = crate::cursor::word_right(&self.buffer, pos);
        if end != pos {
            self.apply_delete(Range::new(pos, end));
        }
    }

    /// Deletes the whole line the cursor is on (or every line the selection
    /// touches), leaving the cursor at the start of what follows.
    pub fn delete_line(&mut self) {
        self.history.begin();
        let (first, last) = self.selected_lines();
        let start = Position::new(first, 0);
        let end = if last + 1 < self.buffer.line_count() {
            Position::new(last + 1, 0)
        } else {
            Position::new(last, self.buffer.line_len(last))
        };
        // The last line of a file has no terminator of its own to take, so the
        // one above it goes instead and the file does not gain a blank line.
        let start = if last + 1 >= self.buffer.line_count() && first > 0 {
            Position::new(first - 1, self.buffer.line_len(first - 1))
        } else {
            start
        };
        self.apply_delete(Range::new(start, end));
        self.history.end(self.cursor);
    }

    /// Duplicates the cursor's line (or the selected lines) below itself.
    pub fn duplicate_line(&mut self) {
        self.history.begin();
        let (first, last) = self.selected_lines();
        let mut text = String::from("\n");
        for line in first..=last {
            text.push_str(self.buffer.line(line));
            if line != last {
                text.push('\n');
            }
        }
        let at = Position::new(last, self.buffer.line_len(last));
        let cursor = self.cursor;
        self.apply_insert(at, &text, false);
        let moved = last - first + 1;
        self.cursor = Cursor::at(self.buffer.clamp(Position::new(
            cursor.position.line + moved,
            cursor.position.col,
        )));
        self.history.end(self.cursor);
    }

    /// Moves the cursor's line (or the selected lines) one line up or down,
    /// carrying the selection with it.
    pub fn move_lines(&mut self, down: bool) {
        self.history.begin();
        self.move_lines_inner(down);
        self.history.end(self.cursor);
    }

    fn move_lines_inner(&mut self, down: bool) {
        let (first, last) = self.selected_lines();
        if down && last + 1 >= self.buffer.line_count() {
            return;
        }
        if !down && first == 0 {
            return;
        }

        let block: Vec<String> = (first..=last)
            .map(|l| String::from(self.buffer.line(l)))
            .collect();
        let other = if down { last + 1 } else { first - 1 };
        let other_text = String::from(self.buffer.line(other));

        let (new_first, rewritten) = if down {
            let mut lines = alloc::vec![other_text];
            lines.extend(block);
            (first, lines)
        } else {
            let mut lines = block;
            lines.push(other_text);
            (first - 1, lines)
        };

        let span_start = Position::new(new_first, 0);
        let span_end_line = new_first + rewritten.len() - 1;
        let span_end = Position::new(span_end_line, self.buffer.line_len(span_end_line));
        let joined = rewritten.join("\n");

        let cursor_before = self.cursor;
        self.apply_delete(Range::new(span_start, span_end));
        self.apply_insert(span_start, &joined, false);

        let delta = if down { 1isize } else { -1 };
        // A shifted position must land inside the buffer: a whole-line
        // selection ending at the start of the line below the block shifts to
        // one line past the end, and an out-of-range anchor would paint a
        // selection wider than the one the next edit deletes.
        let shift = |buffer: &TextBuffer, pos: Position| -> Position {
            let line = (pos.line as isize + delta).max(0) as usize;
            if line >= buffer.line_count() {
                buffer.end_position()
            } else {
                Position::new(line, pos.col.min(buffer.line_len(line)))
            }
        };
        let position = shift(&self.buffer, cursor_before.position);
        let anchor = cursor_before.anchor.map(|a| shift(&self.buffer, a));
        self.cursor = Cursor {
            position,
            anchor,
            goal_col: None,
        };
    }

    /// Tab: one indent level, or an indent of every selected line.
    pub fn indent_selection(&mut self) {
        if self.cursor.has_selection() {
            self.shift_lines(true);
            return;
        }
        let unit = self.indent.unit();
        self.insert_text(&unit);
        self.history.seal();
    }

    /// Shift+Tab: one indent level off the front of every selected line.
    pub fn outdent(&mut self) {
        self.shift_lines(false);
    }

    fn shift_lines(&mut self, add: bool) {
        self.history.begin();
        let (first, last) = self.selected_lines();
        let unit = self.indent.unit();
        let width = self.indent.width();
        let cursor_before = self.cursor;
        let mut first_delta = 0isize;
        let mut last_delta = 0isize;

        for line in first..=last {
            let text = self.buffer.line(line);
            if add {
                if text.is_empty() {
                    continue;
                }
                self.apply_insert(Position::new(line, 0), &unit, false);
                let delta = unit.chars().count() as isize;
                if line == first {
                    first_delta = delta;
                }
                if line == last {
                    last_delta = delta;
                }
            } else {
                let removable = match self.indent {
                    IndentStyle::Tabs(_) => {
                        if text.starts_with('\t') {
                            1
                        } else {
                            text.chars().take(width).take_while(|c| *c == ' ').count()
                        }
                    }
                    IndentStyle::Spaces(_) => {
                        if text.starts_with('\t') {
                            1
                        } else {
                            text.chars().take(width).take_while(|c| *c == ' ').count()
                        }
                    }
                };
                if removable == 0 {
                    continue;
                }
                self.apply_delete(Range::new(
                    Position::new(line, 0),
                    Position::new(line, removable),
                ));
                if line == first {
                    first_delta = -(removable as isize);
                }
                if line == last {
                    last_delta = -(removable as isize);
                }
            }
        }

        // Keep the same text selected: only the columns on the first and last
        // lines moved, and only by what was added or removed there.
        let adjust = |pos: Position| -> Position {
            let delta = if pos.line == first {
                first_delta
            } else if pos.line == last {
                last_delta
            } else {
                0
            };
            Position::new(pos.line, (pos.col as isize + delta).max(0) as usize)
        };
        self.cursor = Cursor {
            position: self.buffer.clamp(adjust(cursor_before.position)),
            anchor: cursor_before.anchor.map(|a| self.buffer.clamp(adjust(a))),
            goal_col: None,
        };
        self.history.end(self.cursor);
    }

    /// Comments the selected lines, or uncomments them when they all already
    /// are — the rule every editor uses, and the only one that round-trips.
    pub fn toggle_comment(&mut self) {
        let Some(token) = self.language.line_comment() else {
            return;
        };
        let (first, last) = self.selected_lines();
        let non_blank: Vec<usize> = (first..=last)
            .filter(|l| !self.buffer.line(*l).trim().is_empty())
            .collect();
        if non_blank.is_empty() {
            return;
        }
        // Opened only once there is something to do: a `begin` without its
        // `end` would leave every later edit merging into one undo group.
        self.history.begin();
        let all_commented = non_blank
            .iter()
            .all(|l| self.buffer.line(*l).trim_start().starts_with(token));

        let cursor_before = self.cursor;
        for line in non_blank {
            let text = self.buffer.line(line);
            let indent = self.buffer.indent_len(line);
            if all_commented {
                let rest: String = text.chars().skip(indent).collect();
                let stripped = rest.strip_prefix(token).unwrap_or(&rest);
                let stripped = stripped.strip_prefix(' ').unwrap_or(stripped);
                let removed = rest.chars().count() - stripped.chars().count();
                self.apply_delete(Range::new(
                    Position::new(line, indent),
                    Position::new(line, indent + removed),
                ));
            } else {
                let mut insert = String::from(token);
                insert.push(' ');
                self.apply_insert(Position::new(line, indent), &insert, false);
            }
        }
        self.cursor = Cursor {
            position: self.buffer.clamp(cursor_before.position),
            anchor: cursor_before.anchor.map(|a| self.buffer.clamp(a)),
            goal_col: None,
        };
        self.history.end(self.cursor);
    }

    /// Replaces every match of `needle` with `replacement`, as one undoable
    /// step; returns how many were replaced.
    ///
    /// Back to front, so an earlier replacement cannot move a later match.
    pub fn replace_all(
        &mut self,
        needle: &str,
        replacement: &str,
        options: crate::search::SearchOptions,
    ) -> usize {
        let matches = crate::search::find_all(&self.buffer, needle, options);
        if matches.is_empty() {
            return 0;
        }
        self.history.begin();
        for range in matches.iter().rev() {
            self.apply_delete(*range);
            self.apply_insert(range.start, replacement, false);
        }
        self.history.end(self.cursor);
        matches.len()
    }

    /// Replaces `range` with `text` as one undoable step.
    pub fn replace_range(&mut self, range: Range, text: &str) {
        self.history.begin();
        self.apply_delete(range);
        self.apply_insert(range.start, text, false);
        self.history.end(self.cursor);
    }

    pub fn undo(&mut self) -> bool {
        match self.history.undo(&mut self.buffer) {
            Some(cursor) => {
                self.cursor = Cursor {
                    position: self.buffer.clamp(cursor.position),
                    anchor: cursor.anchor.map(|a| self.buffer.clamp(a)),
                    goal_col: None,
                };
                self.invalidate_from(0);
                true
            }
            None => false,
        }
    }

    pub fn redo(&mut self) -> bool {
        match self.history.redo(&mut self.buffer) {
            Some(cursor) => {
                self.cursor = Cursor {
                    position: self.buffer.clamp(cursor.position),
                    anchor: cursor.anchor.map(|a| self.buffer.clamp(a)),
                    goal_col: None,
                };
                self.invalidate_from(0);
                true
            }
            None => false,
        }
    }

    // ── motion and selection ────────────────────────────────────────────────

    pub fn move_cursor(&mut self, motion: Motion, extend: bool) {
        self.cursor.apply(&self.buffer, motion, extend);
        self.history.seal();
    }

    pub fn place_cursor(&mut self, pos: Position, extend: bool) {
        let pos = self.buffer.clamp(pos);
        self.cursor.set_position(pos, extend);
        self.history.seal();
    }

    pub fn select_all(&mut self) {
        self.cursor.anchor = Some(Position::ZERO);
        self.cursor.position = self.buffer.end_position();
        self.cursor.goal_col = None;
    }

    pub fn select_word_at(&mut self, pos: Position) {
        let range = word_at(&self.buffer, self.buffer.clamp(pos));
        self.cursor.anchor = Some(range.start);
        self.cursor.position = range.end;
        self.cursor.goal_col = None;
    }

    pub fn select_line_at(&mut self, line: usize) {
        let range = line_range(&self.buffer, line.min(self.buffer.line_count() - 1));
        self.cursor.anchor = Some(range.start);
        self.cursor.position = range.end;
        self.cursor.goal_col = None;
    }

    pub fn selected_text(&self) -> Option<String> {
        self.cursor.selection().map(|r| self.buffer.slice(r))
    }

    /// The line the cursor is on, or the span of lines the selection touches.
    ///
    /// A selection that ends at column 0 does not count that line: selecting
    /// three whole lines leaves the cursor at the start of the fourth, and
    /// commenting it too would surprise.
    pub fn selected_lines(&self) -> (usize, usize) {
        match self.cursor.selection() {
            Some(range) => {
                let last = if range.end.col == 0 && range.end.line > range.start.line {
                    range.end.line - 1
                } else {
                    range.end.line
                };
                (range.start.line, last.min(self.buffer.line_count() - 1))
            }
            None => {
                let line = self.cursor.position.line.min(self.buffer.line_count() - 1);
                (line, line)
            }
        }
    }

    pub fn goto_line(&mut self, line_1_based: usize) {
        let line = line_1_based
            .saturating_sub(1)
            .min(self.buffer.line_count() - 1);
        let col = self.buffer.indent_len(line);
        self.cursor.set_position(Position::new(line, col), false);
    }

    /// Scrolls the viewport so the cursor is inside it, with `margin` lines of
    /// context above and below where the buffer allows.
    pub fn scroll_to_cursor(&mut self, margin: usize) {
        let height = self.viewport.visible_lines.max(1);
        let line = self.cursor.position.line;
        let margin = margin.min(height.saturating_sub(1) / 2);

        if line < self.viewport.first_line + margin {
            self.viewport.first_line = line.saturating_sub(margin);
        } else if line + margin >= self.viewport.first_line + height {
            self.viewport.first_line = line + margin + 1 - height;
        }
        let max_first = self.buffer.line_count().saturating_sub(1);
        self.viewport.first_line = self.viewport.first_line.min(max_first);

        let width = self.viewport.visible_cols.max(1);
        let col = crate::buffer::display_col(
            self.buffer.line(line),
            self.cursor.position.col,
            self.indent.width(),
        );
        if col < self.viewport.first_col {
            self.viewport.first_col = col;
        } else if col >= self.viewport.first_col + width {
            self.viewport.first_col = col + 1 - width;
        }
    }

    /// Scrolls by `delta` lines without moving the cursor.
    pub fn scroll_by(&mut self, delta: isize) {
        let max_first = self.buffer.line_count().saturating_sub(1);
        let first = (self.viewport.first_line as isize + delta).max(0) as usize;
        self.viewport.first_line = first.min(max_first);
    }
}

/// The file name part of a path, or the whole of it when it has no separator.
pub fn file_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => {
            if trimmed.is_empty() {
                "/"
            } else {
                trimmed
            }
        }
    }
}

/// The directory part of a path: everything before the last separator.
pub fn parent_dir(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/",
        Some((dir, _)) => dir,
        None => "",
    }
}
