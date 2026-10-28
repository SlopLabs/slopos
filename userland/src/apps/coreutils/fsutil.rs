//! Filesystem operations the utilities share: links by their target's bytes,
//! the access check `std` does not expose, and one directory walk instead of
//! a recursion per tool.

use std::ffi::{CString, OsStr};
use std::fs::{self, Metadata};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use slopos_slibc::pal::{Pal, Sys};

/// `symlink(2)`, storing `target`'s bytes as they are.
pub fn symlink(target: &[u8], link: &str) -> Result<(), std::io::Error> {
    std::os::unix::fs::symlink(OsStr::from_bytes(target), link)
}

/// `readlink(2)`, answering the target bytes.
pub fn read_link(path: &str) -> Result<Vec<u8>, std::io::Error> {
    fs::read_link(path).map(|target| target.into_os_string().into_vec())
}

/// `PATH`, or the system's default search path when the environment names none.
pub fn search_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| default_search_path().to_owned())
}

pub fn default_search_path() -> &'static str {
    slopos_abi::fs::DEFAULT_PATH
        .to_str()
        .expect("the default search path is ASCII")
}

/// The first executable regular file `name` names on `path`; a name holding a
/// `/` is taken as it stands, if it is one.
pub fn resolve_command(name: &str, path: &str) -> Option<String> {
    if name.contains('/') {
        return is_executable_file(name).then(|| name.to_string());
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| join(dir, name))
        .find(|candidate| is_executable_file(candidate))
}

/// A regular file the caller may execute.
pub fn is_executable_file(path: &str) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file()) && may(path, slopos_abi::fs::X_OK)
}

/// Whether the caller's effective ids may `mode` (`R_OK`, `W_OK`, `X_OK`)
/// on `path`, as `test` and a `PATH` search ask it.
pub fn may(path: &str, mode: u32) -> bool {
    CString::new(path).is_ok_and(|c_path| {
        Sys::faccessat(
            slopos_abi::fs::AT_FDCWD,
            c_path.as_ptr().cast(),
            mode,
            slopos_abi::fs::AT_EACCESS,
        )
        .is_ok()
    })
}

/// Join a directory and a name, collapsing the separator so `/` + `bin` is
/// `/bin` rather than `//bin`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        return name.to_string();
    }
    let mut out = String::with_capacity(dir.len() + 1 + name.len());
    out.push_str(dir);
    if !dir.ends_with('/') {
        out.push('/');
    }
    out.push_str(name);
    out
}

/// Whether `path` reaches outside the directory it is relative to. Callers
/// refuse rather than trim: trimming turns a hostile input into a silently
/// successful overwrite.
pub fn escapes(path: &[u8]) -> bool {
    path.first() == Some(&b'/') || path.split(|&byte| byte == b'/').any(|part| part == b"..")
}

/// The final component of `path`, as `basename(1)` reports it.
pub fn base_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { "" } else { "/" };
    }
    match trimmed.rfind('/') {
        Some(i) => &trimmed[i + 1..],
        None => trimmed,
    }
}

/// The directory part of `path`, as `dirname(1)` reports it.
pub fn dir_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return if path.is_empty() { "." } else { "/" };
    }
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(i) => &trimmed[..i],
        None => ".",
    }
}

pub fn is_dir(path: &str) -> bool {
    fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl Kind {
    pub fn of(meta: &Metadata) -> Kind {
        let t = meta.file_type();
        if t.is_dir() {
            Kind::Dir
        } else if t.is_symlink() {
            Kind::Symlink
        } else if t.is_file() {
            Kind::File
        } else {
            Kind::Other
        }
    }
}

pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub meta: Metadata,
    pub depth: usize,
}

/// A directory a pre-order visit entered, reported again after its children so
/// a caller can remove or stamp it last.
pub enum Visit {
    Pre(Entry),
    Post(Entry),
}

impl Visit {
    pub fn entry(&self) -> &Entry {
        match self {
            Visit::Pre(e) | Visit::Post(e) => e,
        }
    }
}

enum Frame {
    /// A path not yet inspected.
    Open(String, usize),
    /// A directory whose children have been pushed; report it on the way out.
    Close(Entry),
}

/// Depth-first walk over an explicit stack, so a deep tree is not a deep
/// recursion. A symlink is reported, never traversed, unless `follow` is set —
/// which is what keeps `rm -r` and `tar -c` inside the tree they were given.
pub struct Walk {
    stack: Vec<Frame>,
    follow: bool,
    post: bool,
}

impl Walk {
    pub fn new(root: &str) -> Self {
        Self {
            stack: vec![Frame::Open(root.to_string(), 0)],
            follow: false,
            post: false,
        }
    }

    /// Traverse symlinked directories.
    pub fn follow(mut self) -> Self {
        self.follow = true;
        self
    }

    /// Also report each directory after its children.
    pub fn with_post(mut self) -> Self {
        self.post = true;
        self
    }
}

pub struct WalkError {
    pub path: String,
    pub error: std::io::Error,
}

impl Iterator for Walk {
    type Item = Result<Visit, WalkError>;

    fn next(&mut self) -> Option<Self::Item> {
        let frame = self.stack.pop()?;
        let (path, depth) = match frame {
            Frame::Close(entry) => return Some(Ok(Visit::Post(entry))),
            Frame::Open(path, depth) => (path, depth),
        };

        let meta = if self.follow {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        };
        let meta = match meta {
            Ok(meta) => meta,
            Err(error) => return Some(Err(WalkError { path, error })),
        };
        let kind = Kind::of(&meta);
        let entry = Entry {
            path: path.clone(),
            kind,
            meta,
            depth,
        };

        if kind == Kind::Dir {
            let mut children = Vec::new();
            match fs::read_dir(&path) {
                Ok(dir) => {
                    for child in dir {
                        match child {
                            Ok(child) => {
                                if let Ok(name) = child.file_name().into_string() {
                                    children.push(join(&path, &name));
                                }
                            }
                            Err(error) => {
                                return Some(Err(WalkError {
                                    path: path.clone(),
                                    error,
                                }));
                            }
                        }
                    }
                }
                Err(error) => return Some(Err(WalkError { path, error })),
            }
            children.sort_unstable_by(|a, b| b.cmp(a));
            if self.post {
                self.stack.push(Frame::Close(Entry {
                    path: path.clone(),
                    kind,
                    meta: entry.meta.clone(),
                    depth,
                }));
            }
            for child in children {
                self.stack.push(Frame::Open(child, depth + 1));
            }
        }

        Some(Ok(Visit::Pre(entry)))
    }
}
