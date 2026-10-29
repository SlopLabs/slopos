//! What the editor tells the person: a passing status as a label, and when
//! something goes wrong, what happened, then what to do about it, or for a
//! refusal what was refused and why.

use std::io::ErrorKind;
use std::path::Path;

use slopos_appkit::Refusal;
use slopos_appkit::theme::words::Bytes;
use slopos_editor_core::document::file_name;

use super::Overwrite;
use super::commands::Command;
use super::files::MAX_FILE_BYTES;
use crate::refused;

macro_rules! name {
    () => {
        "Editor"
    };
}
pub(super) use name;

pub const NAME: &str = name!();

/// A command as a sentence names it: without its menu entry's ellipsis.
pub fn choose(command: Command) -> &'static str {
    command.label().trim_end_matches('…')
}

pub fn unreadable(path: &str, kind: ErrorKind) -> String {
    let name = file_name(path);
    match kind {
        ErrorKind::NotFound => format!("Couldn't find {name}. Check its name, then open it again."),
        kind if Refusal::is(kind) => refused_open(path, Refusal::reading(Path::new(path))),
        _ => format!("Couldn't read {name}. Try again."),
    }
}

pub fn refused_open(path: &str, refusal: Refusal) -> String {
    refused::to_open(file_name(path), refusal)
}

pub fn unlisted(path: &str, kind: ErrorKind) -> String {
    match kind {
        kind if Refusal::is(kind) => refused_listing(path, Refusal::reading(Path::new(path))),
        _ => format!("Couldn't open the folder {}. Try again.", file_name(path)),
    }
}

pub fn refused_listing(path: &str, refusal: Refusal) -> String {
    refused::to_open(&format!("the folder {}", file_name(path)), refusal)
}

pub fn folder(path: &str) -> String {
    format!(
        "Can't open {} as a file, because it's a folder. Choose {} to open it.",
        file_name(path),
        choose(Command::OpenFolder)
    )
}

pub fn too_large(path: &str) -> String {
    format!(
        "Can't open {}, because {NAME} opens files up to {}. Open a smaller file.",
        file_name(path),
        Bytes(MAX_FILE_BYTES)
    )
}

pub fn not_text(path: &str) -> String {
    format!(
        "Can't open {}, because it isn't a text file. Choose a text file instead.",
        file_name(path)
    )
}

pub fn too_many_lines(path: &str) -> String {
    format!(
        "Can't open {}, because it has more lines than {NAME} can hold. Open a shorter file.",
        file_name(path)
    )
}

pub fn unsaved(path: &str, kind: ErrorKind) -> String {
    let save_as = choose(Command::SaveAs);
    match kind {
        kind if Refusal::is(kind) => refused_save(path, Refusal::changing(Path::new(path))),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => format!(
            "There's no room to save {}. Free some space, then save again.",
            file_name(path)
        ),
        _ => format!(
            "Couldn't save {}. Try again, or choose {save_as} to save it somewhere else.",
            file_name(path)
        ),
    }
}

pub fn refused_save(path: &str, refusal: Refusal) -> String {
    let because = match refusal {
        Refusal::System => "its folder is part of the system",
        Refusal::Extension => "its folder is part of an extension",
        Refusal::ReadOnlyDisk => "its folder is on a disk that can't be written to",
        Refusal::Sealed => "it's locked against changes",
        Refusal::Denied => "you don't have permission to save it in its folder",
    };
    format!(
        "Can't save {}, because {because}. Choose {} to save it somewhere else.",
        file_name(path),
        choose(Command::SaveAs)
    )
}

pub fn open_elsewhere(path: &str) -> String {
    format!(
        "Another tab has {} open. Close that tab, or choose {} to save this one under another name.",
        file_name(path),
        choose(Command::SaveAs)
    )
}

pub const TAB_GONE: &str =
    "That tab was closed before it was saved. Open the file again to go on with it.";

