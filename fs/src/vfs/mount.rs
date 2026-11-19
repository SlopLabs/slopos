use crate::vfs::canon::canonicalise;
use crate::vfs::traits::{FileSystem, InodeId, VfsError, VfsResult, same_filesystem};
use slopos_ostd::sync::{IrqRwLock, IrqRwLockWriteGuard, LOCK_LEVEL_REGISTRY};
use slopos_ostd::{KVec, lock_class};

/// Directories held where they are: those pinned mount points' paths pass
/// through, `/usr` and `/etc` for the base, `/media` for the install medium
/// and `/opt` for the extensions, and the kept directories and those on the
/// way to them.
const MAX_PINNED_DIRS: usize = 32;

/// Every mutation through this mount fails with `EROFS` at the VFS, before
/// the filesystem sees it.
pub const MOUNT_RDONLY: u32 = 1 << 0;

/// The kernel's own: `umount(2)` refuses it and `mount(2)` puts nothing at or
/// beneath it.
pub const MOUNT_PINNED: u32 = 1 << 1;

#[derive(Clone, Copy)]
pub struct Mounted {
    pub fs: &'static dyn FileSystem,
    /// The directory a walk enters at: the filesystem's root, or the directory
    /// of it a subtree mount names.
    pub root: InodeId,
    pub flags: u32,
    /// Identity of the *mount*, not of the filesystem: one filesystem mounted
    /// at two paths carries two ids.
    pub id: u32,
}

impl Mounted {
    pub fn read_only(&self) -> bool {
        self.flags & MOUNT_RDONLY != 0
    }

    pub fn pinned(&self) -> bool {
        self.flags & MOUNT_PINNED != 0
    }
}

pub struct MountPoint {
    path: KVec<u8>,
    fs: &'static dyn FileSystem,
    root: InodeId,
    flags: u32,
    id: u32,
}

impl MountPoint {
    fn path_bytes(&self) -> &[u8] {
        &self.path
    }

    fn mounted(&self) -> Mounted {
        Mounted {
            fs: self.fs,
            root: self.root,
            flags: self.flags,
            id: self.id,
        }
    }
}

#[derive(Clone, Copy)]
struct PinnedDir {
    fs: &'static dyn FileSystem,
    inode: InodeId,
}

pub struct MountTable {
    /// Sorted by path, so a walk finds a mount point by halving.
    mounts: KVec<MountPoint>,
    next_id: u32,
    pinned_dirs: [Option<PinnedDir>; MAX_PINNED_DIRS],
}

impl MountTable {
    const EMPTY: Self = Self {
        mounts: KVec::new(),
        next_id: 1,
        pinned_dirs: [None; MAX_PINNED_DIRS],
    };

    fn pins_dir(&self, fs: &'static dyn FileSystem, inode: InodeId) -> bool {
        self.pinned_dirs
            .iter()
            .flatten()
            .any(|dir| dir.inode == inode && same_filesystem(dir.fs, fs))
    }

    /// Monotonic within a boot and never reused, unlike a slot index, which
    /// the next mount takes the instant one is released. Exhaustion refuses
    /// the mount rather than wrapping; id 0 is no mount's.
    fn alloc_id(&mut self) -> VfsResult<u32> {
        let id = self.next_id;
        if id == u32::MAX {
            return Err(VfsError::NoSpace);
        }
        self.next_id = id + 1;
        Ok(id)
    }

    /// `path` is canonical, so `/tmp/` and `/tmp` cannot both occupy the
    /// table while only one of them is matchable by the per-component walk.
    /// Nothing here reaches the allocator: the table has room for it, and a
    /// refused path is handed back to be freed once the lock is let go.
    fn insert(
        &mut self,
        path: KVec<u8>,
        fs: &'static dyn FileSystem,
        root: InodeId,
        flags: u32,
    ) -> Result<(), (VfsError, KVec<u8>)> {
        let Err(at) = self.find(&path) else {
            return Err((VfsError::AlreadyExists, path));
        };
        if self.mounts.len() == self.mounts.capacity() {
            return Err((VfsError::NoSpace, path));
        }
        let id = match self.alloc_id() {
            Ok(id) => id,
            Err(e) => return Err((e, path)),
        };
        let point = MountPoint {
            path,
            fs,
            root,
            flags,
            id,
        };
        self.mounts
            .insert(at, point)
            .map_err(|_| (VfsError::NoSpace, KVec::new()))
    }

    fn find(&self, path: &[u8]) -> Result<usize, usize> {
        self.mounts
            .as_slice()
            .binary_search_by(|mp| mp.path_bytes().cmp(path))
    }

