//! The cursor, the selection it anchors, and every motion either can take.
//!
//! A motion is resolved against the buffer here and nowhere else, so the
//! keyboard, the mouse and the command palette all move the cursor by the same
//! rules.

use crate::buffer::{CharClass, Position, Range, TextBuffer, char_class};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    /// First non-blank column, or column 0 when already there — Home twice
    /// reaches the true start of a line, which is what every editor does.
    LineStart,
    LineEnd,
    PageUp(usize),
    PageDown(usize),
    DocStart,
    DocEnd,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Cursor {
    pub position: Position,
    /// Set while a selection is live; the selection is `anchor..position`.
    pub anchor: Option<Position>,
    /// Column a vertical motion aims for, so moving through a short line and
    /// back out again returns to the column the cursor started in.
    pub goal_col: Option<usize>,
}

impl Cursor {
    pub fn at(position: Position) -> Self {
        Self {
            position,
            anchor: None,
            goal_col: None,
        }
    }

    pub fn selection(&self) -> Option<Range> {
        let anchor = self.anchor?;
        if anchor == self.position {
            return None;
        }
        Some(Range::new(anchor, self.position))
    }

    pub fn has_selection(&self) -> bool {
        self.selection().is_some()
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Starts (or keeps) a selection anchored where the cursor is now.
    pub fn begin_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some(self.position);
        }
    }

    pub fn set_position(&mut self, position: Position, extend: bool) {
        if extend {
            self.begin_selection();
        } else {
            self.anchor = None;
        }
        self.position = position;
        self.goal_col = None;
    }

    /// Applies `motion`, extending the selection when `extend` is set.
    pub fn apply(&mut self, buffer: &TextBuffer, motion: Motion, extend: bool) {
        if extend {
            self.begin_selection();
        }

        // A plain horizontal motion out of a selection collapses to its edge
        // rather than moving one further, which is what a user expects after
        // selecting a word and pressing Left.
        if !extend {
            if let Some(range) = self.selection() {
                match motion {
                    Motion::Left => {
                        self.position = range.start;
                        self.anchor = None;
                        self.goal_col = None;
                        return;
                    }
                    Motion::Right => {
                        self.position = range.end;
                        self.anchor = None;
                        self.goal_col = None;
                        return;
                    }
                    _ => {}
                }
            }
            self.anchor = None;
        }

        let pos = buffer.clamp(self.position);
        let (next, keep_goal) = match motion {
            Motion::Left => (buffer.prev_position(pos), false),
            Motion::Right => (buffer.next_position(pos), false),
            Motion::Up => (self.vertical(buffer, pos, -1), true),
            Motion::Down => (self.vertical(buffer, pos, 1), true),
            Motion::PageUp(rows) => (self.vertical(buffer, pos, -(rows as isize)), true),
            Motion::PageDown(rows) => (self.vertical(buffer, pos, rows as isize), true),
            Motion::WordLeft => (word_left(buffer, pos), false),
            Motion::WordRight => (word_right(buffer, pos), false),
            Motion::LineStart => (line_start(buffer, pos), false),
            Motion::LineEnd => (Position::new(pos.line, buffer.line_len(pos.line)), false),
            Motion::DocStart => (Position::ZERO, false),
            Motion::DocEnd => (buffer.end_position(), false),
        };

        self.position = next;
        if !keep_goal {
            self.goal_col = None;
        }
    }

    fn vertical(&mut self, buffer: &TextBuffer, pos: Position, delta: isize) -> Position {
        let goal = self.goal_col.unwrap_or(pos.col);
        self.goal_col = Some(goal);
        let target = pos.line as isize + delta;
        let line = target.clamp(0, buffer.line_count() as isize - 1) as usize;
        Position::new(line, goal.min(buffer.line_len(line)))
    }
}

/// Home: the first non-blank column, or column zero when already there.
///
/// A toggle in both directions, so a third press comes back — Home from column
/// zero on an indented line is how the caret reaches the text again.
fn line_start(buffer: &TextBuffer, pos: Position) -> Position {
    let indent = buffer.indent_len(pos.line).min(buffer.line_len(pos.line));
    let col = if indent == 0 || pos.col == indent {
        0
    } else {
        indent
    };
    Position::new(pos.line, col)
}

/// The start of the word before `pos`, crossing at most one line break.
pub fn word_left(buffer: &TextBuffer, pos: Position) -> Position {
    if pos.col == 0 {
        return buffer.prev_position(pos);
    }
    let chars: alloc::vec::Vec<char> = buffer.line(pos.line).chars().collect();
    let mut col = pos.col.min(chars.len());

    while col > 0 && char_class(chars[col - 1]) == CharClass::Whitespace {
        col -= 1;
    }
    if col == 0 {
        return Position::new(pos.line, 0);
    }
    let class = char_class(chars[col - 1]);
    while col > 0 && char_class(chars[col - 1]) == class {
        col -= 1;
    }
    Position::new(pos.line, col)
}

/// The end of the word after `pos`, crossing at most one line break.
pub fn word_right(buffer: &TextBuffer, pos: Position) -> Position {
    let chars: alloc::vec::Vec<char> = buffer.line(pos.line).chars().collect();
    if pos.col >= chars.len() {
        return buffer.next_position(pos);
    }
    let mut col = pos.col;

    let class = char_class(chars[col]);
    if class == CharClass::Whitespace {
        while col < chars.len() && char_class(chars[col]) == CharClass::Whitespace {
            col += 1;
        }
        if col < chars.len() {
            let next = char_class(chars[col]);
            while col < chars.len() && char_class(chars[col]) == next {
                col += 1;
            }
        }
    } else {
        while col < chars.len() && char_class(chars[col]) == class {
            col += 1;
        }
        while col < chars.len() && char_class(chars[col]) == CharClass::Whitespace {
            col += 1;
        }
    }
    Position::new(pos.line, col)
}

/// The word under `pos`, for a double-click or "select word".
pub fn word_at(buffer: &TextBuffer, pos: Position) -> Range {
    let chars: alloc::vec::Vec<char> = buffer.line(pos.line).chars().collect();
    if chars.is_empty() {
        return Range::empty(Position::new(pos.line, 0));
    }
    let idx = pos.col.min(chars.len() - 1);
    let class = char_class(chars[idx]);
    if class == CharClass::Whitespace {
        let mut start = idx;
        let mut end = idx;
        while start > 0 && char_class(chars[start - 1]) == CharClass::Whitespace {
            start -= 1;
        }
        while end < chars.len() && char_class(chars[end]) == CharClass::Whitespace {
            end += 1;
        }
        return Range::new(Position::new(pos.line, start), Position::new(pos.line, end));
    }

    let mut start = idx;
    let mut end = idx;
    while start > 0 && char_class(chars[start - 1]) == class {
        start -= 1;
    }
    while end < chars.len() && char_class(chars[end]) == class {
        end += 1;
    }
    Range::new(Position::new(pos.line, start), Position::new(pos.line, end))
}

/// The whole of `line`, including its terminator when one follows.
pub fn line_range(buffer: &TextBuffer, line: usize) -> Range {
    if line + 1 < buffer.line_count() {
        Range::new(Position::new(line, 0), Position::new(line + 1, 0))
    } else {
        Range::new(
            Position::new(line, 0),
            Position::new(line, buffer.line_len(line)),
        )
    }
}
