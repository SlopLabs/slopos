use crate::vfs::mount::{MAX_MOUNTS, MountTable, with_mount_table};
use crate::vfs::path::{
    RESOLVE_FOLLOW, RESOLVE_NOFOLLOW_FINAL, ResolvedPath, resolve_parent_at, resolve_path,
    resolve_path_at, resolve_path_canon_at,
};
use crate::vfs::traits::{FileStat, FileType, InodeId, VfsError, VfsResult, same_filesystem};
use slopos_abi::fs::{FS_TYPE_DIRECTORY, UserFsEntry};
use slopos_ostd::KVec;

pub struct VfsHandle {
    pub inode: InodeId,
    pub fs: &'static dyn crate::vfs::FileSystem,
    /// Granted at open, where the read-only mount and the seal were checked;
    /// a handle without it cannot become a writer later.
    writable: bool,
}

impl VfsHandle {
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.fs.read(self.inode, offset, buf)
    }

    pub fn write(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        if !self.writable {
            return Err(VfsError::PermissionDenied);
        }
        self.fs.write(self.inode, offset, buf)
    }

    pub fn size(&self) -> VfsResult<u64> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.size)
    }

    pub fn is_directory(&self) -> VfsResult<bool> {
        let stat = self.fs.stat(self.inode)?;
        Ok(stat.file_type == FileType::Directory)
    }
}

pub struct VfsOpenFlags {
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
    pub writable: bool,
}

impl VfsOpenFlags {
    pub const fn read_only() -> Self {
        Self {
            create: false,
            exclusive: false,
            truncate: false,
            writable: false,
        }
    }

    pub const fn create_only() -> Self {
        Self {
            create: true,
            exclusive: false,
            truncate: false,
            writable: true,
        }
    }
}

/// A name the VFS accepts fits a listing entry together with its NUL, so a
/// listed name is never truncated and can always be opened again. The two
/// widths are defined apart, which is what makes this worth asserting.
const _: () = assert!(crate::MAX_NAME_LEN < UserFsEntry::new().name.len());

/// One creation-time limit whatever the root filesystem is: a longer name
/// would list truncated and then fail to open.
fn check_name_len(name: &[u8]) -> VfsResult<()> {
    if name.len() > crate::MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

pub fn vfs_open(path: &[u8], create: bool) -> VfsResult<VfsHandle> {
    vfs_open_flags(
        path,
        VfsOpenFlags {
            create,
            exclusive: false,
            truncate: false,
            writable: create,
        },
    )
}

pub fn vfs_open_flags(path: &[u8], flags: VfsOpenFlags) -> VfsResult<VfsHandle> {
    vfs_open_flags_at(path, b"/", flags, RESOLVE_FOLLOW)
}

pub fn vfs_open_flags_at(
    path: &[u8],
    cwd: &[u8],
    flags: VfsOpenFlags,
    resolve_flags: u32,
) -> VfsResult<VfsHandle> {
    match resolve_path_at(path, cwd, resolve_flags) {
        Ok(resolved) => {
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let stat = resolved.fs.stat(resolved.inode)?;
            match stat.file_type {
                FileType::Directory => return Err(VfsError::IsDirectory),
                FileType::Symlink => return Err(VfsError::TooManySymlinks),
                _ => {}
            }
            // Refused at open, not at the first write: a descriptor obtained
            // before the check is a descriptor that outlives it.
            if flags.writable {
                resolved.check_writable()?;
                if stat.sealed {
                    return Err(VfsError::PermissionDenied);
                }
            }
            if flags.truncate && flags.writable && stat.file_type == FileType::Regular {
                // `forget_inode`, not `detach_inode`: a flush would write back
                // the bytes the truncate discards, and a set left keyed would
                // put pre-truncate pages back over the zeroed region.
                crate::filemap::forget_inode(resolved.fs, resolved.inode);
                resolved.fs.truncate(resolved.inode, 0)?;
            }
            Ok(VfsHandle {
                inode: resolved.inode,
                fs: resolved.fs,
                writable: flags.writable,
            })
        }
        Err(VfsError::NotFound) if flags.create => {
            let (parent, name) = resolve_parent_at(path, cwd)?;
            check_name_len(name.as_bytes())?;
            parent.check_writable()?;
            let new_inode = parent
                .fs
                .create(parent.inode, name.as_bytes(), FileType::Regular)?;
            Ok(VfsHandle {
                inode: new_inode,
                fs: parent.fs,
                writable: flags.writable,
            })
        }
        Err(e) => Err(e),
    }
}

pub fn vfs_stat(path: &[u8]) -> VfsResult<FileStat> {
    vfs_stat_at(path, b"/", RESOLVE_FOLLOW)
}

pub fn vfs_stat_at(path: &[u8], cwd: &[u8], flags: u32) -> VfsResult<FileStat> {
    let resolved = resolve_path_at(path, cwd, flags)?;
    resolved.fs.stat(resolved.inode)
}

pub fn vfs_mkdir(path: &[u8]) -> VfsResult<()> {
    vfs_mkdir_at(path, b"/", None)
}

/// `mkdirat(2)`. `mode` lands on the inode `create` returned: re-resolving the
/// path to chmod it could hand the mode to a replacement, or to a symlink's
/// target.
pub fn vfs_mkdir_at(path: &[u8], cwd: &[u8], mode: Option<u16>) -> VfsResult<()> {
    let (parent, name) = resolve_parent_at(path, cwd)?;
    check_name_len(name.as_bytes())?;
    parent.check_writable()?;
    let inode = parent
        .fs
        .create(parent.inode, name.as_bytes(), FileType::Directory)?;
    if let Some(mode) = mode {
        parent.fs.set_mode(inode, mode)?;
    }
    Ok(())
}

pub fn vfs_set_mode(path: &[u8], mode: u16) -> VfsResult<()> {
    vfs_set_mode_at(path, b"/", mode, RESOLVE_FOLLOW)
}

pub fn vfs_set_mode_at(path: &[u8], cwd: &[u8], mode: u16, flags: u32) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, flags) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path_at(path, cwd, flags)?;
    resolved.check_writable()?;
    resolved.fs.set_mode(resolved.inode, mode)
}

