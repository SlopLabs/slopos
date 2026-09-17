//! Undo and redo, as groups of primitive changes.
//!
//! A group is what one Ctrl+Z takes back. Typing coalesces into one group while
//! it stays a run of single characters advancing from the last one; anything
//! else — a motion, a delete, a paste, a save — seals the group, so undo lands
//! on boundaries a person recognizes rather than on keystrokes.

use alloc::string::String;
use alloc::vec::Vec;

use crate::buffer::{Position, Range, TextBuffer};
use crate::cursor::Cursor;

/// Undo groups kept per document. Past it the oldest group is dropped, which
/// bounds a long session's memory at roughly the edited text rather than the
/// whole editing history.
pub const MAX_UNDO_GROUPS: usize = 500;

#[derive(Clone, Debug)]
pub enum Change {
    Insert { at: Position, text: String },
    Delete { range: Range, text: String },
}

impl Change {
    fn apply(&self, buffer: &mut TextBuffer) {
        match self {
            Change::Insert { at, text } => {
                buffer.insert(*at, text);
            }
            Change::Delete { range, .. } => {
                buffer.delete(*range);
            }
        }
    }

    fn inverse(&self) -> Change {
        match self {
            Change::Insert { at, text } => {
                let end = advance(*at, text);
                Change::Delete {
                    range: Range::new(*at, end),
                    text: text.clone(),
                }
            }
            Change::Delete { range, text } => Change::Insert {
                at: range.start,
                text: text.clone(),
            },
        }
    }
}

/// The position reached by inserting `text` at `at`.
pub fn advance(at: Position, text: &str) -> Position {
    let normalized = text.replace("\r\n", "\n");
    let newlines = normalized.matches('\n').count();
    if newlines == 0 {
        Position::new(at.line, at.col + normalized.chars().count())
    } else {
        let last = normalized.rsplit('\n').next().unwrap_or("");
        Position::new(at.line + newlines, last.chars().count())
    }
}

#[derive(Clone, Debug)]
struct Group {
    changes: Vec<Change>,
    cursor_before: Cursor,
    cursor_after: Cursor,
    /// Coalescing only continues while this is set and the next insert starts
    /// exactly where the last one ended.
    open: bool,
    /// End of the last insert in this group, for that test.
    tip: Option<Position>,
}

#[derive(Default)]
pub struct History {
    undo: Vec<Group>,
    redo: Vec<Group>,
    /// Nesting depth of [`History::begin`]/[`History::end`].
    transaction: usize,
    /// The group changes are landing in while a transaction is open.
    transaction_group: Option<usize>,
}

impl History {
    pub fn new() -> Self {
        Self::default()
    }

    /// Undo steps held, for the cap's own test.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Opens a transaction: every change recorded until the matching
    /// [`History::end`] is one undo step, whatever it is made of. A selection
    /// replaced by a paste is a delete and an insert, and one Ctrl+Z has to take
    /// back both.
    pub fn begin(&mut self) {
        self.transaction += 1;
    }

    /// Closes the transaction, recording `cursor_after` as where the whole
    /// operation leaves the caret.
    ///
    /// Taken here rather than left to each caller because every compound
    /// operation restores the cursor *after* its last primitive change, and a
    /// group that kept that change's own end position redoes to the wrong
    /// place — which is not cosmetic, because the next keystroke reads it: a
    /// redone "move line up" followed by another one moves a different line.
    pub fn end(&mut self, cursor_after: Cursor) {
        self.transaction = self.transaction.saturating_sub(1);
        if self.transaction == 0 {
            // Only the group this transaction actually created. A transaction
            // that recorded nothing must not restate some earlier group's
            // cursor.
            if let Some(group) = self
                .transaction_group
                .and_then(|index| self.undo.get_mut(index))
            {
                group.cursor_after = cursor_after;
            }
            self.transaction_group = None;
            self.seal();
            // `record` cannot trim mid-transaction, so a transactional edit is
            // never bounded by it — and `insert_newline` is transactional, so
            // ordinary typing grew the history without limit. The end of the
            // transaction is where the cap applies.
            self.trim();
        }
    }

