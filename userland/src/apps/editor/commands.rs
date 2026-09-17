//! Every action the editor can take, named once.
//!
//! A command is the single spelling of an action: the menus list them, the
//! palette searches them, the keymap resolves to them and `update` executes
//! them. Adding a menu entry therefore cannot produce an action the palette
//! cannot reach, and a shortcut cannot drift from the label that documents it.

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
    /// The palette's label for this command, and the menus'.
    pub fn label(&self) -> &'static str {
        match self {
            Command::NewFile => "New File",
            Command::OpenFile => "Open File…",
            Command::OpenFolder => "Open Folder…",
            Command::Save => "Save",
            Command::SaveAs => "Save As…",
            Command::CloseTab => "Close Tab",
            Command::Quit => "Quit",
            Command::Undo => "Undo",
            Command::Redo => "Redo",
            Command::Cut => "Cut",
            Command::Copy => "Copy",
            Command::Paste => "Paste",
            Command::SelectAll => "Select All",
            Command::SelectLine => "Select Line",
            Command::DuplicateLine => "Duplicate Line",
            Command::DeleteLine => "Delete Line",
            Command::MoveLineUp => "Move Line Up",
            Command::MoveLineDown => "Move Line Down",
            Command::ToggleComment => "Toggle Comment",
            Command::Indent => "Indent",
            Command::Outdent => "Outdent",
            Command::Find => "Find",
            Command::Replace => "Replace",
            Command::FindNext => "Find Next",
            Command::FindPrev => "Find Previous",
            Command::ReplaceAll => "Replace All",
            Command::GotoLine => "Go to Line…",
            Command::FileFinder => "Go to File…",
            Command::CommandPalette => "Command Palette…",
            Command::NextTab => "Next Tab",
            Command::PrevTab => "Previous Tab",
            Command::ToggleSidebar => "Toggle Sidebar",
            Command::ToggleLineNumbers => "Toggle Line Numbers",
            Command::RevealInSidebar => "Reveal in Sidebar",
            Command::KeyboardShortcuts => "Keyboard Shortcuts",
            Command::About => "About Sloped",
        }
    }

    /// The chord shown beside the label. One table, so the menu, the palette and
    /// the keymap cannot disagree about what a shortcut is.
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

/// Commands the palette offers, in the order it lists them unfiltered.
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

/// The command a menu item resolves to: `(menu, item)` indexes the tables above.
pub fn menu_command(menu: usize, item: usize) -> Option<Command> {
    match MENUS.get(menu)?.entries.get(item)? {
        MenuEntry::Item(command) => Some(*command),
        MenuEntry::Separator => None,
    }
}