/// Seal `path` against every future mutation. One-way and un-clearable.
pub fn vfs_set_sealed(path: &[u8]) -> VfsResult<()> {
    let resolved = resolve_path(path)?;
    resolved.check_writable()?;
    resolved.fs.set_sealed(resolved.inode)
}

/// Set an inode's times; `None` leaves a field alone, which is `UTIME_OMIT`.
pub fn vfs_utimens(
    path: &[u8],
    cwd: &[u8],
    atime: Option<u64>,
    mtime: Option<u64>,
    flags: u32,
) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, flags) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path_at(path, cwd, flags)?;
    resolved.check_writable()?;
    resolved.fs.set_times(resolved.inode, atime, mtime)
}

/// [`vfs_utimens`] for an open descriptor — `futimens(2)`. No mount flag is
/// reachable from `(fs, inode)`, so the read-only refusal is the descriptor
/// layer's.
pub fn vfs_set_times(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: InodeId,
    atime: Option<u64>,
    mtime: Option<u64>,
) -> VfsResult<()> {
    fs.set_times(inode, atime, mtime)
}

/// `link(2)`: a second name for an existing inode.
pub fn vfs_link(old_path: &[u8], new_path: &[u8], cwd: &[u8]) -> VfsResult<()> {
    vfs_link_at(old_path, cwd, new_path, cwd, false)
}

