//! The text buffer: a file as a vector of lines, addressed in characters.
//!
//! A line vector rather than a rope, deliberately. What this edits is source
//! code on a machine whose root filesystem is measured in tens of megabytes: the
//! cost that matters is per-keystroke work inside one line, which is O(line),
//! and a line insert or delete, which is a `Vec` move of `line_count` pointers.
//! A rope buys its logarithm back at a complexity the whole of this crate would
//! have to be tested against; the ceiling that makes the trade safe is stated
//! and enforced at load time by [`MAX_LINES`].
//!
//! Positions are `(line, col)` with **col in characters, not bytes**, because
//! every consumer — the cursor, the renderer's column arithmetic, the selection
//! — thinks in cells. Byte offsets appear only where `String` needs them, and
//! [`TextBuffer::byte_of`] is the single conversion.

use alloc::string::String;
use alloc::vec::Vec;

/// Line ceiling for one buffer. Past it, a load is refused rather than making
/// every line insert a multi-megabyte move.
pub const MAX_LINES: usize = 1_000_000;

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Position {
    pub line: usize,
    /// Character index within the line, never a byte offset.
    pub col: usize,
}

impl Position {
    pub const ZERO: Self = Self { line: 0, col: 0 };

    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

/// An ordered `[start, end)` span of the buffer.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        if start <= end {
            Self { start, end }
        } else {
            Self {
                start: end,
                end: start,
            }
        }
    }

    pub fn empty(at: Position) -> Self {
        Self { start: at, end: at }
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub fn contains(&self, pos: Position) -> bool {
        pos >= self.start && pos < self.end
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(&self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

/// What one Tab press inserts, and what one outdent removes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IndentStyle {
    Spaces(usize),
    Tabs(usize),
}

impl IndentStyle {
    pub fn width(&self) -> usize {
        match self {
            IndentStyle::Spaces(n) | IndentStyle::Tabs(n) => (*n).max(1),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            IndentStyle::Spaces(_) => "Spaces",
            IndentStyle::Tabs(_) => "Tabs",
        }
    }

    /// The text one indent level inserts.
    pub fn unit(&self) -> String {
        match self {
            IndentStyle::Spaces(n) => " ".repeat((*n).max(1)),
            IndentStyle::Tabs(_) => String::from("\t"),
        }
    }
}

/// Display column of character column `col` on `text`, expanding tabs.
///
/// A character column and a display column differ wherever a tab can appear,
/// and the two must not be mixed: the renderer positions a caret and a
/// selection in display columns, while the cursor and every search result are
/// in character columns. This is the one conversion, and
/// [`char_col_from_display`] is its inverse.
pub fn display_col(text: &str, col: usize, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let mut display = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i >= col {
            break;
        }
        if ch == '\t' {
            display += tab - (display % tab);
        } else {
            display += 1;
        }
    }
    // A column past the end of the line keeps counting, so a caret parked past
    // a short line still lands where the goal column says.
    display + col.saturating_sub(text.chars().count())
}

/// Character column nearest display column `display` on `text`.
pub fn char_col_from_display(text: &str, display: usize, tab_width: usize) -> usize {
    let tab = tab_width.max(1);
    let mut current = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let width = if ch == '\t' { tab - (current % tab) } else { 1 };
        if display < current + width {
            return i;
        }
        current += width;
    }
    let len = text.chars().count();
    len + display.saturating_sub(current)
}

impl Default for IndentStyle {
    fn default() -> Self {
        IndentStyle::Spaces(4)
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// More lines than [`MAX_LINES`].
    TooManyLines,
}

pub struct TextBuffer {
    /// Never empty: an empty file is one empty line, which is what makes every
    /// position in the buffer addressable without a special case.
    lines: Vec<String>,
    line_ending: LineEnding,
    /// Whether the file ended with a line terminator. Preserved so saving a file
    /// that had one does not silently drop it, and one that lacked it does not
    /// gain it.
    final_newline: bool,
    /// Bumped by every mutation; the save marker and the highlight cache compare
    /// against it rather than re-hashing the text.
    revision: u64,
}

impl TextBuffer {
    pub fn new() -> Self {
        Self {
            lines: alloc::vec![String::new()],
            line_ending: LineEnding::Lf,
            final_newline: true,
            revision: 0,
        }
    }

    /// Parses `text` into lines, detecting the dominant line ending.
    pub fn from_str(text: &str) -> Result<Self, LoadError> {
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count();
        let line_ending = if crlf > 0 && crlf * 2 >= lf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        };

