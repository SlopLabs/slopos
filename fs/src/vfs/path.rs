use crate::vfs::canon::{CanonPath, normalise_at};
use crate::vfs::mount::mount_at;
use crate::vfs::traits::{FileSystem, FileType, InodeId, VfsError, VfsResult};
use crate::{MAX_PATH_LEN, MAX_SYMLINK_FOLLOWS};
use slopos_ostd::KVec;

/// Follow symlinks, the final component included.
pub const RESOLVE_FOLLOW: u32 = 0;
/// `AT_SYMLINK_NOFOLLOW`: the final component resolves to the link itself.
pub const RESOLVE_NOFOLLOW_FINAL: u32 = 1;
/// The result must be a directory. Canonicalisation drops a trailing slash, so
/// only the syscall boundary can derive this, from the raw path or `O_DIRECTORY`.
pub const RESOLVE_MUST_BE_DIR: u32 = 2;

pub struct ResolvedPath {
    pub fs: &'static dyn FileSystem,
    pub inode: InodeId,
    /// Of the mount the walk ended in.
    pub mount_flags: u32,
}

impl ResolvedPath {
    pub fn read_only(&self) -> bool {
        self.mount_flags & crate::vfs::mount::MOUNT_RDONLY != 0
    }

    /// `Err(ReadOnly)` when the mount refuses mutation.
    pub fn check_writable(&self) -> VfsResult<()> {
        if self.read_only() {
            Err(VfsError::ReadOnly)
        } else {
            Ok(())
        }
    }
}

/// A resolved path's final component.
///
/// Borrows the normalised path the resolver built: an inline 255-byte array
/// does not fit the 2 KiB frame this kernel bounds.
pub struct NameBuf {
    canon: CanonPath,
    start: usize,
}

impl NameBuf {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.canon.as_bytes()[self.start..]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.canon.len() - self.start
    }

    /// Always false: an empty final component is [`VfsError::InvalidPath`].
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn resolve_path(path: &[u8]) -> VfsResult<ResolvedPath> {
    resolve_path_at(path, b"/", RESOLVE_FOLLOW)
}

/// Resolve `path` against `cwd`, following symlinks.
///
/// [`MAX_SYMLINK_FOLLOWS`] is the budget for the whole resolution, not per
/// component; exhausting it is [`VfsError::TooManySymlinks`].
pub fn resolve_path_at(path: &[u8], cwd: &[u8], flags: u32) -> VfsResult<ResolvedPath> {
    resolve_followed(normalise_at(path, cwd)?.as_bytes(), flags)
}

/// [`resolve_path_at`] plus the canonical path the walk ended on.
///
/// A directory descriptor stores this, not the caller's spelling: a lexical
/// canonicalisation of a symlinked directory names a different place than the
/// walk reached.
pub fn resolve_path_canon_at(
    path: &[u8],
    cwd: &[u8],
    flags: u32,
) -> VfsResult<(ResolvedPath, CanonPath)> {
    let normalised = normalise_at(path, cwd)?;
    let step = walk_followed(normalised.as_bytes(), flags)?;
    Ok((step.resolved, CanonPath::from_buf(step.canon)))
}

/// The symlink-resolved directory holding `path`'s final component, and that
/// component, which is left unresolved.
pub fn resolve_parent_at(path: &[u8], cwd: &[u8]) -> VfsResult<(ResolvedPath, NameBuf)> {
    let canon = normalise_at(path, cwd)?;
    let (parent_len, start) = {
        let bytes = canon.as_bytes();
        let (parent, name) = split_path(bytes).ok_or(VfsError::InvalidPath)?;
        // A trailing `..` names a directory that already exists, so it is never
        // a name to create, remove or rename; Linux answers `EINVAL`.
        if name == b".." {
            return Err(VfsError::InvalidPath);
        }
        (parent.len(), bytes.len() - name.len())
    };
    let resolved = resolve_followed(&canon.as_bytes()[..parent_len], RESOLVE_MUST_BE_DIR)?;
    Ok((resolved, NameBuf { canon, start }))
}

