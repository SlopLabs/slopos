use crate::vfs::canon::CanonPath;
use crate::vfs::mount::{MAX_MOUNTS, with_mount_table};
use crate::vfs::path::{
    RESOLVE_FOLLOW, RESOLVE_NOFOLLOW_FINAL, ResolvedPath, resolve_parent_at, resolve_path,
    resolve_path_at, resolve_path_canon_at,
};
use crate::vfs::perm::{InodePerm, Perm, Subject};
use crate::vfs::traits::{
    FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult, same_filesystem,
};

pub struct VfsHandle {
    pub inode: InodeId,
    pub fs: &'static dyn crate::vfs::FileSystem,
    pub file_type: FileType,
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

#[derive(Clone, Copy)]
pub struct VfsOpenFlags {
    pub create: bool,
    pub exclusive: bool,
    pub truncate: bool,
    pub readable: bool,
    pub writable: bool,
}

impl VfsOpenFlags {
    pub const fn read_only() -> Self {
        Self {
            create: false,
            exclusive: false,
            truncate: false,
            readable: true,
            writable: false,
        }
    }

    pub const fn create_only() -> Self {
        Self {
            create: true,
            exclusive: false,
            truncate: false,
            readable: false,
            writable: true,
        }
    }
}

/// One creation-time limit whatever the filesystem is.
fn check_name_len(name: &[u8]) -> VfsResult<()> {
    if name.len() > crate::MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

/// The kernel's own opens, which no mode bit refuses.
pub fn vfs_open(path: &[u8], create: bool) -> VfsResult<VfsHandle> {
    vfs_open_flags(
        path,
        VfsOpenFlags {
            create,
            exclusive: false,
            truncate: false,
            readable: true,
            writable: create,
        },
    )
}

pub fn vfs_open_flags(path: &[u8], flags: VfsOpenFlags) -> VfsResult<VfsHandle> {
    vfs_open_flags_at(path, b"/", flags, RESOLVE_FOLLOW, &Subject::KERNEL, None)
}

/// Open `path` as `who`. A file this call creates takes `create_mode`, which
/// the bits it is opened for are not checked against.
pub fn vfs_open_flags_at(
    path: &[u8],
    cwd: &[u8],
    flags: VfsOpenFlags,
    resolve_flags: u32,
    who: &Subject,
    create_mode: Option<u16>,
) -> VfsResult<VfsHandle> {
    match resolve_path_at(path, cwd, resolve_flags, who) {
        Ok(resolved) => {
            if flags.create && flags.exclusive {
                return Err(VfsError::AlreadyExists);
            }
            let stat = resolved.fs.stat(resolved.inode)?;
            match stat.file_type {
                FileType::Directory => return Err(VfsError::IsDirectory),
                FileType::Symlink => return Err(VfsError::TooManySymlinks),
                FileType::Socket => return Err(VfsError::NoDevice),
                _ => {}
            }
            if flags.readable {
                resolved.perm.check(who, Perm::Read)?;
            }
            if flags.writable {
                resolved.perm.check(who, Perm::Write)?;
            }
            // Refused at open, not at the first write: a descriptor obtained
            // before the check is a descriptor that outlives it. A FIFO's
            // writer stores nothing on the filesystem, so a read-only mount
            // does not refuse it.
            if flags.writable {
                if stat.file_type != FileType::Pipe {
                    resolved.check_writable()?;
                }
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
                file_type: stat.file_type,
                writable: flags.writable,
            })
        }
        Err(VfsError::NotFound) if flags.create => {
            let (parent, name) = resolve_parent_at(path, cwd, who)?;
            check_name_len(name.as_bytes())?;
            parent.check_writable()?;
            parent.perm.check_entries(who)?;
            let mode = create_mode.unwrap_or(FileType::Regular.default_mode());
            let new_inode = parent.fs.create_owned(
                parent.inode,
                name.as_bytes(),
                FileType::Regular,
                parent.perm.owner_for_child(who),
                parent.perm.mode_for_child(who, mode, false),
            )?;
            Ok(VfsHandle {
                inode: new_inode,
                fs: parent.fs,
                file_type: FileType::Regular,
                writable: flags.writable,
            })
        }
        Err(e) => Err(e),
    }
}

pub fn vfs_stat(path: &[u8]) -> VfsResult<FileStat> {
    vfs_stat_at(path, b"/", RESOLVE_FOLLOW, &Subject::KERNEL)
}

pub fn vfs_stat_at(path: &[u8], cwd: &[u8], flags: u32, who: &Subject) -> VfsResult<FileStat> {
    let resolved = resolve_path_at(path, cwd, flags, who)?;
    resolved.fs.stat(resolved.inode)
}

pub fn vfs_mkdir(path: &[u8]) -> VfsResult<()> {
    vfs_mkdir_at(path, b"/", None, &Subject::KERNEL)
}

/// `mkdirat(2)`.
pub fn vfs_mkdir_at(path: &[u8], cwd: &[u8], mode: Option<u16>, who: &Subject) -> VfsResult<()> {
    create_node_at(path, cwd, FileType::Directory, mode, who)
}

/// `mknodat(2)` for a regular file or FIFO, the only node kinds stored here.
pub fn vfs_mknod_at(
    path: &[u8],
    cwd: &[u8],
    file_type: FileType,
    mode: u16,
    who: &Subject,
) -> VfsResult<()> {
    if !matches!(file_type, FileType::Regular | FileType::Pipe) {
        return Err(VfsError::NotSupported);
    }
    create_node_at(path, cwd, file_type, Some(mode), who)
}

fn create_node_at(
    path: &[u8],
    cwd: &[u8],
    file_type: FileType,
    mode: Option<u16>,
    who: &Subject,
) -> VfsResult<()> {
    let (parent, name) = resolve_parent_at(path, cwd, who)?;
    check_name_len(name.as_bytes())?;
    parent.check_writable()?;
    parent.perm.check_entries(who)?;
    let mode = mode.unwrap_or(file_type.default_mode());
    parent.fs.create_owned(
        parent.inode,
        name.as_bytes(),
        file_type,
        parent.perm.owner_for_child(who),
        parent
            .perm
            .mode_for_child(who, mode, file_type == FileType::Directory),
    )?;
    Ok(())
}

/// An `AF_UNIX` socket node.
#[derive(Clone, Copy)]
pub struct SocketNode {
    fs: &'static dyn FileSystem,
    inode: InodeId,
}

impl SocketNode {
    pub fn is(&self, other: &Self) -> bool {
        same_filesystem(self.fs, other.fs) && self.inode == other.inode
    }
}

/// A bound socket's node, held open like a file's, so its inode is neither
/// freed nor reused while the socket lives, whatever becomes of its names.
/// Dropped with no spinlock held: the last reference to an unlinked node
/// frees it.
pub struct SocketName(SocketNode);

impl SocketName {
    pub fn node(&self) -> SocketNode {
        self.0
    }
}

impl Drop for SocketName {
    fn drop(&mut self) {
        if crate::vfs::orphan::close_ref(self.0.fs, self.0.inode) {
            crate::vfs::orphan::drain_or_wake(self.0.fs);
        }
    }
}

/// `bind(2)`'s half: make the socket node `path` names as `who`, who must be
/// able to add a name to its directory. A name already there, a dangling
/// link included, is `AlreadyExists`.
pub fn vfs_make_socket_at(
    path: &[u8],
    cwd: &[u8],
    mode: u16,
    who: &Subject,
) -> VfsResult<SocketName> {
    let (parent, name) = resolve_parent_at(path, cwd, who)?;
    check_name_len(name.as_bytes())?;
    parent.check_writable()?;
    parent.perm.check_entries(who)?;
    let inode = parent.fs.create_owned(
        parent.inode,
        name.as_bytes(),
        FileType::Socket,
        parent.perm.owner_for_child(who),
        parent.perm.mode_for_child(who, mode, false),
    )?;
    match crate::vfs::orphan::open_ref(parent.fs, inode) {
        Ok(()) => Ok(SocketName(SocketNode {
            fs: parent.fs,
            inode,
        })),
        Err(crate::vfs::orphan::OpenRefError::Detached) => Err(VfsError::NotFound),
        Err(crate::vfs::orphan::OpenRefError::TableFull) => {
            let _ = vfs_unlink_at(path, cwd, who);
            Err(VfsError::NoSpace)
        }
    }
}

/// `connect(2)`'s half: the socket node `path` names, which `who` must be
/// able to reach and write, held until the caller has matched it, so it is no
/// other node by then. `None` when the name is not a socket's.
pub fn vfs_socket_at(path: &[u8], cwd: &[u8], who: &Subject) -> VfsResult<Option<SocketName>> {
    let resolved = resolve_path_at(path, cwd, RESOLVE_FOLLOW, who)?;
    if resolved.fs.stat(resolved.inode)?.file_type != FileType::Socket {
        return Ok(None);
    }
    resolved.perm.check(who, Perm::Write)?;
    crate::vfs::orphan::open_ref(resolved.fs, resolved.inode).map_err(|e| match e {
        crate::vfs::orphan::OpenRefError::Detached => VfsError::NotFound,
        crate::vfs::orphan::OpenRefError::TableFull => VfsError::NoSpace,
    })?;
    Ok(Some(SocketName(SocketNode {
        fs: resolved.fs,
        inode: resolved.inode,
    })))
}

pub fn vfs_set_mode(path: &[u8], mode: u16) -> VfsResult<()> {
    vfs_set_mode_at(path, b"/", mode, RESOLVE_FOLLOW, &Subject::KERNEL)
}

/// `chmod(2)`: the owner's alone.
pub fn vfs_set_mode_at(
    path: &[u8],
    cwd: &[u8],
    mode: u16,
    flags: u32,
    who: &Subject,
) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, flags, who) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path_at(path, cwd, flags, who)?;
    resolved.check_writable()?;
    resolved.perm.check_owner(who)?;
    resolved
        .fs
        .set_mode(resolved.inode, who.chmod_mode(mode, resolved.perm.gid))
}