/// `linkat(2)`. `follow` is `AT_SYMLINK_FOLLOW`: without it a symlink source
/// is linked as itself.
///
/// Both ends are seal-checked: a link over a sealed name would give that path
/// a second, unsealed inode to reach, and the seal is what the
/// program-identity grants stand on.
pub fn vfs_link_at(
    old_path: &[u8],
    old_cwd: &[u8],
    new_path: &[u8],
    new_cwd: &[u8],
    follow: bool,
) -> VfsResult<()> {
    let source_flags = if follow {
        RESOLVE_FOLLOW
    } else {
        RESOLVE_NOFOLLOW_FINAL
    };
    let source = resolve_path_at(old_path, old_cwd, source_flags)?;
    let stat = source.fs.stat(source.inode)?;
    // POSIX leaves a directory hard link implementation-defined and every
    // implementation refuses it: `..` cannot describe a graph.
    if stat.file_type == FileType::Directory {
        return Err(VfsError::PermissionDenied);
    }
    if stat.sealed {
        return Err(VfsError::PermissionDenied);
    }
    if path_is_sealed_at(new_path, new_cwd, RESOLVE_NOFOLLOW_FINAL) {
        return Err(VfsError::PermissionDenied);
    }

    let (parent, name) = resolve_parent_at(new_path, new_cwd)?;
    check_name_len(name.as_bytes())?;
    if !same_filesystem(source.fs, parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    parent.check_writable()?;
    parent.fs.link(parent.inode, name.as_bytes(), source.inode)
}

/// Whether `path` names a sealed inode.
///
/// Fails **closed**: a resolve or stat that errors for any reason other than
/// the path not existing answers "sealed". This gate is what stops a task
/// replacing `/bin/compositor` and inheriting that path's privilege grant, so
/// an induced `IoError` or an out-of-memory in the block cache must not read
/// as permission. A path that genuinely resolves to nothing is not sealed —
/// the caller's own lookup reports that, and `NotFound` is the one error that
/// carries no ambiguity.
fn path_is_sealed_at(path: &[u8], cwd: &[u8], flags: u32) -> bool {
    match resolve_path_at(path, cwd, flags).and_then(|r| r.fs.stat(r.inode)) {
        Ok(stat) => stat.sealed,
        // Decided by resolution before any inode is consulted, so a path that
        // trips them cannot be naming a sealed inode; answering "sealed" would
        // report `EACCES` for all five.
        Err(VfsError::InvalidPath)
        | Err(VfsError::NameTooLong)
        | Err(VfsError::NotFound)
        | Err(VfsError::NotDirectory)
        | Err(VfsError::TooManySymlinks) => false,
        Err(_) => true,
    }
}

/// `unlink(2)`: remove a name.
///
/// The inode's contents outlive the name whenever a descriptor still holds
/// them — POSIX's rule, and what stops an `unlink` handing a live reader's
/// blocks to the next allocation. Which of the two removals runs is decided by
/// the open-reference count, under the lock that count is taken under, so an
/// inode opened concurrently is never freed out from under the opener.
///
/// The cheap path is the common one: nothing holds the inode open, and the
/// filesystem frees it with the name exactly as before.
pub fn vfs_unlink(path: &[u8]) -> VfsResult<()> {
    vfs_unlink_at(path, b"/")
}

pub fn vfs_unlink_at(path: &[u8], cwd: &[u8]) -> VfsResult<()> {
    use crate::vfs::orphan::{DetachPlan, RemovalOutcome, begin_removal, end_removal};

    if path_is_sealed_at(path, cwd, RESOLVE_NOFOLLOW_FINAL) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent_at(path, cwd)?;
    let name = name.as_bytes();
    parent.check_writable()?;

    // A name that resolves to nothing cannot be holding an inode open, and a
    // filesystem that reports its own `ENOENT` gives a better error than a
    // lookup here would.
    let Ok(inode) = parent.fs.lookup(parent.inode, name) else {
        return parent.fs.unlink(parent.inode, name);
    };

    // `FileSystem::rmdir` defaults to `unlink`, so a filesystem drawing no
    // distinction of its own would let either call remove either kind.
    if inode_is_directory(parent.fs, inode)? {
        return Err(VfsError::IsDirectory);
    }

    let linked = detach_unless_linked(parent.fs, inode);

    if begin_removal(parent.fs, inode) == DetachPlan::FreeNow {
        let result = parent.fs.unlink(parent.inode, name);
        let _ = end_removal(parent.fs, inode, RemovalOutcome::Nothing);
        forget_if_freed(parent.fs, inode, linked);
        return result;
    }

    let result = parent.fs.detach(parent.inode, name);
    forget_if_freed(parent.fs, inode, linked);
    let outcome = match result {
        Ok(Some(_)) => RemovalOutcome::Deferred,
        _ => RemovalOutcome::Nothing,
    };
    // A close that landed inside the scope above is the one case where the
    // free becomes runnable with nobody left to notice.
    if end_removal(parent.fs, inode, outcome) {
        crate::vfs::orphan::drain_or_wake(parent.fs);
    }
    result.map(|_| ())
}

/// Unkey `inode`'s page set ahead of a removal that takes its last name, and
/// answer whether another name kept it keyed instead. The flush is only safe
/// while the blocks are still the inode's, and the forget must land before its
/// number can be reallocated; a name among several changes neither, and
/// forgetting then would strand every mapping of a file that lives on.
fn detach_unless_linked(fs: &'static dyn crate::vfs::FileSystem, inode: InodeId) -> bool {
    if fs.stat(inode).is_ok_and(|stat| stat.nlink > 1) {
        return true;
    }
    crate::filemap::detach_inode(fs, inode);
    false
}

/// A removal racing for the other name may have taken the last one from an
/// inode whose set was left keyed. Forget only: once no name holds the inode,
/// its blocks are not its own to flush into for long.
fn forget_if_freed(fs: &'static dyn crate::vfs::FileSystem, inode: InodeId, linked: bool) {
    if linked && !fs.stat(inode).is_ok_and(|stat| stat.nlink > 0) {
        crate::filemap::forget_inode(fs, inode);
    }
}

#[inline(never)]
fn inode_is_directory(fs: &'static dyn crate::vfs::FileSystem, inode: InodeId) -> VfsResult<bool> {
    Ok(fs.stat(inode)?.file_type == FileType::Directory)
}

/// `rmdir(2)`: remove an empty directory. Refuses a mount point outright —
/// removing the directory a filesystem is mounted on would leave the mount
/// table naming a path with nothing behind it.
pub fn vfs_rmdir(path: &[u8]) -> VfsResult<()> {
    vfs_rmdir_at(path, b"/")
}

pub fn vfs_rmdir_at(path: &[u8], cwd: &[u8]) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, RESOLVE_NOFOLLOW_FINAL) {
        return Err(VfsError::PermissionDenied);
    }
    // Keyed on the path the walk ends on, not a lexical canonicalisation: `..`
    // after a symlink names a different directory, and the mount table is
    // keyed on the real one.
    if let Ok((_, canon)) = resolve_path_canon_at(path, cwd, RESOLVE_NOFOLLOW_FINAL)
        && crate::vfs::mount::mount_at(canon.as_bytes()).is_some()
    {
        return Err(VfsError::Busy);
    }
    let (parent, name) = resolve_parent_at(path, cwd)?;
    parent.check_writable()?;
    // Only a regular file can carry a page set while `mmap` refuses every
    // other type, and that is not a rule to leave load-bearing.
    if let Ok(inode) = parent.fs.lookup(parent.inode, name.as_bytes()) {
        if !inode_is_directory(parent.fs, inode)? {
            return Err(VfsError::NotDirectory);
        }
        crate::filemap::detach_inode(parent.fs, inode);
    }
    parent.fs.rmdir(parent.inode, name.as_bytes())
}

