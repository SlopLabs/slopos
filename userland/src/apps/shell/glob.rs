//! Pathname expansion (POSIX §2.14.3): the directory walk.
//!
//! Matching itself is [`slopos_shell_core::pattern`]; what lives here is the
//! part that reads the filesystem.
//!
//! Two rules make the result safe to hand a command: a field with no unquoted
//! pattern character is not globbed at all, and a pattern that matches nothing
//! is left as written. There is no `nullglob`, so `rm *.o` in a directory with
//! no object files runs `rm` with a literal argument rather than with none.

use std::fs;

use slopos_shell_core::pattern;
use slopos_shell_core::qbuf::QBuf;

/// Expand one field. `None` means the field stands as written — either it
/// holds no unquoted pattern character, or nothing on disk matched.
pub fn expand(field: &QBuf) -> Option<Vec<Vec<u8>>> {
    if !pattern::has_meta(field) {
        return None;
    }

    let absolute = field.bytes.first() == Some(&b'/');
    let trailing_slash = field.len() > 1 && field.bytes.last() == Some(&b'/');
    let components = components(field);
    if components.is_empty() {
        return None;
    }

    let mut frontier: Vec<Vec<u8>> = if absolute {
        vec![b"/".to_vec()]
    } else {
        vec![Vec::new()]
    };
    for (index, component) in components.iter().enumerate() {
        let last = index + 1 == components.len();
        if !pattern::has_meta(component) {
            for path in frontier.iter_mut() {
                join(path, &component.bytes);
            }
            continue;
        }
        // Only a directory can carry the components that follow.
        let want_dir = !last || trailing_slash;
        let mut next: Vec<Vec<u8>> = Vec::new();
        for dir in &frontier {
            let mut names = children(dir, component, want_dir);
            names.sort();
            for name in names {
                let mut path = dir.clone();
                join(&mut path, &name);
                next.push(path);
            }
        }
        frontier = next;
        if frontier.is_empty() {
            return None;
        }
    }

    // POSIX §2.13.3: a generated pathname is listed only if it names an
    // existing file, and a literal component after a wildcard was never
    // checked by the walk — `echo */nope` would otherwise invent one path per
    // directory.
    frontier.retain(|path| exists(path, trailing_slash));
    if frontier.is_empty() {
        return None;
    }
    if trailing_slash {
        for path in frontier.iter_mut() {
            path.push(b'/');
        }
    }
    Some(frontier)
}

/// Whether a generated pathname names something. A dangling symlink still
/// names a file, hence no following — unless a trailing slash asked for a
/// directory, which a link's target must be.
fn exists(path: &[u8], must_be_dir: bool) -> bool {
    let Ok(path) = core::str::from_utf8(path) else {
        return false;
    };
    if must_be_dir {
        return fs::metadata(path).is_ok_and(|meta| meta.is_dir());
    }
    fs::symlink_metadata(path).is_ok()
}

/// Split on `/`, dropping empty components: a quoted slash is still a
/// separator, and `a//b` names the same file as `a/b`.
fn components(field: &QBuf) -> Vec<QBuf> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for i in 0..=field.len() {
        if i == field.len() || field.bytes[i] == b'/' {
            if i > start {
                out.push(field.slice(start, i));
            }
            start = i + 1;
        }
    }
    out
}

fn join(path: &mut Vec<u8>, name: &[u8]) {
    if !path.is_empty() && path.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
}

/// Names in `dir` matching `component`. An unreadable directory is not an
/// error; it contributes no matches.
fn children(dir: &[u8], component: &QBuf, want_dir: bool) -> Vec<Vec<u8>> {
    let listing = if dir.is_empty() { b"." } else { dir };
    let Ok(path) = core::str::from_utf8(listing) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };

    // A leading `.` is only ever matched by a literal `.` in the pattern.
    let allow_hidden = component.bytes.first() == Some(&b'.');
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "." || name == ".." {
            continue;
        }
        if name.starts_with('.') && !allow_hidden {
            continue;
        }
        if !pattern::matches_component(component, name.as_bytes()) {
            continue;
        }
        if want_dir && !fs::metadata(entry.path()).is_ok_and(|m| m.is_dir()) {
            continue;
        }
        out.push(name.as_bytes().to_vec());
    }
    out
}