#[derive(Clone, Copy)]
struct Link {
    rest: usize,
    size: u64,
}

struct Step {
    resolved: ResolvedPath,
    /// Canonical path of `resolved`, or of the link's parent when `link` is set.
    canon: KVec<u8>,
    link: Option<Link>,
}

#[derive(Clone, Copy)]
struct Ancestor {
    fs: &'static dyn FileSystem,
    inode: InodeId,
    mount_flags: u32,
    canon_len: usize,
}

#[inline(never)]
fn resolve_followed(path: &[u8], flags: u32) -> VfsResult<ResolvedPath> {
    Ok(walk_followed(path, flags)?.resolved)
}

/// Walk `path`, splicing each followed symlink's target in and restarting, so
/// the budget spans the whole resolution rather than one component's chain.
#[inline(never)]
fn walk_followed(path: &[u8], flags: u32) -> VfsResult<Step> {
    let mut step = walk(path, flags)?;
    let Some(mut link) = step.link else {
        return Ok(step);
    };

    let mut scratch = KVec::<u8>::new();
    let mut canon = expand_link(path, &step, link, &mut scratch)?;
    let mut follows = 1u32;
    loop {
        step = walk(canon.as_bytes(), flags)?;
        match step.link {
            None => return Ok(step),
            Some(next) => link = next,
        }
        follows += 1;
        if follows > MAX_SYMLINK_FOLLOWS {
            return Err(VfsError::TooManySymlinks);
        }
        let next_canon = expand_link(canon.as_bytes(), &step, link, &mut scratch)?;
        canon = next_canon;
    }
}

/// Walk a normalised path, building the canonical path as it goes, and stop at
/// the first symlink that must be followed.
///
/// `..` is resolved here rather than lexically: POSIX puts the `b` of
/// `/a/link/../b` beside *link's target*, which a rewind of the spelling
/// cannot say. Above the root it is absorbed.
///
/// The mount table is re-asked at every component, against the canonical path
/// built so far, so a mount point crossed mid-walk is honoured.
#[inline(never)]
fn walk(path: &[u8], flags: u32) -> VfsResult<Step> {
    let root = mount_at(b"/").ok_or(VfsError::NotFound)?;
    let mut cur = Ancestor {
        fs: root.fs,
        inode: root.fs.root_inode(),
        mount_flags: root.flags,
        canon_len: 1,
    };
    // Never reallocates: `..` only shortens, every other component is copied
    // with its separator, so the canonical path is never longer than `path`.
    let mut canon = KVec::<u8>::with_capacity(path.len() + 1).map_err(|_| VfsError::NoSpace)?;
    canon.push(b'/').map_err(|_| VfsError::NoSpace)?;
    // Only a `..` needs the stack, so the scan buys most paths no allocation.
    let track_parents = has_parent_component(path);
    let mut ancestors = KVec::<Ancestor>::new();

    let mut idx = 1usize;
    while idx < path.len() {
        let start = idx;
        let end = match path[start..].iter().position(|&c| c == b'/') {
            Some(off) => start + off,
            None => path.len(),
        };
        idx = end + 1;
        let name = &path[start..end];

        if name == b".." {
            if let Some(parent) = ancestors.pop() {
                canon.truncate(parent.canon_len);
                cur = parent;
            }
            continue;
        }

        push_name(&mut canon, name)?;
        if let Some(crossed) = mount_at(canon.as_slice()) {
            if track_parents {
                ancestors.push(cur).map_err(|_| VfsError::NoSpace)?;
            }
            cur = Ancestor {
                fs: crossed.fs,
                inode: crossed.fs.root_inode(),
                mount_flags: crossed.flags,
                canon_len: canon.len(),
            };
            continue;
        }

        let child = cur.fs.lookup(cur.inode, name)?;
        // `mount_at` released the mount table before returning, which is what
        // lets this reach a filesystem whose own lock is a sleeping mutex.
        let (kind, size) = child_kind(cur.fs, child)?;
        let is_final = end == path.len();

        if kind == FileType::Symlink && !(is_final && flags & RESOLVE_NOFOLLOW_FINAL != 0) {
            canon.truncate(cur.canon_len);
            return Ok(Step {
                resolved: ResolvedPath {
                    fs: cur.fs,
                    inode: child,
                    mount_flags: cur.mount_flags,
                },
                canon,
                link: Some(Link { rest: end, size }),
            });
        }
        if !is_final && kind != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }
        if track_parents {
            ancestors.push(cur).map_err(|_| VfsError::NoSpace)?;
        }
        cur = Ancestor {
            inode: child,
            canon_len: canon.len(),
            ..cur
        };
    }

    if flags & RESOLVE_MUST_BE_DIR != 0 && child_kind(cur.fs, cur.inode)?.0 != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    Ok(Step {
        resolved: ResolvedPath {
            fs: cur.fs,
            inode: cur.inode,
            mount_flags: cur.mount_flags,
        },
        canon,
        link: None,
    })
}

