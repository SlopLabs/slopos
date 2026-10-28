//! Every action the editor can take, named once for the menus, the palette and
//! the keymap.

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Command {
    NewFile,
    OpenFile,
    OpenFolder,
    Save,
    SaveAs,
    CloseTab,
    Quit,

    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    SelectLine,
    DuplicateLine,
    DeleteLine,
    MoveLineUp,
    MoveLineDown,
    ToggleComment,
    Indent,
    Outdent,

    Find,
    Replace,
    FindNext,
    FindPrev,
    ReplaceAll,
    GotoLine,
    FileFinder,
    CommandPalette,

    NextTab,
    PrevTab,
    ToggleSidebar,
    ToggleLineNumbers,
    RevealInSidebar,
    KeyboardShortcuts,
    About,
}

impl Command {
    /// Whether it reads or reopens the prompt showing; any other closes it.
    pub fn reads_prompt(&self) -> bool {
        matches!(
            self,
            Command::Find
                | Command::Replace
                | Command::FindNext
                | Command::FindPrev
                | Command::ReplaceAll
        )
    }

    pub fn label(&self) -> &'static str {
        match self {
            Command::NewFile => "New file",
            Command::OpenFile => "Open file…",
            Command::OpenFolder => "Open folder…",
            Command::Save => "Save",
            Command::SaveAs => "Save as…",
            Command::CloseTab => "Close tab",
            Command::Quit => "Quit",
            Command::Undo => "Undo",
            Command::Redo => "Redo",
            Command::Cut => "Cut",
            Command::Copy => "Copy",
            Command::Paste => "Paste",
            Command::SelectAll => "Select all",
            Command::SelectLine => "Select line",
            Command::DuplicateLine => "Duplicate line",
            Command::DeleteLine => "Delete line",
            Command::MoveLineUp => "Move line up",
            Command::MoveLineDown => "Move line down",
            Command::ToggleComment => "Toggle comment",
            Command::Indent => "Indent",
            Command::Outdent => "Outdent",
            Command::Find => "Find",
            Command::Replace => "Replace",
            Command::FindNext => "Find next",
            Command::FindPrev => "Find previous",
            Command::ReplaceAll => "Replace all",
            Command::GotoLine => "Go to line…",
            Command::FileFinder => "Go to file…",
            Command::CommandPalette => "Command palette…",
            Command::NextTab => "Next tab",
            Command::PrevTab => "Previous tab",
            Command::ToggleSidebar => "Toggle sidebar",
            Command::ToggleLineNumbers => "Toggle line numbers",
            Command::RevealInSidebar => "Reveal in sidebar",
            Command::KeyboardShortcuts => "Keyboard shortcuts",
            Command::About => concat!("About ", super::said::name!()),
        }
    }

    pub fn shortcut(&self) -> Option<&'static str> {
        Some(match self {
            Command::NewFile => "Ctrl+N",
            Command::OpenFile => "Ctrl+O",
            Command::OpenFolder => "Ctrl+Shift+O",
            Command::Save => "Ctrl+S",
            Command::SaveAs => "Ctrl+Shift+S",
            Command::CloseTab => "Ctrl+W",
            Command::Quit => "Ctrl+Q",
            Command::Undo => "Ctrl+Z",
            Command::Redo => "Ctrl+Y",
            Command::Cut => "Ctrl+X",
            Command::Copy => "Ctrl+C",
            Command::Paste => "Ctrl+V",
            Command::SelectAll => "Ctrl+A",
            Command::SelectLine => "Ctrl+L",
            Command::DuplicateLine => "Ctrl+D",
            Command::DeleteLine => "Ctrl+K",
            Command::MoveLineUp => "Alt+Up",
            Command::MoveLineDown => "Alt+Down",
            Command::ToggleComment => "Ctrl+/",
            Command::Find => "Ctrl+F",
            Command::Replace => "Ctrl+H",
            Command::FindNext => "F3",
            Command::FindPrev => "Shift+F3",
            Command::GotoLine => "Ctrl+G",
            Command::FileFinder => "Ctrl+P",
            Command::CommandPalette => "Ctrl+Shift+P",
            Command::NextTab => "Ctrl+Tab",
            Command::PrevTab => "Ctrl+Shift+Tab",
            Command::ToggleSidebar => "Ctrl+B",
            Command::Indent => "Tab",
            Command::Outdent => "Shift+Tab",
            Command::ReplaceAll
            | Command::ToggleLineNumbers
            | Command::RevealInSidebar
            | Command::KeyboardShortcuts
            | Command::About => return None,
        })
    }
}

