//! Filesystem operations the utilities share: the calls `std` does not expose
//! on this target, and one directory walk instead of a recursion per tool.

use std::ffi::CString;
use std::fs::{self, Metadata};

use slopos_slibc::pal::{Pal, Sys};

fn bad_path() -> std::io::Error {
    std::io::Error::from(std::io::ErrorKind::InvalidInput)
}

fn os_error(errno: slopos_slibc::errno::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(errno.raw())
}

/// `chmod(2)`. `std::fs::set_permissions` needs `PermissionsExt` to carry a
/// mode, and that extension trait is not wired into the patched std.
pub fn chmod(path: &str, mode: u32) -> Result<(), std::io::Error> {
    let c_path = CString::new(path).map_err(|_| bad_path())?;
    Sys::chmod(c_path.as_ptr() as *const u8, mode).map_err(os_error)
}

/// `stat(2)`'s `st_mode`. The patched std's `Metadata` carries no mode, so a
/// tool that has to preserve one — copying a file, or replacing it through a
/// temporary — has to ask the kernel directly.
pub fn mode_of(path: &str) -> Result<u32, std::io::Error> {
    let c_path = CString::new(path).map_err(|_| bad_path())?;
    let mut buf = [0u8; core::mem::size_of::<slopos_abi::fs::UserFsStat>()];
    Sys::stat(c_path.as_ptr() as *const u8, buf.as_mut_ptr()).map_err(os_error)?;
    // `st_mode` sits at a fixed offset in the Linux x86-64 layout the struct
    // reproduces; reading it by offset avoids a transmute of a byte buffer.
    const ST_MODE_OFFSET: usize = 24;
    let mut mode = [0u8; 4];
    mode.copy_from_slice(&buf[ST_MODE_OFFSET..ST_MODE_OFFSET + 4]);
    Ok(u32::from_ne_bytes(mode))
}

/// `symlink(2)`.
pub fn symlink(target: &[u8], link: &str) -> Result<(), std::io::Error> {
    let c_target = CString::new(target).map_err(|_| bad_path())?;
    let c_link = CString::new(link).map_err(|_| bad_path())?;
    Sys::symlink(c_target.as_ptr() as *const u8, c_link.as_ptr() as *const u8).map_err(os_error)
}

/// `link(2)`.
pub fn hard_link(existing: &str, new: &str) -> Result<(), std::io::Error> {
    fs::hard_link(existing, new)
}

/// `readlink(2)`, answering the target bytes.
pub fn read_link(path: &str) -> Result<Vec<u8>, std::io::Error> {
    let c_path = CString::new(path).map_err(|_| bad_path())?;
    let mut buf = vec![0u8; slopos_abi::fs::USER_PATH_MAX];
    let len = Sys::readlink(c_path.as_ptr() as *const u8, buf.as_mut_ptr(), buf.len())
        .map_err(os_error)?;
    buf.truncate(len.min(buf.len()));
    Ok(buf)
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
