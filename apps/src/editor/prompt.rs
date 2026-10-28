//! The prompts, one state machine: a line of text with a caret, and for the
//! finder and the palette a list the text filters.

use slopos_appkit::LineInput;

use crate::editor::commands::Command;

#[derive(Clone, Debug)]
pub enum Prompt {
    None,
    Find {
        query: LineInput,
        replace: Option<LineInput>,
        on_replace: bool,
        total: usize,
        current: usize,
    },
    Goto {
        input: LineInput,
    },
    OpenPath {
        input: LineInput,
    },
    OpenFolder {
        input: LineInput,
    },
    SaveAs {
        input: LineInput,
        /// The tab that asked, which by Enter need not be the active one.
        index: usize,
    },
    FileFinder {
        input: LineInput,
        results: Vec<String>,
        selected: usize,
    },
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

    pub fn is_overlay(&self) -> bool {
        matches!(self, Prompt::FileFinder { .. } | Prompt::Palette { .. })
    }

    /// The finder's or the palette's chosen row, and how many it has.
    pub fn overlay_mut(&mut self) -> Option<(&mut usize, usize)> {
        match self {
            Prompt::FileFinder {
                results, selected, ..
            } => Some((selected, results.len())),
            Prompt::Palette {
                results, selected, ..
            } => Some((selected, results.len())),
            _ => None,
        }
    }

    /// The bar's title, for the prompts that ask a question.
    pub fn title(&self) -> &'static str {
        match self {
            Prompt::Goto { .. } => "Go to line",
            Prompt::OpenPath { .. } => "Open file",
            Prompt::OpenFolder { .. } => "Open folder",
            Prompt::SaveAs { .. } => "Save as",
            Prompt::None
            | Prompt::Find { .. }
            | Prompt::FileFinder { .. }
            | Prompt::Palette { .. } => "",
        }
    }

    pub fn placeholder(&self) -> &'static str {
        match self {
            Prompt::Goto { .. } => "Line number",
            Prompt::OpenPath { .. } => "Path to open",
            Prompt::OpenFolder { .. } => "Folder to open",
            Prompt::SaveAs { .. } => "Path to save to",
            Prompt::FileFinder { .. } => "File name",
            Prompt::Palette { .. } => "Command name",
            Prompt::None | Prompt::Find { .. } => "",
        }
    }
}