pub enum MenuEntry {
    Item(Command),
    Separator,
}

pub struct MenuDef {
    pub title: &'static str,
    pub entries: &'static [MenuEntry],
}

pub const MENUS: &[MenuDef] = &[
    MenuDef {
        title: "File",
        entries: &[
            MenuEntry::Item(Command::NewFile),
            MenuEntry::Item(Command::OpenFile),
            MenuEntry::Item(Command::OpenFolder),
            MenuEntry::Separator,
            MenuEntry::Item(Command::Save),
            MenuEntry::Item(Command::SaveAs),
            MenuEntry::Separator,
            MenuEntry::Item(Command::CloseTab),
            MenuEntry::Item(Command::Quit),
        ],
    },
    MenuDef {
        title: "Edit",
        entries: &[
            MenuEntry::Item(Command::Undo),
            MenuEntry::Item(Command::Redo),
            MenuEntry::Separator,
            MenuEntry::Item(Command::Cut),
            MenuEntry::Item(Command::Copy),
            MenuEntry::Item(Command::Paste),
            MenuEntry::Separator,
            MenuEntry::Item(Command::Find),
            MenuEntry::Item(Command::Replace),
            MenuEntry::Item(Command::ToggleComment),
        ],
    },
    MenuDef {
        title: "Selection",
        entries: &[
            MenuEntry::Item(Command::SelectAll),
            MenuEntry::Item(Command::SelectLine),
            MenuEntry::Separator,
            MenuEntry::Item(Command::DuplicateLine),
            MenuEntry::Item(Command::DeleteLine),
            MenuEntry::Item(Command::MoveLineUp),
            MenuEntry::Item(Command::MoveLineDown),
        ],
    },
    MenuDef {
        title: "Go",
        entries: &[
            MenuEntry::Item(Command::FileFinder),
            MenuEntry::Item(Command::GotoLine),
            MenuEntry::Item(Command::CommandPalette),
            MenuEntry::Separator,
            MenuEntry::Item(Command::FindNext),
            MenuEntry::Item(Command::FindPrev),
            MenuEntry::Separator,
            MenuEntry::Item(Command::NextTab),
            MenuEntry::Item(Command::PrevTab),
        ],
    },
    MenuDef {
        title: "View",
        entries: &[
            MenuEntry::Item(Command::ToggleSidebar),
            MenuEntry::Item(Command::ToggleLineNumbers),
            MenuEntry::Item(Command::RevealInSidebar),
        ],
    },
    MenuDef {
        title: "Help",
        entries: &[
            MenuEntry::Item(Command::KeyboardShortcuts),
            MenuEntry::Item(Command::About),
        ],
    },
];

/// In the order the palette lists them unfiltered.
pub const PALETTE_COMMANDS: &[Command] = &[
    Command::Save,
    Command::SaveAs,
    Command::OpenFile,
    Command::OpenFolder,
    Command::NewFile,
    Command::CloseTab,
    Command::FileFinder,
    Command::GotoLine,
    Command::Find,
    Command::Replace,
    Command::ReplaceAll,
    Command::FindNext,
    Command::FindPrev,
    Command::Undo,
    Command::Redo,
    Command::Cut,
    Command::Copy,
    Command::Paste,
    Command::SelectAll,
    Command::SelectLine,
    Command::DuplicateLine,
    Command::DeleteLine,
    Command::MoveLineUp,
    Command::MoveLineDown,
    Command::ToggleComment,
    Command::Indent,
    Command::Outdent,
    Command::NextTab,
    Command::PrevTab,
    Command::ToggleSidebar,
    Command::ToggleLineNumbers,
    Command::RevealInSidebar,
    Command::KeyboardShortcuts,
    Command::About,
    Command::Quit,
];

pub fn menu_command(menu: usize, item: usize) -> Option<Command> {
    match MENUS.get(menu)?.entries.get(item)? {
        MenuEntry::Item(command) => Some(*command),
        MenuEntry::Separator => None,
    }
}
