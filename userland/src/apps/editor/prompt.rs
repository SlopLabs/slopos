//! The one-line prompts: find, replace, go-to-line, open, save-as, the command
//! palette and the file finder.
//!
//! All of them are the same two things — a line of text with a caret, and a list
//! the text filters — so they are one state machine rather than seven.

use slopos_appkit::{Key, Modifiers, NamedKey};

use crate::apps::editor::commands::Command;

/// Text and caret for one input line. The widget draws it and reports keys;
/// this is where they take effect.
#[derive(Clone, Debug, Default)]
pub struct LineInput {
    pub text: String,
    /// Caret position in characters.
    pub caret: usize,
}

impl LineInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(text: String) -> Self {
        let caret = text.chars().count();
        Self { text, caret }
    }

    fn byte_of(&self, col: usize) -> usize {
        self.text
            .char_indices()
            .nth(col)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, c: char) {
        let at = self.byte_of(self.caret);
        self.text.insert(at, c);
        self.caret += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' || c == '\r' {
                continue;
            }
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        let end = self.byte_of(self.caret);
        let start = self.byte_of(self.caret - 1);
        self.text.replace_range(start..end, "");
        self.caret -= 1;
    }

    pub fn delete(&mut self) {
        if self.caret >= self.len() {
            return;
        }
        let start = self.byte_of(self.caret);
        let end = self.byte_of(self.caret + 1);
        self.text.replace_range(start..end, "");
    }

    pub fn set_caret(&mut self, col: usize) {
        self.caret = col.min(self.len());
    }

    pub fn left(&mut self) {
        self.caret = self.caret.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.caret = (self.caret + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.caret = self.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.caret = 0;
    }

    /// Deletes back to the previous word boundary, as Ctrl+Backspace does
    /// everywhere else.
    ///
    /// A separator counts as a boundary, not just whitespace: what this line
    /// most often holds is a path, and a whitespace-only rule makes one
    /// Ctrl+Backspace erase the whole of it rather than the last segment.
    pub fn delete_word_left(&mut self) {
        let boundary = |c: char| c.is_whitespace() || c == '/';
        let chars: Vec<char> = self.text.chars().collect();
        let mut start = self.caret;
        while start > 0 && boundary(chars[start - 1]) {
            start -= 1;
        }
        while start > 0 && !boundary(chars[start - 1]) {
            start -= 1;
        }
        let end_byte = self.byte_of(self.caret);
        let start_byte = self.byte_of(start);
        self.text.replace_range(start_byte..end_byte, "");
        self.caret = start;
    }

    /// Applies a key that edits or moves within the line. Returns false for a
    /// key the caller should interpret itself (Enter, Escape, Up, Down, Tab).
    pub fn handle_key(&mut self, key: Key, mods: Modifiers) -> bool {
        match key {
            Key::Named(NamedKey::Space) if !mods.ctrl && !mods.plain_alt() => {
                self.insert(' ');
                true
            }
            Key::Named(NamedKey::Backspace) => {
                if mods.ctrl {
                    self.delete_word_left();
                } else {
                    self.backspace();
                }
                true
            }
            Key::Named(NamedKey::Delete) => {
                self.delete();
                true
            }
            Key::Named(NamedKey::Left) => {
                self.left();
                true
            }
            Key::Named(NamedKey::Right) => {
                self.right();
                true
            }
            Key::Named(NamedKey::Home) => {
                self.home();
                true
            }
            Key::Named(NamedKey::End) => {
                self.end();
                true
            }
            Key::Char('a') if mods.ctrl => {
                self.home();
                true
            }
            Key::Char('e') if mods.ctrl => {
                self.end();
                true
            }
            Key::Char('u') if mods.ctrl => {
                self.clear();
                true
            }
            _ => false,
        }
    }
}

/// Which prompt is open, and what it is collecting.
#[derive(Clone, Debug)]
pub enum Prompt {
    None,
    /// The find bar, with an optional replacement field beside it.
    Find {
        query: LineInput,
        replace: Option<LineInput>,
        /// Which of the two fields the keyboard is in.
        on_replace: bool,
        /// Matches in the active document, refreshed as the query changes.
        total: usize,
        current: usize,
    },
    Goto {
        input: LineInput,
    },
    /// Open a file by path.
    OpenPath {
        input: LineInput,
    },
    /// Open a directory as the sidebar's root.
    OpenFolder {
        input: LineInput,
    },
    SaveAs {
        input: LineInput,
        /// Which document is being saved. Without it the prompt would write
        /// whichever tab happened to be active when Enter was pressed, which is
        /// not the one that asked.
        index: usize,
    },
    /// Fuzzy file finder over the tree.
    FileFinder {
        input: LineInput,
        results: Vec<String>,
        selected: usize,
    },
    /// Fuzzy command palette.
    Palette {
        input: LineInput,
        results: Vec<Command>,
        selected: usize,
    },
}

impl Prompt {
    pub fn is_open(&self) -> bool {
        !matches!(self, Prompt::None)
    }

    /// The input the keyboard is currently in.
    pub fn input_mut(&mut self) -> Option<&mut LineInput> {
        match self {
            Prompt::None => None,
            Prompt::Find {
                query,
                replace,
                on_replace,
                ..
            } => {
                if *on_replace {
                    replace.as_mut()
                } else {
                    Some(query)
                }
            }
            Prompt::Goto { input }
            | Prompt::OpenPath { input }
            | Prompt::OpenFolder { input }
            | Prompt::SaveAs { input, .. }
            | Prompt::FileFinder { input, .. }
            | Prompt::Palette { input, .. } => Some(input),
        }
    }

    /// The text and caret of the active input, for the widget that draws it.
    pub fn input_text(&self) -> Option<(String, usize)> {
        match self {
            Prompt::None => None,
            Prompt::Find {
                query,
                replace,
                on_replace,
                ..
            } => {
                let line = if *on_replace {
                    replace.as_ref()?
                } else {
                    query
                };
                Some((line.text.clone(), line.caret))
            }
            Prompt::Goto { input }
            | Prompt::OpenPath { input }
            | Prompt::OpenFolder { input }
            | Prompt::SaveAs { input, .. }
            | Prompt::FileFinder { input, .. }
            | Prompt::Palette { input, .. } => Some((input.text.clone(), input.caret)),
        }
    }

    /// Whether this prompt renders as a centred overlay rather than a bar.
    pub fn is_overlay(&self) -> bool {
        matches!(self, Prompt::FileFinder { .. } | Prompt::Palette { .. })
    }

    pub fn title(&self) -> &'static str {
        match self {
            Prompt::None => "",
            Prompt::Find { .. } => "Find",
            Prompt::Goto { .. } => "Go to line",
            Prompt::OpenPath { .. } => "Open file",
            Prompt::OpenFolder { .. } => "Open folder",
            Prompt::SaveAs { .. } => "Save as",
            Prompt::FileFinder { .. } => "Go to file",
            Prompt::Palette { .. } => "Command",
        }
    }

    pub fn placeholder(&self) -> &'static str {
        match self {
            Prompt::None => "",
            Prompt::Find { .. } => "Find in file",
            Prompt::Goto { .. } => "Line number",
            Prompt::OpenPath { .. } => "Path to open",
            Prompt::OpenFolder { .. } => "Folder to open",
            Prompt::SaveAs { .. } => "Path to save to",
            Prompt::FileFinder { .. } => "File name",
            Prompt::Palette { .. } => "Command name",
        }
    }
}