    fn slot_at(&self, path: &[u8]) -> Option<usize> {
        self.find(path).ok()
    }

    /// Once per mount point, so a filesystem mounted twice is visited twice.
    pub fn for_each_mount(&self, callback: &mut dyn FnMut(&'static dyn FileSystem)) {
        for mp in self.mounts.iter() {
            callback(mp.fs);
        }
    }
}

static MOUNT_TABLE: IrqRwLock<MountTable> = IrqRwLock::new(
    MountTable::EMPTY,
    lock_class!("MOUNT_TABLE", LOCK_LEVEL_REGISTRY),
);

/// `path` in memory of its own, made before the table is locked.
fn owned(path: &[u8]) -> VfsResult<KVec<u8>> {
    let mut owned = KVec::with_capacity(path.len()).map_err(|_| VfsError::NoSpace)?;
    owned
        .extend_from_slice(path)
        .map_err(|_| VfsError::NoSpace)?;
    Ok(owned)
}

/// The table, write-locked with room for one more mount: the lock disables
/// interrupts, so nothing under it reaches the allocator, and the table grows
/// outside it instead.
fn table_with_room() -> VfsResult<IrqRwLockWriteGuard<'static, MountTable>> {
    loop {
        let table = MOUNT_TABLE.write();
        if table.mounts.len() < table.mounts.capacity() {
            return Ok(table);
        }
        let want = (table.mounts.capacity() * 2).max(16);
        drop(table);
        let mut fresh = KVec::with_capacity(want).map_err(|_| VfsError::NoSpace)?;
        let mut table = MOUNT_TABLE.write();
        if table.mounts.capacity() < want {
            for point in table.mounts.drain(..) {
                let _ = fresh.push(point);
            }
            core::mem::swap(&mut table.mounts, &mut fresh);
        }
        drop(table);
    }
}

pub fn mount(path: &[u8], fs: &'static dyn FileSystem, flags: u32) -> VfsResult<()> {
    mount_subtree(path, fs, fs.root_inode(), flags)
}

/// Mount the directory `root` of `fs` at `path`, which then shows that
/// directory's contents: a view of part of a filesystem another path may
/// already show whole.
pub fn mount_subtree(
    path: &[u8],
    fs: &'static dyn FileSystem,
    root: InodeId,
    flags: u32,
) -> VfsResult<()> {
    let path = owned(canonicalise(path)?.as_bytes())?;
    let mut table = table_with_room()?;
    let inserted = table.insert(path, fs, root, flags);
    drop(table);
    inserted.map_err(|(e, _path)| e)
}

/// Move each `(path, root)`'s mount of `fs` to show `root`, as one step: all
/// of them, or `NotFound` and none when a path has no mount of `fs`.
pub fn switch_roots(fs: &'static dyn FileSystem, roots: &[(&[u8], InodeId)]) -> VfsResult<()> {
    let mut table = MOUNT_TABLE.write();
    let slot_of = |table: &MountTable, path: &[u8]| {
        table
            .slot_at(path)
            .filter(|&slot| same_filesystem(table.mounts[slot].fs, fs))
            .ok_or(VfsError::NotFound)
    };
    for (path, _) in roots {
        slot_of(&table, path)?;
    }
    for &(path, root) in roots {
        let slot = slot_of(&table, path)?;
        table.mounts.as_mut_slice()[slot].root = root;
    }
    Ok(())
}

/// Show `root` of `fs` at `path` with `flags` in place of the mount there,
/// as one step, when `replaces` takes the filesystem it shows: `Busy` when it
/// does not. Answers the mount replaced, `None` for a new mount.
pub fn mount_replacing(
    path: &[u8],
    fs: &'static dyn FileSystem,
    root: InodeId,
    flags: u32,
    replaces: impl Fn(&dyn FileSystem) -> bool,
) -> VfsResult<Option<Mounted>> {
    let path = owned(canonicalise(path)?.as_bytes())?;
    let mut table = table_with_room()?;
    let Some(slot) = table.slot_at(&path) else {
        let inserted = table.insert(path, fs, root, flags);
        drop(table);
        return inserted.map(|()| None).map_err(|(e, _path)| e);
    };
    let mp = &mut table.mounts.as_mut_slice()[slot];
    let shown = mp.mounted();
    if !replaces(shown.fs) {
        return Err(VfsError::Busy);
    }
    mp.fs = fs;
    mp.root = root;
    mp.flags = flags;
    drop(table);
    Ok(Some(shown))
}

