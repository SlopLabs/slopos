//! Lexical path canonicalisation.
//!
//! Mount resolution is a prefix match against the mount table, so it must run
//! on a normalised path: `//tmp/x`, `/./tmp/x` and `/a/../tmp/x` all name
//! `/tmp/x` but none of them share its byte prefix, and each would otherwise
//! miss the `/tmp` mount and land on the root filesystem's shadowed directory.

use crate::vfs::traits::{VfsError, VfsResult};
use crate::{MAX_NAME_LEN, MAX_PATH_LEN};
use slopos_ostd::KVec;

/// A canonicalised absolute path.
///
/// Heap-backed: `MAX_PATH_LEN` is 4096 and a kernel frame is bounded at 2 KiB
/// against a 4 KiB guard page. Not `Clone`: `KVec`'s clone panics on
/// allocation failure.
pub struct CanonPath {
    buf: KVec<u8>,
}

impl CanonPath {
    /// Adopt a buffer the resolver built; it must already be absolute and
    /// separator-collapsed.
    #[inline]
    pub(crate) fn from_buf(buf: KVec<u8>) -> Self {
        Self { buf }
    }

    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.buf.as_slice()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Always false: a canonical path carries at least its root slash.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Collapse `//`, drop `.`, and resolve `..` lexically against the path root.
///
/// `..` above the root is absorbed, as POSIX requires of an absolute path.
pub fn canonicalise(path: &[u8]) -> VfsResult<CanonPath> {
    canonicalise_at(path, b"/")
}

/// [`canonicalise`] against a working directory; a `path` starting with `/`
/// ignores `cwd`.
///
/// An empty `path` is `ENOENT`, as Linux reports it. `cwd` may carry a
/// trailing NUL: the task's own copy is NUL-terminated.
pub fn canonicalise_at(path: &[u8], cwd: &[u8]) -> VfsResult<CanonPath> {
    build(path, cwd, false)
}

/// [`canonicalise_at`] with `..` left standing for the walk to resolve.
///
/// Lexical `..` erases the symlink it follows: `/a/link/../b` names `b` beside
/// *link's target*, which only a walk that traversed `link` knows.
pub(crate) fn normalise_at(path: &[u8], cwd: &[u8]) -> VfsResult<CanonPath> {
    build(path, cwd, true)
}

fn build(path: &[u8], cwd: &[u8], defer_parents: bool) -> VfsResult<CanonPath> {
    if path.is_empty() {
        return Err(VfsError::NotFound);
    }
    if path.len() > MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }

    let base: &[u8] = if path[0] == b'/' {
        b""
    } else {
        let cwd = trim_trailing_nul(cwd);
        if cwd.is_empty() || cwd[0] != b'/' {
            return Err(VfsError::InvalidPath);
        }
        if cwd.len() > MAX_PATH_LEN {
            return Err(VfsError::NameTooLong);
        }
        cwd
    };

    let cap = (base.len() + 1 + path.len()).min(MAX_PATH_LEN) + 1;
    let mut out = KVec::<u8>::with_capacity(cap).map_err(|_| VfsError::NoSpace)?;
    out.push(b'/').map_err(|_| VfsError::NoSpace)?;

    for segment in [base, path] {
        for component in segment.split(|&c| c == b'/') {
            push_component(&mut out, component, defer_parents)?;
        }
    }

    Ok(CanonPath { buf: out })
}

fn trim_trailing_nul(mut cwd: &[u8]) -> &[u8] {
    while let [rest @ .., 0] = cwd {
        cwd = rest;
    }
    cwd
}

fn push_component(out: &mut KVec<u8>, component: &[u8], defer_parents: bool) -> VfsResult<()> {
    if component.is_empty() || component == b"." {
        return Ok(());
    }
    if component == b".." && !defer_parents {
        rewind(out);
        return Ok(());
    }
    if component.len() > MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }

    let sep = usize::from(out.len() > 1);
    if out.len() + sep + component.len() > MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }
    if sep == 1 {
        out.push(b'/').map_err(|_| VfsError::NoSpace)?;
    }
    out.extend_from_slice(component)
        .map_err(|_| VfsError::NoSpace)
}

/// Drop the last component. The backwards scan for the separator is what
/// replaces a per-component offset table, which a 4096-byte path could not
/// hold on the frame.
fn rewind(out: &mut KVec<u8>) {
    let bytes = out.as_slice();
    let mut len = bytes.len();
    while len > 1 && bytes[len - 1] != b'/' {
        len -= 1;
    }
    if len > 1 {
        len -= 1;
    }
    out.truncate(len.max(1));
}