    /// Seals the open group, so the next edit starts a new one.
    pub fn seal(&mut self) {
        if self.transaction > 0 {
            return;
        }
        if let Some(group) = self.undo.last_mut() {
            group.open = false;
            group.tip = None;
        }
    }

    /// Records `change`, merging into the open group when `coalesce` allows it.
    pub fn record(
        &mut self,
        change: Change,
        cursor_before: Cursor,
        cursor_after: Cursor,
        coalesce: bool,
    ) {
        self.redo.clear();

        if self.transaction > 0 {
            match self.transaction_group {
                Some(index) => {
                    let group = &mut self.undo[index];
                    group.changes.push(change);
                    group.cursor_after = cursor_after;
                    return;
                }
                None => {
                    self.seal_inner();
                    self.undo.push(Group {
                        changes: alloc::vec![change],
                        cursor_before,
                        cursor_after,
                        open: false,
                        tip: None,
                    });
                    self.trim();
                    self.transaction_group = Some(self.undo.len() - 1);
                    return;
                }
            }
        }

        let merged = coalesce
            && match (&change, self.undo.last()) {
                (Change::Insert { at, text }, Some(group))
                    if group.open
                        && group.tip == Some(*at)
                        && !text.contains('\n')
                        && !text.is_empty() =>
                {
                    true
                }
                _ => false,
            };

        if merged {
            let tip = match &change {
                Change::Insert { at, text } => Some(advance(*at, text)),
                Change::Delete { range, .. } => Some(range.start),
            };
            let group = self.undo.last_mut().expect("merged implies a group");
            group.changes.push(change);
            group.cursor_after = cursor_after;
            group.tip = tip;
            return;
        }

        self.seal();
        let tip = match &change {
            Change::Insert { at, text } if coalesce && !text.contains('\n') => {
                Some(advance(*at, text))
            }
            _ => None,
        };
        self.undo.push(Group {
            changes: alloc::vec![change],
            cursor_before,
            cursor_after,
            open: coalesce && tip.is_some(),
            tip,
        });
        self.trim();
    }

    /// Drops the oldest group once the history is full.
    ///
    /// Never while a transaction is open: dropping one shifts every index, and
    /// the transaction's own group would be detached from the changes still
    /// arriving for it.
    fn trim(&mut self) {
        if self.transaction == 0 && self.undo.len() > MAX_UNDO_GROUPS {
            self.undo.remove(0);
        }
    }

    /// [`History::seal`] without the transaction guard, for the transaction path
    /// itself — which has to close the previous group before opening its own.
    fn seal_inner(&mut self) {
        if let Some(group) = self.undo.last_mut() {
            group.open = false;
            group.tip = None;
        }
    }

    /// Reverses the newest group; returns the cursor to restore.
    pub fn undo(&mut self, buffer: &mut TextBuffer) -> Option<Cursor> {
        // Half of a compound edit is not an undo step. Nothing reaches here
        // mid-transaction today; enforcing it means a future compound
        // operation cannot make it so by accident.
        if self.transaction > 0 {
            return None;
        }
        let mut group = self.undo.pop()?;
        group.open = false;
        group.tip = None;
        for change in group.changes.iter().rev() {
            change.inverse().apply(buffer);
        }
        let cursor = group.cursor_before;
        self.redo.push(group);
        Some(cursor)
    }

    /// Replays the newest undone group; returns the cursor to restore.
    pub fn redo(&mut self, buffer: &mut TextBuffer) -> Option<Cursor> {
        if self.transaction > 0 {
            return None;
        }
        let group = self.redo.pop()?;
        for change in group.changes.iter() {
            change.apply(buffer);
        }
        let cursor = group.cursor_after;
        self.undo.push(group);
        Some(cursor)
    }
}
