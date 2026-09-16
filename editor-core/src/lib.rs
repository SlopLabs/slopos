//! Editor core: everything an editor does that is not drawing or I/O.
//!
//! The buffer, the cursor and its motions, the edit operations with their undo
//! history, literal search, the syntax lexer and the file-tree model live here;
//! the userland app supplies the filesystem, the clipboard and the pixels. That
//! split is what makes this host-testable — `just test-host` runs these tests
//! with no SlopOS in the path — and it is the same split `terminal-core` and
//! `shell-core` already draw.
//!
//! Nothing here allocates a fixed-size buffer on a frame: paths, lines and
//! selections are all `String`/`Vec`, because the kernel's 2 KiB stack rule
//! applies to the whole tree and an editor's natural unit is a whole line.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod buffer;
pub mod cursor;
pub mod document;
pub mod filetree;
pub mod history;
pub mod search;
pub mod syntax;

pub use buffer::{IndentStyle, LineEnding, Position, Range, TextBuffer};
pub use cursor::{Cursor, Motion};
pub use document::{Document, Viewport};
pub use filetree::{DirEntry, FileTree};
pub use search::SearchOptions;
pub use syntax::{Language, Span, TokenKind};

/// Unit tests, compiled into the library so `/bin/editor_test` can run the same
/// cases on the target that `cargo test` runs on the host.
pub mod tests;