/// Create a symlink at `link_path` pointing at `target`.
///
/// The seal is checked on `link_path` as it is for every other mutator: a
/// symlink written over a sealed name would redirect the path a privilege
/// grant is keyed on. The filesystem refuses a duplicate name underneath
/// (`Ext2Fs::create_inode_entry`), so this is defence in depth rather than the
/// only barrier — which is exactly what the seal warrants.
pub fn vfs_symlink(target: &[u8], link_path: &[u8]) -> VfsResult<()> {
    vfs_symlink_at(target, link_path, b"/")
}

pub fn vfs_symlink_at(target: &[u8], link_path: &[u8], cwd: &[u8]) -> VfsResult<()> {
    if target.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if target.len() > crate::MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }
    if path_is_sealed_at(link_path, cwd, RESOLVE_NOFOLLOW_FINAL) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent_at(link_path, cwd)?;
    check_name_len(name.as_bytes())?;
    parent.check_writable()?;
    parent
        .fs
        .symlink(parent.inode, name.as_bytes(), target)
        .map(|_| ())
}

pub fn vfs_readlink_at(path: &[u8], cwd: &[u8], buf: &mut [u8]) -> VfsResult<usize> {
    let resolved = resolve_path_at(path, cwd, RESOLVE_NOFOLLOW_FINAL)?;
    let stat = resolved.fs.stat(resolved.inode)?;
    if stat.file_type != FileType::Symlink {
        return Err(VfsError::InvalidArgument);
    }
    resolved.fs.readlink(resolved.inode, buf)
}