/// `chown(2)`; `None` keeps an id.
pub fn vfs_set_owner_at(
    path: &[u8],
    cwd: &[u8],
    uid: Option<u32>,
    gid: Option<u32>,
    flags: u32,
    who: &Subject,
) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, flags, who) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path_at(path, cwd, flags, who)?;
    resolved.check_writable()?;
    set_owner(resolved.fs, resolved.inode, &resolved.perm, uid, gid, who)
}

/// The shared tail of `chown` and `fchown`: the rule, the owner, and the
/// set-ID bits a change of owner takes away.
pub fn set_owner(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: InodeId,
    perm: &InodePerm,
    uid: Option<u32>,
    gid: Option<u32>,
    who: &Subject,
) -> VfsResult<()> {
    perm.check_chown(who, uid, gid)?;
    if uid.is_none() && gid.is_none() {
        return Ok(());
    }
    fs.set_owner(inode, uid, gid)?;
    let mode = who.mode_after_chown(perm.mode, perm.gid, perm.is_dir);
    if mode != perm.mode {
        fs.set_mode(inode, mode)?;
    }
    Ok(())
}

/// Seal `path` against every future mutation; only a `Seal` holder clears it.
/// The kernel's own: the image builder and the initramfs unpack.
pub fn vfs_set_sealed(path: &[u8]) -> VfsResult<()> {
    let resolved = resolve_path(path)?;
    resolved.check_writable()?;
    resolved.fs.set_sealed(resolved.inode)
}

