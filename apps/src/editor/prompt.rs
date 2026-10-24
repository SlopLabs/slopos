//! The one-line prompts: find, replace, go-to-line, open, save-as, the command
//! palette and the file finder.
//!
//! All of them are the same two things — a line of text with a caret, and a list
//! the text filters — so they are one state machine rather than seven.

use slopos_appkit::LineInput;

use crate::editor::commands::Command;

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