pub fn vfs_rename(old_path: &[u8], new_path: &[u8]) -> VfsResult<()> {
    vfs_rename_at(old_path, b"/", new_path, b"/")
}

pub fn vfs_rename_at(
    old_path: &[u8],
    old_cwd: &[u8],
    new_path: &[u8],
    new_cwd: &[u8],
) -> VfsResult<()> {
    // Both ends: renaming a sealed file moves it out from under the path its
    // privilege is keyed on, and renaming over one replaces it just as a write
    // would.
    if path_is_sealed_at(old_path, old_cwd, RESOLVE_NOFOLLOW_FINAL)
        || path_is_sealed_at(new_path, new_cwd, RESOLVE_NOFOLLOW_FINAL)
    {
        return Err(VfsError::PermissionDenied);
    }
    let (old_parent, old_name) = resolve_parent_at(old_path, old_cwd)?;
    let (new_parent, new_name) = resolve_parent_at(new_path, new_cwd)?;
    check_name_len(new_name.as_bytes())?;

    if !same_filesystem(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    old_parent.check_writable()?;
    new_parent.check_writable()?;

    rename_resolved(
        &old_parent,
        old_name.as_bytes(),
        &new_parent,
        new_name.as_bytes(),
    )
}

#[inline(never)]
fn rename_resolved(
    old_parent: &ResolvedPath,
    old_name: &[u8],
    new_parent: &ResolvedPath,
    new_name: &[u8],
) -> VfsResult<()> {
    use crate::vfs::orphan::{DetachPlan, RemovalOutcome, begin_removal, end_removal};

    // Renaming *over* an open file is the same hazard as unlinking one: the
    // displaced name was that inode's last, and freeing it hands a live
    // reader's blocks away. A destination that names nothing, or nothing open,
    // takes the plain path.
    let displaced = match new_parent.fs.lookup(new_parent.inode, new_name) {
        Ok(displaced) => displaced,
        Err(VfsError::NotFound) => {
            return old_parent
                .fs
                .rename(old_parent.inode, old_name, new_parent.inode, new_name);
        }
        Err(e) => return Err(e),
    };

    let linked = detach_unless_linked(new_parent.fs, displaced);

    if begin_removal(new_parent.fs, displaced) == DetachPlan::FreeNow {
        let result = old_parent
            .fs
            .rename(old_parent.inode, old_name, new_parent.inode, new_name);
        let _ = end_removal(new_parent.fs, displaced, RemovalOutcome::Nothing);
        forget_if_freed(new_parent.fs, displaced, linked);
        return result;
    }

    let result =
        old_parent
            .fs
            .rename_detaching(old_parent.inode, old_name, new_parent.inode, new_name);
    forget_if_freed(new_parent.fs, displaced, linked);
    let outcome = match result {
        Ok(Some(_)) => RemovalOutcome::Deferred,
        _ => RemovalOutcome::Nothing,
    };
    if end_removal(new_parent.fs, displaced, outcome) {
        crate::vfs::orphan::drain_or_wake(new_parent.fs);
    }
    result.map(|_| ())
}

/// Commits every mount, returning the first error. Snapshots the table and
/// drops its `IrqRwLock` before the first `sync`: ext2's takes a sleeping
/// mutex, which must not be acquired under it.
pub fn vfs_sync_all() -> VfsResult<()> {
    // Before the mounts: page-set writeback reaches a filesystem through
    // `write`, so it must land before that filesystem's `sync`.
    crate::filemap::flush_all();
    let mut snapshot: [Option<&'static dyn crate::vfs::FileSystem>; MAX_MOUNTS] =
        [None; MAX_MOUNTS];
    let mut n = 0usize;
    with_mount_table(|table| {
        table.for_each_mount(&mut |fs| {
            if n < snapshot.len() {
                snapshot[n] = Some(fs);
                n += 1;
            }
        });
    });

    let mut first_err = None;
    for fs in snapshot.iter().take(n).flatten() {
        if let Err(e) = fs.sync() {
            first_err.get_or_insert(e);
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// One-shot listing: the first page only, so a directory larger than `entries`
/// is cut off at the buffer. A caller that must see every entry uses
/// [`vfs_list_from`] and carries its cursor.
pub fn vfs_list(path: &[u8], entries: &mut [UserFsEntry]) -> VfsResult<usize> {
    let mut cursor = ListCursor::start();
    vfs_list_from(path, entries, &mut cursor)
}

/// Where a paged listing resumes. Opaque to userland: the filesystem chooses
/// what its cookie means, and the mount-point pass carries the identity of the
/// last mount it emitted, because those entries are synthesised by the VFS
/// rather than read from the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListCursor {
    /// The filesystem's own resumption point, or [`Self::DIR_DONE`] once the
    /// directory itself is exhausted and only mount entries remain.
    fs_cookie: u64,
    /// The last child mount emitted, 0 before the first. Keyed on the mount's
    /// identity, not its table position: a released slot is reused at once, so
    /// an ordinal would drop or repeat entries between pages.
    last_mount_id: u32,
    done: bool,
}

impl ListCursor {
    const DIR_DONE: u64 = u64::MAX;
    /// Set in the packed form once the filesystem walk is done. A filesystem
    /// cookie is refused rather than truncated if it would collide.
    const MOUNT_PHASE: u64 = 1 << 63;

    pub const fn start() -> Self {
        Self {
            fs_cookie: 0,
            last_mount_id: 0,
            done: false,
        }
    }

    pub fn is_end(&self) -> bool {
        self.done
    }

    /// Pack into the single opaque `u64` the ABI carries. A mount id is 32
    /// bits wide, so `MOUNT_PHASE | id` can never collide with
    /// [`slopos_abi::fs::FS_LIST_CURSOR_END`]'s all-ones.
    pub fn to_abi(self) -> u64 {
        if self.done {
            return slopos_abi::fs::FS_LIST_CURSOR_END;
        }
        if self.fs_cookie == Self::DIR_DONE {
            return Self::MOUNT_PHASE | (self.last_mount_id as u64);
        }
        self.fs_cookie
    }

    pub fn from_abi(raw: u64) -> Self {
        if raw == slopos_abi::fs::FS_LIST_CURSOR_END {
            return Self {
                fs_cookie: Self::DIR_DONE,
                last_mount_id: 0,
                done: true,
            };
        }
        if raw & Self::MOUNT_PHASE != 0 {
            return Self {
                fs_cookie: Self::DIR_DONE,
                last_mount_id: (raw & 0xFFFF_FFFF) as u32,
                done: false,
            };
        }
        Self {
            fs_cookie: raw,
            last_mount_id: 0,
            done: false,
        }
    }
}

/// Drop from `page` every name a child mount of `dir` shadows, compacting the
/// survivors to the front and answering how many remain. The mount pass is
/// then the single authority for those names across every page.
///
/// A stored name is never clipped — the assertion above pins
/// `MAX_NAME_LEN + 1` inside the entry's buffer — so both passes compare the
/// same bytes.
fn drop_shadowed_names(mt: &MountTable, dir: &[u8], page: &mut [UserFsEntry]) -> usize {
    let mut kept = 0usize;
    for i in 0..page.len() {
        let cap = page[i].name.len();
        let elen = page[i].name.iter().position(|&b| b == 0).unwrap_or(cap);
        if mt.has_child_mount(dir, &page[i].name[..elen]) {
            continue;
        }
        if kept != i {
            let entry = page[i];
            page[kept] = entry;
        }
        kept += 1;
    }
    kept
}

/// One filesystem page of a listing.
///
/// The mount table is taken only *after* the filesystem calls have returned:
/// `MOUNT_TABLE` is an `IrqRwLock` at `LOCK_LEVEL_REGISTRY` — IRQs and
/// preemption off — and ext2's own lock is a sleeping mutex, so the table must
/// not be held across block I/O. Never inlined so its frame is not charged to
/// [`vfs_list_from`]'s.
#[inline(never)]
fn list_fs_page(
    resolved: &ResolvedPath,
    dir: &[u8],
    entries: &mut [UserFsEntry],
    inodes: &mut KVec<u64>,
    cursor: &mut ListCursor,
) -> VfsResult<usize> {
    let max = entries.len();
    let mut filled = 0usize;

    resolved.fs.readdir_cookie(
        resolved.inode,
        cursor.fs_cookie,
        &mut |next, name, inode, file_type| {
            // The callback's own bound, not merely the `filled < max`
            // return below: correctness must not rest on every filesystem
            // honouring a stop request promptly, because an index past the
            // end panics the kernel in a `forbid(unsafe_code)` crate.
            if filled >= max {
                return false;
            }
            let entry = &mut entries[filled];
            *entry = UserFsEntry::new();

            let nlen = name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&name[..nlen]);
            entry.name[nlen] = 0;

            entry.type_ = file_type.to_fs_type();

            inodes[filled] = inode;
            filled += 1;
            // Advanced past this entry *before* the buffer-full check, so
            // a resumed call does not repeat it.
            cursor.fs_cookie = next;
            filled < max
        },
    )?;

    if filled < max {
        cursor.fs_cookie = ListCursor::DIR_DONE;
    } else if cursor.fs_cookie >= ListCursor::MOUNT_PHASE {
        // A cookie that cannot round-trip through the ABI would resume the
        // walk somewhere else entirely.
        return Err(VfsError::InvalidArgument);
    }

    for i in 0..filled {
        if let Ok(child_stat) = resolved.fs.stat(inodes[i]) {
            entries[i].size = child_stat.size;
        }
    }

    Ok(with_mount_table(|mt| {
        drop_shadowed_names(mt, dir, &mut entries[..filled])
    }))
}

/// Fill `entries` from `cursor`, advancing it to where the next call resumes.
///
/// Answers how many entries were written. A buffer that fills mid-directory is
/// neither an error nor a truncation: the cursor names the next entry, which
/// is what makes a directory of any size listable. The listing ends when
/// `cursor.is_end()`.
pub fn vfs_list_from(
    path: &[u8],
    entries: &mut [UserFsEntry],
    cursor: &mut ListCursor,
) -> VfsResult<usize> {
    vfs_list_from_at(path, b"/", entries, cursor)
}

pub fn vfs_list_from_at(
    path: &[u8],
    cwd: &[u8],
    entries: &mut [UserFsEntry],
    cursor: &mut ListCursor,
) -> VfsResult<usize> {
    // One walk for both: the mount table is keyed on the canonical path the
    // walk ends on, so a listing of `//tmp` or of a symlink to it must ask
    // about `/tmp` to see that directory's child mounts.
    let (resolved, canon) = resolve_path_canon_at(path, cwd, RESOLVE_FOLLOW)?;
    let stat = resolved.fs.stat(resolved.inode)?;

    if stat.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }
    if entries.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    let dir = canon.as_bytes();

    let max = entries.len();
    // Sized with the buffer rather than fixed at 64: this holds the inode of
    // every entry written, which the second `stat` pass reads back.
    let mut inodes = KVec::<u64>::zeroed(max).map_err(|_| VfsError::NoSpace)?;

    let mut count = 0usize;
    while cursor.fs_cookie != ListCursor::DIR_DONE {
        count = list_fs_page(&resolved, dir, entries, &mut inodes, cursor)?;
        // An all-shadowed page must not go back empty: an empty page is how a
        // finished listing looks to a caller. Bounded, since a whole listing
        // can drop at most `MAX_MOUNTS` names.
        if count > 0 {
            break;
        }
    }

    if cursor.fs_cookie != ListCursor::DIR_DONE {
        return Ok(count);
    }

    // Mount points appear as directory entries in the parent listing even when
    // the underlying filesystem has no matching entry (Linux VFS behaviour).
    let mut exhausted = true;
    with_mount_table(|mt| {
        mt.for_each_child_mount_from(dir, cursor.last_mount_id, &mut |id, child_name| {
            if count >= max {
                exhausted = false;
                return false;
            }
            let entry = &mut entries[count];
            *entry = UserFsEntry::new();
            let nlen = child_name.len().min(entry.name.len() - 1);
            entry.name[..nlen].copy_from_slice(&child_name[..nlen]);
            entry.name[nlen] = 0;
            // A mount point always lists as a directory, whatever the entry it
            // shadows was.
            entry.type_ = FS_TYPE_DIRECTORY;
            entry.size = 0;
            count += 1;
            cursor.last_mount_id = id;
            true
        });
    });

    if exhausted {
        cursor.done = true;
    }

    Ok(count)
}
