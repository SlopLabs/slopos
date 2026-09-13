//! Static command buffer management for the shell.

use std::sync::Mutex;

use crate::syscall::UserFsEntry;
use slopos_abi::fs::USER_PATH_MAX;

/// One limit for every source: the interactive editor and the script reader
/// must agree, and neither may be shorter than a path the kernel accepts.
pub const SHELL_LINE_MAX: usize = 2 * USER_PATH_MAX;

/// Headroom: `$VAR` substitution can grow a line past its source length.
pub const EXPAND_BUF_SIZE: usize = 2 * SHELL_LINE_MAX;

static LINE_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

static EXPAND_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Heap-backed: `USER_PATH_MAX` bytes need not be resident before a path is
/// ever built.
static PATH_BUF: Mutex<Vec<u8>> = Mutex::new(Vec::new());

static LIST_ENTRIES: Mutex<[UserFsEntry; 32]> = Mutex::new([UserFsEntry::new(); 32]);

/// Parsed token storage: one byte arena plus a span per token, so neither the
/// number of words on a line nor the length of any one of them is capped.
pub struct ParsedTokens {
    bytes: Vec<u8>,
    spans: Vec<(usize, usize)>,
}

impl ParsedTokens {
    pub const fn new() -> Self {
        Self {
            bytes: Vec::new(),
            spans: Vec::new(),
        }
    }

    pub fn token(&self, idx: usize) -> &[u8] {
        let (start, end) = self.spans[idx];
        &self.bytes[start..end]
    }

    /// Append a token. Returns its index.
    pub fn push_token(&mut self, content: &[u8]) -> usize {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(content);
        self.spans.push((start, self.bytes.len()));
        self.spans.len() - 1
    }

    pub fn count(&self) -> usize {
        self.spans.len()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.spans.clear();
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

pub fn with_expand_buf<R, F: FnOnce(&mut [u8]) -> R>(f: F) -> R {
    let mut buf = EXPAND_BUF.lock().unwrap();
    if buf.len() != EXPAND_BUF_SIZE {
        buf.clear();
        buf.resize(EXPAND_BUF_SIZE, 0);
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