/// What a `utimensat` asks to set, which decides who may.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimesChange {
    /// Both times `UTIME_OMIT`: nothing, and nothing to check.
    None,
    /// Both times to now: the owner or anyone who may write the file.
    Now,
    /// Any other time: the owner's alone.
    Explicit,
}

impl TimesChange {
    fn check(self, perm: &InodePerm, who: &Subject) -> VfsResult<()> {
        match self {
            Self::None => Ok(()),
            Self::Now if who.owns(perm.uid) => Ok(()),
            Self::Now => perm.check(who, Perm::Write),
            Self::Explicit => perm.check_owner(who),
        }
    }
}

/// Set an inode's times; `None` leaves a field alone, which is `UTIME_OMIT`.
pub fn vfs_utimens(
    path: &[u8],
    cwd: &[u8],
    atime: Option<crate::vfs::Timestamp>,
    mtime: Option<crate::vfs::Timestamp>,
    change: TimesChange,
    flags: u32,
    who: &Subject,
) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, flags, who) {
        return Err(VfsError::PermissionDenied);
    }
    let resolved = resolve_path_at(path, cwd, flags, who)?;
    resolved.check_writable()?;
    change.check(&resolved.perm, who)?;
    resolved.fs.set_times(resolved.inode, atime, mtime)
}

/// [`vfs_utimens`] for an open descriptor — `futimens(2)`. No mount flag is
/// reachable from `(fs, inode)`, so the read-only refusal is the descriptor
/// layer's.
pub fn vfs_set_times(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: InodeId,
    atime: Option<crate::vfs::Timestamp>,
    mtime: Option<crate::vfs::Timestamp>,
    change: TimesChange,
    who: &Subject,
) -> VfsResult<()> {
    change.check(&InodePerm::of(&fs.stat(inode)?), who)?;
    fs.set_times(inode, atime, mtime)
}

