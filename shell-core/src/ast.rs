//! The shell's syntax tree.
//!
//! A [`Word`] keeps the bytes the lexer saw and is expanded at execution time,
//! so a loop body re-expands each iteration without re-parsing and quoting
//! needs no separate representation: `"if"` is not the reserved word `if`
//! because its text is not `if`.

use alloc::sync::Arc;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    pub text: Vec<u8>,
    /// Already-final bytes: no expansion, splitting or globbing. Set for a
    /// here-document body with a quoted delimiter, and for pre-expanded words.
    pub literal: bool,
}

impl Word {
    pub fn raw(text: Vec<u8>) -> Self {
        Self {
            text,
            literal: false,
        }
    }

    pub fn literal(text: Vec<u8>) -> Self {
        Self {
            text,
            literal: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedirKind {
    Input,
    InputOutput,
    OutputTruncate,
    OutputAppend,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedirTarget {
    Path(Word),
    /// `>&N` / `<&N`, or `>&-` to close. A word, because `>&"$fd"` is legal.
    Dup(Word),
    /// A here-document body, already assembled by the lexer.
    Here(Word),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    /// The descriptor being redirected, defaulted from the operator.
    pub fd: i32,
    pub kind: RedirKind,
    pub target: RedirTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseItem {
    pub patterns: Vec<Word>,
    pub body: List,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Simple {
        assigns: Vec<Word>,
        words: Vec<Word>,
    },
    /// `( list )` — runs in a forked copy of the shell.
    Subshell(List),
    /// `{ list; }` — runs in this shell.
    Group(List),
    If {
        /// `(condition, body)` for the `if` and each `elif`, in order.
        arms: Vec<(List, List)>,
        otherwise: Option<List>,
    },
    Loop {
        /// `until` inverts the condition; everything else is `while`.
        until: bool,
        cond: List,
        body: List,
    },
    For {
        name: Vec<u8>,
        /// `None` when the `in` clause is absent, which iterates `"$@"`.
        words: Option<Vec<Word>>,
        body: List,
    },
    Case {
        word: Word,
        items: Vec<CaseItem>,
    },
    /// `name() compound` — a command with a status, not a declaration.
    Function {
        name: Vec<u8>,
        body: Arc<Command>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub kind: CommandKind,
    pub redirects: Vec<Redirect>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    /// `!` — the pipeline's status is inverted.
    pub negate: bool,
    pub cmds: Vec<Command>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AndOrOp {
    And,
    Or,
}

/// Left-associative, as POSIX specifies: `a && b || c` is `(a && b) || c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AndOr {
    pub first: Pipeline,
    pub rest: Vec<(AndOrOp, Pipeline)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    pub andor: AndOr,
    /// Terminated by `&` rather than `;` or a newline.
    pub background: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct List {
    pub items: Vec<ListItem>,
}

impl List {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