pub fn not_a_folder(path: &str) -> String {
    format!(
        "Can't open {path} as a folder. Choose {} again and type a folder's path.",
        choose(Command::OpenFolder)
    )
}

pub fn line_limit() -> String {
    format!("That would make the file longer than {NAME} can hold. Paste less at a time.")
}

pub fn not_a_line(typed: &str, lines: usize) -> String {
    match typed {
        "" => format!("No line number was typed. Type one from 1 to {lines}."),
        typed => format!("There's no line “{typed}”. Type a number from 1 to {lines}."),
    }
}

pub fn no_manual() -> String {
    format!(
        "The {} page isn't on this computer. Choose {} instead: it lists every command with its keys.",
        Command::KeyboardShortcuts.label().to_lowercase(),
        choose(Command::CommandPalette)
    )
}

pub fn unnamed_left() -> String {
    format!(
        "Some files have no name yet. Save each with {}, then quit.",
        choose(Command::SaveAs)
    )
}

pub fn tree_full(limit: usize) -> String {
    format!(
        "The sidebar shows the first {limit} items. Choose {} to open a smaller folder.",
        choose(Command::OpenFolder)
    )
}

pub fn index_full(folders: usize) -> String {
    format!(
        "The finder looked through the first {folders} folders only. Choose {} to open a smaller folder.",
        choose(Command::OpenFolder)
    )
}

/// The title and body of the question a save asks before it writes over `path`.
pub fn overwrite(path: &str, found: Overwrite) -> (String, String) {
    let name = file_name(path);
    match found {
        Overwrite::Changed => (
            format!("Replace {name} with this tab's copy?"),
            format!("{name} changed since you opened it, and saving replaces those changes."),
        ),
        Overwrite::Gone => (
            format!("Save {name} again?"),
            format!("{name} was moved or deleted since you opened it. Saving puts it back."),
        ),
        Overwrite::Exists => (
            format!("Replace {name}?"),
            String::from("A file with that name already exists. Saving replaces it."),
        ),
    }
}

pub const NOT_SAVED_YET: &str =
    "This file isn't saved yet, so the sidebar can't show it. Save it first.";

pub fn opened(path: &str) -> String {
    format!("Opened {path}")
}

pub fn saved(path: &str) -> String {
    format!("Saved {path}")
}

pub const CUT: &str = "Cut";
pub const COPIED: &str = "Copied";
pub const EMPTY_CLIPBOARD: &str = "The clipboard is empty";
pub const NOTHING_TO_UNDO: &str = "Nothing to undo";
pub const NOTHING_TO_REDO: &str = "Nothing to redo";

pub fn no_comments(language: &str) -> String {
    format!("{language} has no comments to toggle")
}

pub fn no_match(query: &str) -> String {
    format!("No match for “{query}”")
}

pub fn replaced(count: usize) -> String {
    match count {
        1 => String::from("Replaced 1 match"),
        count => format!("Replaced {count} matches"),
    }
}

/// `bar_open`: the find bar shows, so only typing is wanted.
pub fn nothing_to_find(bar_open: bool) -> String {
    match bar_open {
        true => String::from("There's nothing to find yet. Type what to look for."),
        false => format!(
            "There's nothing to find yet. Choose {}, then type what to look for.",
            choose(Command::Find)
        ),
    }
}

/// `bar_open`: the find bar shows with its replacement field.
pub fn nothing_to_replace(bar_open: bool) -> String {
    match bar_open {
        true => String::from("There's nothing to replace yet. Type what to find."),
        false => format!(
            "There's nothing to replace yet. Choose {}, then type what to find and what goes in its place.",
            choose(Command::Replace)
        ),
    }
}

pub fn no_replacement() -> String {
    format!(
        "{} needs what goes in place of each match. Choose {} and type it.",
        choose(Command::ReplaceAll),
        choose(Command::Replace)
    )
}