        let final_newline = text.ends_with('\n');
        let body = if final_newline {
            let trimmed = &text[..text.len() - 1];
            trimmed.strip_suffix('\r').unwrap_or(trimmed)
        } else {
            text
        };

        let mut lines: Vec<String> = Vec::new();
        for line in body.split('\n') {
            if lines.len() >= MAX_LINES {
                return Err(LoadError::TooManyLines);
            }
            lines.push(String::from(line.strip_suffix('\r').unwrap_or(line)));
        }
        if lines.is_empty() {
            lines.push(String::new());
        }

        Ok(Self {
            lines,
            line_ending,
            final_newline,
            revision: 0,
        })
    }

    /// Serializes back to file bytes, restoring the detected line ending.
    pub fn to_text(&self) -> String {
        let eol = self.line_ending.as_str();
        let mut out = String::new();
        for (i, line) in self.lines.iter().enumerate() {
            out.push_str(line);
            if i + 1 < self.lines.len() || self.final_newline {
                out.push_str(eol);
            }
        }
        out
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn set_line_ending(&mut self, ending: LineEnding) {
        self.line_ending = ending;
        self.revision += 1;
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, index: usize) -> &str {
        self.lines.get(index).map(String::as_str).unwrap_or("")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn line_len(&self, index: usize) -> usize {
        self.line(index).chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Total characters, counting one line terminator between lines.
    pub fn char_count(&self) -> usize {
        let newlines = self.lines.len().saturating_sub(1);
        self.lines.iter().map(|l| l.chars().count()).sum::<usize>() + newlines
    }

    pub fn end_position(&self) -> Position {
        let line = self.lines.len() - 1;
        Position::new(line, self.line_len(line))
    }

    /// Byte offset of character column `col` in `line`, clamped to its length.
    pub fn byte_of(&self, line: usize, col: usize) -> usize {
        let text = self.line(line);
        text.char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(text.len())
    }

    /// Nearest real position: a line past the end becomes the last line, a
    /// column past the end of its line becomes that line's length.
    pub fn clamp(&self, pos: Position) -> Position {
        let line = pos.line.min(self.lines.len() - 1);
        Position::new(line, pos.col.min(self.line_len(line)))
    }

    /// Inserts `text` at `pos`, returning the position just past it — or the
    /// unchanged position when nothing was inserted, which the caller must
    /// distinguish, because a refused insert that is recorded in the undo
    /// history is a step that takes back text the buffer never held.
    ///
    /// Embedded newlines split lines; `\r\n` in the inserted text is normalized
    /// to `\n` so a paste from a CRLF source does not leave bare carriage
    /// returns inside a line.
    pub fn insert(&mut self, pos: Position, text: &str) -> Position {
        let pos = self.clamp(pos);
        if text.is_empty() {
            return pos;
        }
        // The ceiling `from_str` enforces holds for a paste too, or a buffer
        // could be taken past it one insert at a time.
        let added = text.matches('\n').count();
        if self.lines.len() + added > MAX_LINES {
            return pos;
        }
        self.revision += 1;

        let split_at = self.byte_of(pos.line, pos.col);
        let tail: String = self.lines[pos.line][split_at..].into();
        self.lines[pos.line].truncate(split_at);

        let normalized = text.replace("\r\n", "\n");
        let mut pieces = normalized.split('\n');
        let first = pieces.next().unwrap_or("");
        self.lines[pos.line].push_str(first);

        let mut end = Position::new(pos.line, pos.col + first.chars().count());
        let rest: Vec<&str> = pieces.collect();
        if !rest.is_empty() {
            let mut inserted: Vec<String> = Vec::with_capacity(rest.len());
            for piece in &rest {
                inserted.push(String::from(*piece));
            }
            end = Position::new(
                pos.line + inserted.len(),
                inserted.last().map(|l| l.chars().count()).unwrap_or(0),
            );
            let at = pos.line + 1;
            self.lines.splice(at..at, inserted);
        }

        let last_line = end.line;
        self.lines[last_line].push_str(&tail);
        end
    }

    /// Removes `range` and returns the text that was there.
    pub fn delete(&mut self, range: Range) -> String {
        let start = self.clamp(range.start);
        let end = self.clamp(range.end);
        if start == end {
            return String::new();
        }
        self.revision += 1;

        let removed = self.slice(Range { start, end });

        let start_byte = self.byte_of(start.line, start.col);
        let end_byte = self.byte_of(end.line, end.col);

        if start.line == end.line {
            self.lines[start.line].replace_range(start_byte..end_byte, "");
        } else {
            let tail: String = self.lines[end.line][end_byte..].into();
            self.lines[start.line].truncate(start_byte);
            self.lines[start.line].push_str(&tail);
            self.lines.drain(start.line + 1..=end.line);
        }
        removed
    }

    /// The text in `range`, with `\n` between lines whatever the file's ending.
    pub fn slice(&self, range: Range) -> String {
        let start = self.clamp(range.start);
        let end = self.clamp(range.end);
        if start >= end {
            return String::new();
        }

        let mut out = String::new();
        if start.line == end.line {
            let text = self.line(start.line);
            let a = self.byte_of(start.line, start.col);
            let b = self.byte_of(start.line, end.col);
            out.push_str(&text[a..b]);
            return out;
        }

        let first = self.line(start.line);
        out.push_str(&first[self.byte_of(start.line, start.col)..]);
        out.push('\n');
        for line in start.line + 1..end.line {
            out.push_str(self.line(line));
            out.push('\n');
        }
        let last = self.line(end.line);
        out.push_str(&last[..self.byte_of(end.line, end.col)]);
        out
    }

    /// Replaces line `index` wholesale; used by indent operations, which rewrite
    /// a line's leading whitespace without disturbing the rest of it.
    pub fn replace_line(&mut self, index: usize, text: String) {
        if index < self.lines.len() {
            self.revision += 1;
            self.lines[index] = text;
        }
    }

    pub fn char_at(&self, pos: Position) -> Option<char> {
        self.line(pos.line).chars().nth(pos.col)
    }

    /// Characters of leading whitespace on `line`.
    pub fn indent_len(&self, line: usize) -> usize {
        self.line(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .count()
    }

    /// The leading whitespace of `line`, to copy onto a new line below it.
    pub fn indent_of(&self, line: usize) -> String {
        self.line(line)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect()
    }

    /// The position one character before `pos`, or `pos` at the buffer start.
    pub fn prev_position(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.col > 0 {
            Position::new(pos.line, pos.col - 1)
        } else if pos.line > 0 {
            Position::new(pos.line - 1, self.line_len(pos.line - 1))
        } else {
            pos
        }
    }

    /// The position one character after `pos`, or `pos` at the buffer end.
    pub fn next_position(&self, pos: Position) -> Position {
        let pos = self.clamp(pos);
        if pos.col < self.line_len(pos.line) {
            Position::new(pos.line, pos.col + 1)
        } else if pos.line + 1 < self.lines.len() {
            Position::new(pos.line + 1, 0)
        } else {
            pos
        }
    }

    /// Infers the file's indentation from the first line that is indented, so
    /// Tab in a tab-indented file inserts a tab.
    pub fn detect_indent(&self, default: IndentStyle) -> IndentStyle {
        // Histogram the *step into* a block, not the absolute indent a line
        // carries. The indents a file shows are multiples of its unit, so in
        // anything with nesting the most common absolute width is not the unit
        // — a four-space file with enough two-deep blocks looks like an
        // eight-space one.
        let mut steps: [usize; 9] = [0; 9];
        let mut tabs = 0usize;
        let mut spaces = 0usize;
        let mut previous: Option<usize> = None;
        for line in self.lines.iter().take(4000) {
            match line.chars().next() {
                Some('\t') => {
                    tabs += 1;
                    previous = None;
                    continue;
                }
                Some(' ') => spaces += 1,
                // A blank line is inside whatever block it sits in, so it does
                // not break the chain.
                None => continue,
                _ => {}
            }
            let n = line.chars().take_while(|c| *c == ' ').count();
            if let Some(previous) = previous {
                let step = n.saturating_sub(previous);
                if (1..=8).contains(&step) {
                    steps[step] += 1;
                }
            }
            previous = Some(n);
        }

        if tabs > spaces {
            return IndentStyle::Tabs(default.width());
        }
        // Ties go to the narrower width: two is a plausible reading of a file
        // that also shows fours, and four is not a plausible reading of one
        // that only shows twos.
        let best = steps
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, count)| **count > 0)
            .max_by_key(|(width, count)| (**count, core::cmp::Reverse(*width)))
            .map(|(width, _)| width);
        match best {
            Some(width) => IndentStyle::Spaces(width),
            None => default,
        }
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Word class of one character, for word-wise motion and double-click select.
///
/// Three classes rather than two: a run of punctuation is its own word, which is
/// what makes Ctrl+Right stop between `foo` and `::` in `foo::bar`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum CharClass {
    Whitespace,
    Word,
    Punctuation,
}

pub fn char_class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}
