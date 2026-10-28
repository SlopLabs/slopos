//! Every read and write the editor does, each failure a message for the status
//! bar rather than a panic.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

use slopos_editor_core::document::DiskStamp;
use slopos_editor_core::filetree::DirEntry;

use super::said;

/// Past it the read alone would hold the window; refusing says so rather than
/// seeming to hang.
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

pub const DOC_PATH: &str = "/usr/share/slopos/doc/editor.md";

const SNIFF_BYTES: usize = 8192;

pub fn read_file(path: &str) -> Result<String, String> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => return Err(said::folder(path)),
        Ok(meta) if meta.len() > MAX_FILE_BYTES => return Err(said::too_large(path)),
        _ => {}
    }
    let bytes = fs::read(path).map_err(|e| said::unreadable(path, e.kind()))?;
    // The file may have grown since the metadata; the read decides.
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(said::too_large(path));
    }
    // Its replacement characters would be what a save writes back.
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return Err(said::not_text(path));
    }
    // Lossy: a text file with one stray byte is still worth opening.
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Through a synced sibling renamed over `path`, so a failed save leaves the
/// old file or the whole new one, never a prefix.
pub fn write_file(path: &str, text: &str) -> Result<(), String> {
    // A new inode does not inherit the old one's mode.
    let mode = fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o7777);
    let temp = temp_path(path);
    let unsaved = |e: std::io::Error| said::unsaved(path, e.kind());
    let write = || -> Result<(), String> {
        let mut file = fs::File::create(&temp).map_err(unsaved)?;
        file.write_all(text.as_bytes()).map_err(unsaved)?;
        file.flush().map_err(unsaved)?;
        file.sync_all().map_err(unsaved)?;
        Ok(())
    };
    if let Err(message) = write() {
        let _ = fs::remove_file(&temp);
        return Err(message);
    }
    if let Some(mode) = mode {
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(mode));
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(unsaved(e));
    }
    Ok(())
}

/// Named so one a crash leaves behind is recognisable.
fn temp_path(path: &str) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(at) => (&path[..at + 1], &path[at + 1..]),
        None => ("", path),
    };
    let suffix = format!(".editor-{}", std::process::id());
    // The name shortens, so the suffix never pushes it past the limit.
    let room = slopos_abi::fs::USER_NAME_MAX.saturating_sub(suffix.len() + 1);
    let mut stem = String::new();
    for (index, ch) in name.char_indices() {
        if index + ch.len_utf8() > room {
            break;
        }
        stem.push(ch);
    }
    format!("{dir}.{stem}{suffix}")
}

/// A larger folder shows its first this many entries.
pub const MAX_DIR_ENTRIES: usize = 20_000;

pub fn read_dir(path: &str) -> Result<Vec<DirEntry>, String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(path).map_err(|e| said::unlisted(path, e.kind()))?;
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

pub fn disk_stamp(path: &str) -> Option<DiskStamp> {
    let meta = fs::metadata(path).ok()?;
    let modified_secs = meta
        .modified()
        .ok()?
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(DiskStamp {
        modified_secs,
        len: meta.len(),
    })
}

pub fn is_file(path: &str) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

pub fn is_dir(path: &str) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

/// The argument's folder, else the working directory, else `/`.
pub fn start_directory(arg: Option<&str>) -> String {
    // Absolute: `absolutize` would re-root a relative base at `/`.
    let working = working_directory();
    if let Some(path) = arg {
        if is_dir(path) {
            return absolutize(&working, path);
        }
        let parent = slopos_editor_core::document::parent_dir(path);
        if !parent.is_empty() {
            let parent = absolutize(&working, parent);
            if is_dir(&parent) {
                return parent;
            }
        }
    }
    working
}

fn working_directory() -> String {
    std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .filter(|p| p.starts_with('/') && is_dir(p))
        .unwrap_or_else(|| String::from("/"))
}

/// Textual rather than a syscall: a Save as target need not exist yet.
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