fn push_name(canon: &mut KVec<u8>, name: &[u8]) -> VfsResult<()> {
    if canon.len() > 1 {
        canon.push(b'/').map_err(|_| VfsError::NoSpace)?;
    }
    canon.extend_from_slice(name).map_err(|_| VfsError::NoSpace)
}

fn has_parent_component(path: &[u8]) -> bool {
    let mut idx = 1usize;
    while idx < path.len() {
        let end = match path[idx..].iter().position(|&c| c == b'/') {
            Some(off) => idx + off,
            None => path.len(),
        };
        if &path[idx..end] == b".." {
            return true;
        }
        idx = end + 1;
    }
    false
}

/// Its own frame: [`FileStat`] is 88 bytes, and the walk already carries a
/// canonical-path buffer and an ancestor stack.
///
/// [`FileStat`]: crate::vfs::FileStat
#[inline(never)]
fn child_kind(fs: &'static dyn FileSystem, inode: InodeId) -> VfsResult<(FileType, u64)> {
    let stat = fs.stat(inode)?;
    Ok((stat.file_type, stat.size))
}

/// Splice a symlink's target into the path that named it: an absolute target
/// replaces the resolved prefix, a relative one hangs off `step.canon`.
#[inline(never)]
fn expand_link(
    path: &[u8],
    step: &Step,
    link: Link,
    scratch: &mut KVec<u8>,
) -> VfsResult<CanonPath> {
    let want = usize::try_from(link.size).unwrap_or(MAX_PATH_LEN);
    if want == 0 || want > MAX_PATH_LEN {
        return Err(VfsError::InvalidPath);
    }
    scratch.clear();
    scratch.resize(want, 0).map_err(|_| VfsError::NoSpace)?;

    let read = step
        .resolved
        .fs
        .readlink(step.resolved.inode, scratch.as_mut_slice())?;
    if read == 0 || read > want {
        return Err(VfsError::InvalidPath);
    }
    scratch.truncate(read);

    let suffix = &path[link.rest..];
    if scratch.len() + suffix.len() > MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }
    scratch
        .extend_from_slice(suffix)
        .map_err(|_| VfsError::NoSpace)?;

    if scratch.as_slice()[0] == b'/' {
        return normalise_at(scratch.as_slice(), b"/");
    }
    normalise_at(scratch.as_slice(), step.canon.as_slice())
}

fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return None;
    }

    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }

    if end <= 1 {
        return None;
    }

    let trimmed = &path[..end];

    let mut idx = trimmed.len();
    while idx > 0 && trimmed[idx - 1] != b'/' {
        idx -= 1;
    }

    if idx == 0 {
        return None;
    }

    let parent = if idx == 1 {
        &trimmed[..1]
    } else {
        &trimmed[..idx - 1]
    };
    let name = &trimmed[idx..];

    if name.is_empty() {
        return None;
    }

    Some((parent, name))
}
