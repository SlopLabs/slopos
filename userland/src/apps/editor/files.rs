//! The editor's filesystem boundary.
//!
//! Every read and write the editor does is here, so a failure is a message on
//! the status bar rather than a panic in a message handler, and so
//! `editor-core` stays free of `std::fs`.

use std::fs;
use std::io::Write;

use slopos_editor_core::filetree::DirEntry;

/// Largest file the editor opens. Past it the buffer's per-line `String` vector
/// is no longer the right shape and the read alone would hold the UI; refusing
/// says so instead of appearing to hang.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// The editor's own documentation, as the image installs it.
pub const DOC_PATH: &str = "/usr/share/slopos/doc/sloped.md";

/// Bytes examined when deciding whether a file is text.
const SNIFF_BYTES: usize = 8192;

pub fn read_file(path: &str) -> Result<String, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => return Err(format!("{path} is a directory")),
        Ok(meta) if meta.len() > MAX_FILE_BYTES => {
            return Err(format!(
                "{path} is {} MiB; the editor opens up to {} MiB",
                meta.len() / (1024 * 1024),
                MAX_FILE_BYTES / (1024 * 1024)
            ));
        }
        _ => {}
    }
    let bytes = fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    // The metadata above is a separate syscall, so it describes the file as it
    // was, not as it is; the read is what decides.
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "{path} is larger than the {} MiB the editor opens",
            MAX_FILE_BYTES / (1024 * 1024)
        ));
    }
    // A NUL byte means this is not text. Opening it would produce a buffer of
    // replacement characters that saving would then write back over the file,
    // so refusing is the only answer that cannot destroy anything.
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return Err(format!("{path} is a binary file"));
    }
    // Lossy rather than refusing: a text file with one stray byte is still
    // worth opening, and the replacement character says where it was.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Writes `text` to `path` through a sibling temporary file.
///
/// `File::create` truncates before the first byte of the new content is
/// written, so a write that fails part way — a full image, a device error, a
/// quota denial — would leave the user's file destroyed while the editor still
/// held the only copy. Writing a sibling, `fsync`-ing it and renaming over the
/// target means the file on the medium is either the old one or the whole new
/// one, and never a prefix of either; the rename is itself one journalled
/// metadata operation, so a rude exit cannot catch it half done.
pub fn write_file(path: &str, text: &str) -> Result<(), String> {
    let temp = temp_path(path);
    let write = || -> Result<(), String> {
        let mut file = fs::File::create(&temp).map_err(|e| format!("{temp}: {e}"))?;
        file.write_all(text.as_bytes())
            .map_err(|e| format!("{temp}: {e}"))?;
        file.flush().map_err(|e| format!("{temp}: {e}"))?;
        file.sync_all().map_err(|e| format!("{temp}: {e}"))?;
        Ok(())
    };
    if let Err(message) = write() {
        let _ = fs::remove_file(&temp);
        return Err(message);
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(format!("{path}: {e}"));
    }
    Ok(())
}

/// A sibling of `path` — the rename below has to stay inside one directory —
/// named so a crash between the write and the rename leaves something a user
/// can recognise and delete.
fn temp_path(path: &str) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(at) => (&path[..at + 1], &path[at + 1..]),
        None => ("", path),
    };
    format!("{dir}.{name}.sloped-{}", std::process::id())
}

/// Entries one directory contributes to the tree. A directory larger than this
/// is shown truncated rather than read into a vector the sidebar cannot scroll.
pub const MAX_DIR_ENTRIES: usize = 20_000;

pub fn read_dir(path: &str) -> Result<Vec<DirEntry>, String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(path).map_err(|e| format!("{path}: {e}"))?;
    for entry in entries {
        if out.len() >= MAX_DIR_ENTRIES {
            break;
        }
        let Ok(entry) = entry else { continue };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name == "." || name == ".." {
            continue;
        }
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
        out.push(DirEntry { name, is_dir });
    }
    Ok(out)
}

pub fn is_file(path: &str) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

pub fn is_dir(path: &str) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// The directory the editor starts in: its argument if it is one, else the
/// working directory, else the root.
pub fn start_directory(arg: Option<&str>) -> String {
    if let Some(path) = arg {
        if is_dir(path) {
            return path.to_string();
        }
        let parent = slopos_editor_core::document::parent_dir(path);
        if !parent.is_empty() && is_dir(parent) {
            return parent.to_string();
        }
    }
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .filter(|p| is_dir(p))
        .unwrap_or_else(|| String::from("/"))
}

/// `path` made absolute against `base`, with `.` and `..` resolved.
///
/// Textual, not a syscall: the editor asks this of paths a person typed, which
/// may not exist yet — a `Save As` target is the whole point.
pub fn absolutize(base: &str, path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let mut out = String::from(base.trim_end_matches('/'));
        out.push('/');
        out.push_str(path);
        out
    };

    let mut parts: Vec<&str> = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let mut out = String::new();
    for part in parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}
