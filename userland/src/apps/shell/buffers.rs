//! Static command buffer management for the shell.

use std::sync::Mutex;

use slopos_shell_core::lexer::{self, Tok};

use crate::syscall::UserFsEntry;
use slopos_abi::fs::USER_PATH_MAX;

/// One limit for every source: the interactive editor and the script reader
/// must agree, and neither may be shorter than a path the kernel accepts.
pub const SHELL_LINE_MAX: usize = 2 * USER_PATH_MAX;

static LINE_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Heap-backed: `USER_PATH_MAX` bytes need not be resident before a path is
/// ever built.
static PATH_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

static LIST_ENTRIES: Mutex<[UserFsEntry; 32]> = Mutex::new([UserFsEntry::new(); 32]);

/// Pre-expanded command words, for a caller that has words rather than a line
/// of text. The bytes are final — no expansion, splitting or globbing — which
/// is what makes `time echo '$HOME'` pass the four characters it was given.
pub struct ParsedTokens {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
    /// Whether each entry was pushed as an operator rather than as a word.
    operators: Vec<bool>,
}

impl ParsedTokens {
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            spans: Vec::new(),
            operators: Vec::new(),
        }
    }

    pub fn token(&self, idx: usize) -> &[u8] {
        let (start, end) = self.spans[idx];
        &self.bytes[start..end]
    }

    /// Append a word, whose bytes are data whatever they are: `push_token(b">")`
    /// passes a one-character argument. Returns its index.
    pub fn push_token(&mut self, content: &[u8]) -> usize {
        self.push(content, false)
    }

    /// Append an operator — `|`, `&&`, `2>` — for a caller building a
    /// pipeline or a redirection out of parts. Returns its index.
    pub fn push_operator(&mut self, content: &[u8]) -> usize {
        self.push(content, true)
    }

    fn push(&mut self, content: &[u8], operator: bool) -> usize {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(content);
        self.spans.push((start, self.bytes.len()));
        self.operators.push(operator);
        self.spans.len() - 1
    }

    pub fn count(&self) -> usize {
        self.spans.len()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
        self.operators.clear();
    }

    /// Re-present these entries as a token stream the parser accepts.
    ///
    /// Only what was pushed through [`ParsedTokens::push_operator`] is lexed;
    /// deciding by lexing every entry made `command echo '>'` a redirection
    /// with no operand.
    pub fn to_syntax_tokens(&self) -> Vec<Tok> {
        (0..self.count())
            .map(|i| {
                let bytes = self.token(i);
                if !self.operators[i] {
                    return Tok::Literal(bytes.to_vec());
                }
                match lexer::lex(bytes) {
                    Ok(lexed) if lexed.len() == 1 && !matches!(lexed[0], Tok::Word(_)) => {
                        lexed.into_iter().next().expect("one token")
                    }
                    _ => Tok::Literal(bytes.to_vec()),
                }
            })
            .collect()
    }
}

pub fn with_line_buf<R, F: FnOnce(&mut [u8]) -> R>(f: F) -> R {
    let mut buf = LINE_BUF.lock().unwrap();
    if buf.len() != SHELL_LINE_MAX {
        buf.clear();
        buf.resize(SHELL_LINE_MAX, 0);
    }
    f(&mut buf)
}

pub fn with_path_buf<R, F: FnOnce(&mut [u8]) -> R>(f: F) -> R {
    let mut buf = PATH_BUF.lock().unwrap();
    if buf.len() != USER_PATH_MAX {
        buf.clear();
        buf.resize(USER_PATH_MAX, 0);
    }
    f(&mut buf)
}

/// For a caller that needs a scratch path while [`with_path_buf`]'s single
/// buffer is already held.
pub fn path_scratch() -> Vec<u8> {
    vec![0u8; USER_PATH_MAX]
}

pub fn with_list_entries<R, F: FnOnce(&mut [UserFsEntry; 32]) -> R>(f: F) -> R {
    f(&mut LIST_ENTRIES.lock().unwrap())
}