/// `link(2)`: a second name for an existing inode.
pub fn vfs_link(old_path: &[u8], new_path: &[u8], cwd: &[u8]) -> VfsResult<()> {
    vfs_link_at(old_path, cwd, new_path, cwd, false, &Subject::KERNEL)
}

/// `linkat(2)`. `follow` is `AT_SYMLINK_FOLLOW`: without it a symlink source
/// is linked as itself.
///
/// Both ends are seal-checked: a link over a sealed name would give that path
/// a second, unsealed inode to reach.
pub fn vfs_link_at(
    old_path: &[u8],
    old_cwd: &[u8],
    new_path: &[u8],
    new_cwd: &[u8],
    follow: bool,
    who: &Subject,
) -> VfsResult<()> {
    let source_flags = if follow {
        RESOLVE_FOLLOW
    } else {
        RESOLVE_NOFOLLOW_FINAL
    };
    let source = resolve_path_at(old_path, old_cwd, source_flags, who)?;
    let stat = source.fs.stat(source.inode)?;
    // POSIX leaves a directory hard link implementation-defined and every
    // implementation refuses it: `..` cannot describe a graph.
    if stat.file_type == FileType::Directory {
        return Err(VfsError::PermissionDenied);
    }
    if stat.sealed {
        return Err(VfsError::PermissionDenied);
    }
    if path_is_sealed_at(new_path, new_cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        return Err(VfsError::PermissionDenied);
    }

    let (parent, name) = resolve_parent_at(new_path, new_cwd, who)?;
    check_name_len(name.as_bytes())?;
    if !same_filesystem(source.fs, parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    parent.check_writable()?;
    parent.perm.check_entries(who)?;
    parent.fs.link(parent.inode, name.as_bytes(), source.inode)
}

/// Whether `path` names a sealed inode.
///
/// Fails **closed**: a resolve or stat that errors for any reason other than
/// the path not existing answers "sealed". This gate is what keeps a sealed
/// file from being replaced, so an induced `IoError` or an out-of-memory in the
/// block cache must not read as permission. A path that genuinely resolves to
/// nothing is not sealed — the caller's own lookup reports that, and
/// `NotFound` is the one error that carries no ambiguity.
fn path_is_sealed_at(path: &[u8], cwd: &[u8], flags: u32, who: &Subject) -> bool {
    match resolve_path_at(path, cwd, flags, who).and_then(|r| r.fs.stat(r.inode)) {
        Ok(stat) => stat.sealed,
        // Decided by resolution before any inode is consulted, so a path that
        // trips them cannot be naming a sealed inode; answering "sealed" would
        // report `EACCES` for all five.
        Err(VfsError::InvalidPath)
        | Err(VfsError::NameTooLong)
        | Err(VfsError::NotFound)
        | Err(VfsError::NotDirectory)
        | Err(VfsError::TooManySymlinks)
        | Err(VfsError::PermissionDenied) => false,
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
    vfs_unlink_at(path, b"/", &Subject::KERNEL)
}

pub fn vfs_unlink_at(path: &[u8], cwd: &[u8], who: &Subject) -> VfsResult<()> {
    use crate::vfs::orphan::{DetachPlan, RemovalOutcome, begin_removal, end_removal};

    if path_is_sealed_at(path, cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent_at(path, cwd, who)?;
    let name = name.as_bytes();
    parent.check_writable()?;
    parent.perm.check(who, Perm::Exec)?;

    // A filesystem that reports its own `ENOENT` gives a better error than a
    // lookup here would.
    let inode = match parent.fs.lookup(parent.inode, name) {
        Ok(inode) => inode,
        Err(VfsError::NotFound) => {
            check_missing_removable(&parent, who)?;
            return parent.fs.unlink(parent.inode, name);
        }
        Err(e) => return Err(e),
    };
    parent.perm.check(who, Perm::Write)?;

    let victim = parent.fs.stat(inode)?;
    parent.perm.check_sticky(who, victim.uid)?;
    // `FileSystem::rmdir` defaults to `unlink`, so a filesystem drawing no
    // distinction of its own would let either call remove either kind.
    if victim.file_type == FileType::Directory {
        return Err(VfsError::IsDirectory);
    }

    let linked = detach_unless_linked(parent.fs, inode)?;

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
fn detach_unless_linked(
    fs: &'static dyn crate::vfs::FileSystem,
    inode: InodeId,
) -> VfsResult<bool> {
    if fs.stat(inode)?.nlink > 1 {
        return Ok(true);
    }
    crate::filemap::detach_inode(fs, inode);
    Ok(false)
}

/// A removal racing for the other name may have taken the last one from an
/// inode whose set was left keyed. Forget only: once no name holds the inode,
/// its blocks are not its own to flush into for long. A stat that fails counts
/// as freed, since a set left keyed on a freed number attaches to its reuse.
fn forget_if_freed(fs: &'static dyn crate::vfs::FileSystem, inode: InodeId, linked: bool) {
    if linked && !fs.stat(inode).is_ok_and(|stat| stat.nlink > 0) {
        crate::filemap::forget_inode(fs, inode);
    }
}

/// `rmdir(2)`: remove an empty directory. Refuses a mount point outright —
/// removing the directory a filesystem is mounted on would leave the mount
/// table naming a path with nothing behind it.
pub fn vfs_rmdir(path: &[u8]) -> VfsResult<()> {
    vfs_rmdir_at(path, b"/", &Subject::KERNEL)
}

pub fn vfs_rmdir_at(path: &[u8], cwd: &[u8], who: &Subject) -> VfsResult<()> {
    if path_is_sealed_at(path, cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        return Err(VfsError::PermissionDenied);
    }
    // Keyed on the path the walk ends on, not a lexical canonicalisation: `..`
    // after a symlink names a different directory, and the mount table is
    // keyed on the real one.
    match resolve_path_canon_at(path, cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        Ok((resolved, canon))
            if crate::vfs::mount::mount_at(canon.as_bytes()).is_some()
                || holds_pinned_mount(&resolved, &canon) =>
        {
            return Err(VfsError::Busy);
        }
        Ok(_) | Err(VfsError::NotFound) => {}
        Err(e) => return Err(e),
    }
    let (parent, name) = resolve_parent_at(path, cwd, who)?;
    parent.check_writable()?;
    parent.perm.check(who, Perm::Exec)?;
    // Only a regular file can carry a page set while `mmap` refuses every
    // other type, and that is not a rule to leave load-bearing.
    match parent.fs.lookup(parent.inode, name.as_bytes()) {
        Ok(inode) => {
            parent.perm.check(who, Perm::Write)?;
            let victim = parent.fs.stat(inode)?;
            parent.perm.check_sticky(who, victim.uid)?;
            if victim.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }
            crate::filemap::detach_inode(parent.fs, inode);
        }
        Err(VfsError::NotFound) => check_missing_removable(&parent, who)?,
        Err(e) => return Err(e),
    }
    parent.fs.rmdir(parent.inode, name.as_bytes())
}

/// A name that is not there is `ENOENT` to a caller who could not have
/// removed it either, as Linux looks a name up before it checks for write.
fn check_missing_removable(parent: &ResolvedPath, who: &Subject) -> VfsResult<()> {
    parent
        .perm
        .check(who, Perm::Write)
        .map_err(|_| VfsError::NotFound)
}

/// Create a symlink at `link_path` pointing at `target`.
///
/// The seal is checked on `link_path` as it is for every other mutator: a
/// symlink written over a sealed name would redirect every walk through it.
/// The filesystem refuses a duplicate name underneath
/// (`Ext4Fs::create_inode_entry`), so this is defence in depth rather than the
/// only barrier — which is exactly what the seal warrants.
pub fn vfs_symlink(target: &[u8], link_path: &[u8]) -> VfsResult<()> {
    vfs_symlink_at(target, link_path, b"/", &Subject::KERNEL)
}

pub fn vfs_symlink_at(target: &[u8], link_path: &[u8], cwd: &[u8], who: &Subject) -> VfsResult<()> {
    if target.is_empty() {
        return Err(VfsError::InvalidArgument);
    }
    if target.len() > crate::MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }
    if path_is_sealed_at(link_path, cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        return Err(VfsError::PermissionDenied);
    }
    let (parent, name) = resolve_parent_at(link_path, cwd, who)?;
    check_name_len(name.as_bytes())?;
    parent.check_writable()?;
    parent.perm.check_entries(who)?;
    parent
        .fs
        .symlink(
            parent.inode,
            name.as_bytes(),
            target,
            parent.perm.owner_for_child(who),
        )
        .map(|_| ())
}

pub fn vfs_readlink_at(path: &[u8], cwd: &[u8], buf: &mut [u8], who: &Subject) -> VfsResult<usize> {
    let resolved = resolve_path_at(path, cwd, RESOLVE_NOFOLLOW_FINAL, who)?;
    let stat = resolved.fs.stat(resolved.inode)?;
    if stat.file_type != FileType::Symlink {
        return Err(VfsError::InvalidArgument);
    }
    resolved.fs.readlink(resolved.inode, buf)
}

/// Whether moving what a walk reached would move a pinned mount point or a
/// directory on its path: by the path the walk ends on, or by identity for
/// one reached through another mount of the same filesystem.
fn holds_pinned_mount(resolved: &ResolvedPath, canon: &CanonPath) -> bool {
    crate::vfs::mount::leads_to_pinned_mount(canon.as_bytes())
        || crate::vfs::mount::is_pinned_dir(resolved.fs, resolved.inode)
}

fn leads_to_pinned_mount_at(path: &[u8], cwd: &[u8], who: &Subject) -> VfsResult<bool> {
    match resolve_path_canon_at(path, cwd, RESOLVE_NOFOLLOW_FINAL, who) {
        Ok((resolved, canon)) => Ok(holds_pinned_mount(&resolved, &canon)),
        Err(VfsError::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn vfs_rename(old_path: &[u8], new_path: &[u8]) -> VfsResult<()> {
    vfs_rename_at(old_path, b"/", new_path, b"/", &Subject::KERNEL)
}

pub fn vfs_rename_at(
    old_path: &[u8],
    old_cwd: &[u8],
    new_path: &[u8],
    new_cwd: &[u8],
    who: &Subject,
) -> VfsResult<()> {
    // Both ends: renaming a sealed file moves it out from under its path, and
    // renaming over one replaces it just as a write would.
    if path_is_sealed_at(old_path, old_cwd, RESOLVE_NOFOLLOW_FINAL, who)
        || path_is_sealed_at(new_path, new_cwd, RESOLVE_NOFOLLOW_FINAL, who)
    {
        return Err(VfsError::PermissionDenied);
    }
    if leads_to_pinned_mount_at(old_path, old_cwd, who)? {
        return Err(VfsError::Busy);
    }
    let (old_parent, old_name) = resolve_parent_at(old_path, old_cwd, who)?;
    let (new_parent, new_name) = resolve_parent_at(new_path, new_cwd, who)?;
    check_name_len(new_name.as_bytes())?;

    if !same_filesystem(old_parent.fs, new_parent.fs) {
        return Err(VfsError::CrossDevice);
    }
    old_parent.check_writable()?;
    new_parent.check_writable()?;
    check_rename(
        &old_parent,
        old_name.as_bytes(),
        &new_parent,
        new_name.as_bytes(),
        who,
    )?;

    rename_resolved(
        &old_parent,
        old_name.as_bytes(),
        &new_parent,
        new_name.as_bytes(),
    )
}

/// Both directories' entries, the sticky rule for the name moved and the name
/// it displaces, and the moved directory itself when its `..` changes.
#[inline(never)]
fn check_rename(
    old_parent: &ResolvedPath,
    old_name: &[u8],
    new_parent: &ResolvedPath,
    new_name: &[u8],
    who: &Subject,
) -> VfsResult<()> {
    old_parent.perm.check(who, Perm::Exec)?;
    new_parent.perm.check(who, Perm::Exec)?;
    let moved = InodePerm::of(
        &old_parent
            .fs
            .stat(old_parent.fs.lookup(old_parent.inode, old_name)?)?,
    );
    old_parent.perm.check(who, Perm::Write)?;
    new_parent.perm.check(who, Perm::Write)?;
    old_parent.perm.check_sticky(who, moved.uid)?;
    match new_parent.fs.lookup(new_parent.inode, new_name) {
        Ok(inode) => {
            let displaced = new_parent.fs.stat(inode)?;
            new_parent.perm.check_sticky(who, displaced.uid)?;
        }
        Err(VfsError::NotFound) => {}
        Err(e) => return Err(e),
    }
    if moved.is_dir && old_parent.inode != new_parent.inode {
        moved.check(who, Perm::Write)?;
    }
    Ok(())
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

    let linked = detach_unless_linked(new_parent.fs, displaced)?;

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
/// drops its `IrqRwLock` before the first `sync`: ext4's takes a sleeping
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