/// Take away the mount at `path` when `takes` takes the filesystem it shows,
/// and answer that mount; `NotFound` when there is none it takes.
pub fn unmount_where(path: &[u8], takes: impl Fn(&dyn FileSystem) -> bool) -> VfsResult<Mounted> {
    let canon = canonicalise(path)?;
    let mut table = MOUNT_TABLE.write();
    let slot = table
        .slot_at(canon.as_bytes())
        .filter(|&slot| takes(table.mounts[slot].fs))
        .ok_or(VfsError::NotFound)?;
    let taken = table.mounts.remove(slot);
    drop(table);
    Ok(taken.mounted())
}

/// Hold the directory `inode` of `fs`, which a pinned mount point's path
/// passes through, where it is: [`is_pinned_dir`] answers for it by identity,
/// so a second mount of `fs` is no way around the pin.
pub fn pin_dir(fs: &'static dyn FileSystem, inode: InodeId) -> VfsResult<()> {
    let mut table = MOUNT_TABLE.write();
    if table.pins_dir(fs, inode) {
        return Ok(());
    }
    let slot = table
        .pinned_dirs
        .iter_mut()
        .find(|dir| dir.is_none())
        .ok_or(VfsError::NoSpace)?;
    *slot = Some(PinnedDir { fs, inode });
    Ok(())
}

pub fn is_pinned_dir(fs: &'static dyn FileSystem, inode: InodeId) -> bool {
    MOUNT_TABLE.read().pins_dir(fs, inode)
}

/// Whether `path` is a pinned mount point or leads to one: renaming or
/// removing it would let whatever the walk then finds at its name stand in
/// for the pinned mount.
pub fn leads_to_pinned_mount(path: &[u8]) -> bool {
    let guard = MOUNT_TABLE.read();
    guard.mounts.iter().any(|mp| {
        let mount = mp.path_bytes();
        mp.flags & MOUNT_PINNED != 0
            && mount.starts_with(path)
            && (mount.len() == path.len() || path == b"/" || mount[path.len()] == b'/')
    })
}

/// Whether a mount at `path` would cover a directory the kernel keeps: one
/// of [`KEPT_DIRS`](slopos_abi::fs::KEPT_DIRS), a directory on the way to it,
/// or anything beneath it.
pub fn covers_kept_dir(path: &[u8]) -> bool {
    let within = |inner: &[u8], outer: &[u8]| {
        inner.starts_with(outer) && (inner.len() == outer.len() || inner[outer.len()] == b'/')
    };
    slopos_abi::fs::KEPT_DIRS
        .iter()
        .any(|kept| within(path, kept.as_bytes()) || within(kept.as_bytes(), path))
}

/// Whether `path` is a pinned mount point or lies beneath one.
pub fn under_pinned_mount(path: &[u8]) -> bool {
    let guard = MOUNT_TABLE.read();
    guard.mounts.iter().any(|mp| {
        let mount = mp.path_bytes();
        mp.flags & MOUNT_PINNED != 0
            && path.starts_with(mount)
            && (path.len() == mount.len() || path[mount.len()] == b'/')
    })
}

pub fn unmount(path: &[u8]) -> VfsResult<()> {
    let canon = canonicalise(path)?;
    let mut table = MOUNT_TABLE.write();
    let slot = table.slot_at(canon.as_bytes()).ok_or(VfsError::NotFound)?;
    let taken = table.mounts.remove(slot);
    drop(table);
    drop(taken);
    Ok(())
}

pub fn with_mount_table<R>(f: impl FnOnce(&MountTable) -> R) -> R {
    let guard = MOUNT_TABLE.read();
    f(&guard)
}

/// The mount with the least id above `after`: a walk from 0 meets each
/// mount present throughout it once, whatever is mounted or taken away
/// meanwhile, and holds the table only for each step.
pub fn mount_after(after: u32) -> Option<Mounted> {
    let guard = MOUNT_TABLE.read();
    guard
        .mounts
        .iter()
        .filter(|mp| mp.id > after)
        .min_by_key(|mp| mp.id)
        .map(MountPoint::mounted)
}

/// The filesystem mounted **exactly** at `path`, if one is.
///
/// A per-component walk asks this at each step, so a mount crossed mid-path is
/// honoured rather than resolved inside the filesystem underneath it.
pub fn mount_at(path: &[u8]) -> Option<Mounted> {
    let guard = MOUNT_TABLE.read();
    let slot = guard.slot_at(path)?;
    Some(guard.mounts[slot].mounted())
}
