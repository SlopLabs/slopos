//! What the editor tells the person when something goes wrong: what
//! happened, then what to do about it.

use std::io::ErrorKind;

use super::Overwrite;
use super::commands::Command;
use super::files::MAX_FILE_BYTES;

/// The app's name, as its dock entry, its window and its About box give it.
pub const NAME: &str = "Editor";

const MIB: u64 = 1024 * 1024;

fn name(path: &str) -> &str {
    path.rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// A command as a sentence names it, without the ellipsis its menu entry
/// carries.
pub fn choose(command: Command) -> &'static str {
    command.label().trim_end_matches('…')
}

/// A file that could not be read, by why.
pub fn unreadable(path: &str, kind: ErrorKind) -> String {
    let name = name(path);
    match kind {
        ErrorKind::NotFound => format!("Couldn't find {name}. Check its name, then open it again."),
        ErrorKind::PermissionDenied => {
            format!("You can't open {name}. Ask whoever owns it to share it with you.")
        }
        _ => format!("Couldn't read {name}. Try again."),
    }
}

/// A folder of the sidebar that could not be listed, by why.
pub fn unlisted(path: &str, kind: ErrorKind) -> String {
    let name = name(path);
    match kind {
        ErrorKind::PermissionDenied => {
            format!("You can't open the folder {name}. Ask whoever owns it to share it with you.")
        }
        _ => format!("Couldn't open the folder {name}. Try again."),
    }
}

pub fn folder(path: &str) -> String {
    format!(
        "Can't open {} as a file, because it's a folder. Choose {} to open it.",
        name(path),
        choose(Command::OpenFolder)
    )
}

pub fn too_large(path: &str, bytes: u64) -> String {
    format!(
        "Can't open {}, because it's {} MiB and {NAME} opens files up to {} MiB. Open a smaller file.",
        name(path),
        bytes.div_ceil(MIB),
        MAX_FILE_BYTES / MIB
    )
}

pub fn not_text(path: &str) -> String {
    format!(
        "Can't open {}, because it isn't a text file. Choose a text file instead.",
        name(path)
    )
}

pub fn too_many_lines(path: &str) -> String {
    format!(
        "Can't open {}, because it has more lines than {NAME} can hold. Open a shorter file.",
        name(path)
    )
}

/// A save the filesystem refused, by why.
pub fn unsaved(path: &str, kind: ErrorKind) -> String {
    let save_as = choose(Command::SaveAs);
    match kind {
        ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem => format!(
            "You can't save in the folder {} is in. Choose {save_as} to save it somewhere you can.",
            name(path)
        ),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => format!(
            "There's no room to save {}. Free some space, then save again.",
            name(path)
        ),
        _ => format!(
            "Couldn't save {}. Try again, or choose {save_as} to save it somewhere else.",
            name(path)
        ),
    }
}

pub fn open_elsewhere(path: &str) -> String {
    format!(
        "Another tab has {} open. Close that tab, or choose {} to save this one under another name.",
        name(path),
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

/// The question a save asks before it writes over `path`: its title and
/// what follows from saving.
pub fn overwrite(path: &str, found: Overwrite) -> (String, String) {
    let name = name(path);
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
