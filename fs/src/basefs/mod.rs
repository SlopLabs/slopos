//! The base: a read-only filesystem over a `newc` archive. The generation
//! booted is served where the loader left the boot module, which it keeps
//! mapped for the kernel's lifetime; each one activated since is a copy the
//! generation owns. The install medium is a second instance, over the
//! `install` module, with one generation.
//!
//! A generation is numbered from 1 and never renumbered within a boot, and
//! its number is the high half of every inode it serves, so an inode names
//! its generation for good: a file, a mapping or a program identity of one
//! generation never reads another's bytes. An extension's activation
//! ([`crate::extension`]) numbers its inodes the same way.

pub(crate) mod activate;
mod index;
pub mod monitor;
pub(crate) mod reap;
mod stage;
mod store;

use core::sync::atomic::{AtomicBool, Ordering};

use slopos_abi::Errno;
use slopos_abi::fs::{BASEFS_MAGIC, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use slopos_boot_core::newc;
use slopos_ostd::process::Program;
use slopos_ostd::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use slopos_ostd::{KArc, lock_class};

use self::index::{Data, Entry, Index, ROOT};
use self::store::Store;
use crate::MAX_NAME_LEN;
use crate::cpio::{CpioError, nul_terminated};
use crate::vfs::traits::FsStats;
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

pub use self::activate::{ActivateError, activate};
pub use self::stage::{Source, Staged, stage};

/// The generation the loader booted, which is never freed.
pub(crate) const BOOTED: u32 = 1;

/// The last number a generation may take: a program identity keeps the top
/// bit of an inode for itself.
const LAST_NUMBER: u32 = i32::MAX as u32;

/// Generations resident at once: the current one and those still held.
/// What their copies may take together is the caller's budget.
pub const MAX_GENERATIONS: usize = 16;

/// Why an archive could not be made a generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageError {
    Read(Errno),
    Archive(newc::Error),
    Cpio(CpioError),
    /// More bytes than the caller allows a base.
    TooLarge,
    /// More records, or nodes, than a base may hold.
    TooMany,
    NoMemory,
}

struct Generation {
    number: u32,
    index: Index,
    store: Store,
    archive_len: u64,
}

impl Generation {
    fn node(&self, inode: InodeId) -> VfsResult<&index::Node> {
        self.index
            .node(index_in(inode).ok_or(VfsError::NotFound)?)
            .ok_or(VfsError::NotFound)
    }

    fn read(&self, data: Data, offset: u64, buf: &mut [u8]) -> usize {
        match data {
            Data::Static(bytes) => {
                let start = usize::try_from(offset).map_or(bytes.len(), |o| o.min(bytes.len()));
                let n = (bytes.len() - start).min(buf.len());
                buf[..n].copy_from_slice(&bytes[start..start + n]);
                n
            }
            Data::Owned(span) => self.store.read(span, offset, buf),
        }
    }

    fn inode(&self, at: usize) -> InodeId {
        inode_of(self.number, at)
    }
}

struct Slot {
    generation: KArc<Generation>,
    /// Admitted and not yet made current: no reap pass may take it.
    admitted: bool,
    /// The last reap pass found nothing holding it.
    quiet: bool,
}

struct Table {
    /// 0 when none is: before the first, and for an activation no longer
    /// shown.
    current: u32,
    issued: u32,
    /// The generation kept for the boot whether or not it is current: the
    /// booted one, or 0 for none.
    kept: u32,
    slots: [Option<Slot>; MAX_GENERATIONS],
}

pub struct BaseFs {
    table: SpinLock<Table>,
    /// A generation other than the current one is resident, so a reap pass
    /// has something to look at.
    retired: AtomicBool,
}

/// The booted base, and each generation activated since.
pub static BASE_FS: BaseFs = BaseFs::new();

/// The install medium: the `install` module's archive, beside the kernel and
/// the base the loader booted.
pub static MEDIUM_FS: BaseFs = BaseFs::new();

/// Whether `fs` is the base, which is reachable only at the `BASE_DIRS` it
/// is mounted on.
pub fn is_base(fs: &dyn FileSystem) -> bool {
    crate::vfs::traits::same_filesystem(fs, &BASE_FS)
}

/// What a file of `fs` attests as a program: the base's or an activation's
/// own, by its inode, and `Other` for any other.
pub fn program_of(fs: &dyn FileSystem, inode: InodeId) -> Program {
    match core::num::NonZeroU64::new(inode) {
        Some(inode) if is_base(fs) => Program::Base(inode),
        Some(inode) if crate::extension::is_activation(fs) => Program::Extension(inode),
        _ => Program::Other,
    }
}

/// Write the absolute path `program` runs from in its own generation into
/// `buf` when it fits, and answer its length either way: `NotFound` for a
/// program of neither the base nor an activation, or whose generation has
/// gone.
pub fn program_path(program: Program, buf: &mut [u8]) -> VfsResult<usize> {
    match program {
        Program::Base(inode) => BASE_FS.path_of(inode.get(), buf),
        Program::Extension(inode) => {
            crate::extension::path_of(generation_of(inode.get()), inode.get(), buf)
        }
        Program::Other => Err(VfsError::NotFound),
    }
}

/// An open reference on a base or activation file for a reader that holds no
/// descriptor, exec, so the generation it reads stays resident past a switch.
pub struct Hold(Option<(&'static dyn FileSystem, InodeId)>);

impl Hold {
    pub fn of(fs: &'static dyn FileSystem, inode: InodeId) -> Self {
        let held = program_of(fs, inode) != Program::Other
            && crate::vfs::orphan::open_ref(fs, inode).is_ok();
        Self(held.then_some((fs, inode)))
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some((fs, inode)) = self.0 {
            let _ = crate::vfs::orphan::close_ref(fs, inode);
        }
    }
}

/// The generation an inode of a [`BaseFs`] belongs to.
pub const fn generation_of(inode: InodeId) -> u32 {
    (inode >> 32) as u32
}

const fn inode_of(generation: u32, at: usize) -> InodeId {
    ((generation as u64) << 32) | (at as u64 + 1)
}

fn index_in(inode: InodeId) -> Option<usize> {
    (inode as u32).checked_sub(1).map(|at| at as usize)
}

impl Staged {
    /// The interface `/usr/share/slopos/base-interface` names.
    pub fn interface(&self) -> Option<u32> {
        let at = self
            .index
            .resolve(slopos_boot_core::base::INTERFACE_PATH.as_bytes())?;
        let node = self.index.node(at)?;
        let Data::Owned(span) = node.data else {
            return None;
        };
        if node.kind != FileType::Regular || span.len() > 16 {
            return None;
        }
        let mut text = [0u8; 16];
        let n = self.store.read(span, 0, &mut text);
        slopos_boot_core::base::parse_interface(&text[..n]).ok()
    }
}

impl BaseFs {
    pub const fn new() -> Self {
        Self {
            table: SpinLock::new(
                Table {
                    current: 0,
                    issued: 0,
                    kept: 0,
                    slots: [const { None }; MAX_GENERATIONS],
                },
                lock_class!("BASEFS_GENERATIONS", LOCK_LEVEL_REGISTRY),
            ),
            retired: AtomicBool::new(false),
        }
    }

    fn note_retired(&self, table: &Table) {
        let retired = table.slots.iter().flatten().any(|slot| {
            !slot.admitted
                && slot.generation.number != table.current
                && slot.generation.number != table.kept
        });
        self.retired.store(retired, Ordering::Release);
    }

    fn generation(&self, number: u32) -> VfsResult<KArc<Generation>> {
        let table = self.table.lock();
        table
            .slots
            .iter()
            .flatten()
            .find(|slot| slot.generation.number == number)
            .map(|slot| slot.generation.clone())
            .ok_or(VfsError::NotFound)
    }

    fn of(&self, inode: InodeId) -> VfsResult<KArc<Generation>> {
        self.generation(generation_of(inode))
    }

    /// The generation the base directories show, 0 before the first.
    pub fn current_generation(&self) -> u32 {
        self.table.lock().current
    }

    /// Index `archive` as the first generation, with each of `files`, a path
    /// and the bytes it reads as, beside what the archive holds; answers the
    /// entries it holds, and [`CpioError::Installed`] once a generation is.
    /// A file the archive already holds is [`CpioError::BadPath`].
    pub fn install(
        &self,
        archive: &'static [u8],
        files: &[(&'static [u8], &'static [u8])],
    ) -> Result<usize, CpioError> {
        if self.table.lock().issued != 0 {
            return Err(CpioError::Installed);
        }
        let mut index = Index::new()?;
        for record in newc::entries(archive) {
            let record = record?;
            let kind = match record.mode & S_IFMT {
                S_IFDIR => FileType::Directory,
                S_IFREG => FileType::Regular,
                S_IFLNK => FileType::Symlink,
                _ => continue,
            };
            let size = match kind {
                FileType::Symlink => nul_terminated(record.data).len(),
                _ => record.data.len(),
            };
            let entry = Entry {
                kind,
                mode: (record.mode & 0o7777) as u16,
                data: Data::Static(record.data),
                size: size as u64,
            };
            index.insert(record.path, entry)?;
        }
        for &(path, data) in files {
            if index.resolve(path).is_some() {
                return Err(CpioError::BadPath);
            }
            let entry = Entry {
                kind: FileType::Regular,
                mode: 0o644,
                data: Data::Static(data),
                size: data.len() as u64,
            };
            index.insert(path, entry)?;
        }
        let entries = index.len() - 1;
        let generation = KArc::try_new(Generation {
            number: BOOTED,
            index,
            store: Store::default(),
            archive_len: archive.len() as u64,
        })
        .map_err(|_| CpioError::NoMemory)?;
        let mut table = self.table.lock();
        if table.issued != 0 {
            return Err(CpioError::Installed);
        }
        table.issued = BOOTED;
        table.current = BOOTED;
        table.kept = BOOTED;
        table.slots[0] = Some(Slot {
            generation,
            admitted: false,
            quiet: false,
        });
        Ok(entries)
    }

    /// Give `staged` the next number and a slot, not yet current: `Busy`
    /// when every slot is held, the numbers are spent, or the copies resident
    /// with it would take more than `budget` bytes.
    fn admit(&self, staged: Staged, budget: u64) -> Result<u32, ActivateError> {
        let mut generation = KArc::try_new(Generation {
            number: 0,
            archive_len: staged.store.len(),
            index: staged.index,
            store: staged.store,
        })
        .map_err(|_| ActivateError::NoMemory)?;
        let mut table = self.table.lock();
        let number = table.issued.checked_add(1).ok_or(ActivateError::Busy)?;
        let copied: u64 = table
            .slots
            .iter()
            .flatten()
            .map(|slot| slot.generation.store.len())
            .sum();
        if copied.saturating_add(generation.store.len()) > budget || number > LAST_NUMBER {
            return Err(ActivateError::Busy);
        }
        let slot = table
            .slots
            .iter()
            .position(Option::is_none)
            .ok_or(ActivateError::Busy)?;
        KArc::get_mut(&mut generation)
            .ok_or(ActivateError::NoMemory)?
            .number = number;
        table.issued = number;
        table.slots[slot] = Some(Slot {
            generation,
            admitted: true,
            quiet: false,
        });
        Ok(number)
    }

    fn directory(&self, number: u32, path: &[u8]) -> Option<InodeId> {
        let generation = self.generation(number).ok()?;
        let at = generation.index.resolve(path)?;
        (generation.index.node(at)?.kind == FileType::Directory).then(|| generation.inode(at))
    }

    fn make_current(&self, number: u32) {
        let mut table = self.table.lock();
        for slot in table.slots.iter_mut().flatten() {
            if slot.generation.number == number {
                slot.admitted = false;
            }
        }
        table.current = number;
        self.note_retired(&table);
    }

    fn withdraw(&self, number: u32) {
        let taken = {
            let mut table = self.table.lock();
            table
                .slots
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|s| s.generation.number == number))
                .and_then(Option::take)
        };
        drop(taken);
    }

    /// The inode at absolute `path` in the current generation.
    pub fn resolve(&self, path: &[u8]) -> VfsResult<InodeId> {
        let generation = self.generation(self.current_generation())?;
        let at = generation.index.resolve(path).ok_or(VfsError::NotFound)?;
        Ok(generation.inode(at))
    }

    /// Write `inode`'s absolute path in its own generation into `buf` when it
    /// fits, and answer its length either way.
    pub fn path_of(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let generation = self.of(inode)?;
        let index = &generation.index;
        let parent = |node: &index::Node| index.node(node.parent).ok_or(VfsError::NotFound);
        let mut len = 0;
        let mut at = generation.node(inode)?;
        while !index.name(at).is_empty() {
            len += 1 + index.name(at).len();
            at = parent(at)?;
        }
        if len == 0 {
            len = 1;
        }
        if let Some(out) = buf.get_mut(..len) {
            out[0] = b'/';
            let mut end = len;
            let mut at = generation.node(inode)?;
            while !index.name(at).is_empty() {
                let name = index.name(at);
                out[end - name.len()..end].copy_from_slice(name);
                end -= name.len() + 1;
                out[end] = b'/';
                at = parent(at)?;
            }
        }
        Ok(len)
    }

    /// The bytes of the regular file at `path` in the current generation,
    /// where the loader left them; `NotSupported` for a generation that
    /// copied them.
    pub fn file(&self, path: &[u8]) -> VfsResult<&'static [u8]> {
        let generation = self.generation(self.current_generation())?;
        let at = generation.index.resolve(path).ok_or(VfsError::NotFound)?;
        let node = generation.index.node(at).ok_or(VfsError::NotFound)?;
        match (node.kind, node.data) {
            (FileType::Regular, Data::Static(bytes)) => Ok(bytes),
            (FileType::Regular, Data::Owned(_)) => Err(VfsError::NotSupported),
            (FileType::Directory, _) => Err(VfsError::IsDirectory),
            _ => Err(VfsError::InvalidArgument),
        }
    }
}

impl Default for BaseFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for BaseFs {
    fn name(&self) -> &'static str {
        "basefs"
    }

    fn root_inode(&self) -> InodeId {
        inode_of(self.current_generation(), ROOT)
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        let generation = self.of(parent)?;
        let dir = generation.node(parent)?;
        if dir.kind != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }
        match name {
            b"." => Ok(parent),
            b".." => Ok(generation.inode(dir.parent)),
            _ => generation
                .index
                .child(index_in(parent).ok_or(VfsError::NotFound)?, name)
                .map(|at| generation.inode(at))
                .ok_or(VfsError::NotFound),
        }
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        let generation = self.of(inode)?;
        let node = generation.node(inode)?;
        let mut stat = match node.kind {
            FileType::Directory => FileStat::new_directory(inode),
            _ => FileStat::new_file(inode, node.size),
        };
        stat.file_type = node.kind;
        stat.mode = node.mode;
        stat.sealed = true;
        Ok(stat)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let generation = self.of(inode)?;
        let node = generation.node(inode)?;
        match node.kind {
            FileType::Regular => Ok(generation.read(node.data, offset, buf)),
            FileType::Directory => Err(VfsError::IsDirectory),
            _ => Err(VfsError::InvalidArgument),
        }
    }

    fn write(&self, _inode: InodeId, _offset: u64, _buf: &[u8]) -> VfsResult<usize> {
        Err(VfsError::ReadOnly)
    }

    fn create_owned(
        &self,
        _parent: InodeId,
        _name: &[u8],
        _file_type: FileType,
        _owner: crate::vfs::Owner,
        _mode: u16,
    ) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        let generation = self.of(inode)?;
        let dir = generation.node(inode)?;
        if dir.kind != FileType::Directory {
            return Err(VfsError::NotDirectory);
        }
        let index = &generation.index;
        let dots = [
            (&b"."[..], inode, FileType::Directory),
            (
                &b".."[..],
                generation.inode(dir.parent),
                FileType::Directory,
            ),
        ];
        let at = index_in(inode).ok_or(VfsError::NotFound)?;
        let entries = index.children(at).filter_map(|i| {
            let node = index.node(i)?;
            Some((index.name(node), generation.inode(i), node.kind))
        });
        let mut visited = 0;
        for (name, ino, kind) in dots.into_iter().chain(entries).skip(offset) {
            if !callback(name, ino, kind) {
                break;
            }
            visited += 1;
        }
        Ok(visited)
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        let generation = self.of(inode)?;
        let node = generation.node(inode)?;
        if node.kind != FileType::Symlink {
            return Err(VfsError::InvalidArgument);
        }
        let n = usize::try_from(node.size)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        Ok(generation.read(node.data, 0, &mut buf[..n]))
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn set_sealed(&self, _inode: InodeId) -> VfsResult<()> {
        Ok(())
    }

    fn statfs(&self) -> VfsResult<FsStats> {
        let generation = self.generation(self.current_generation())?;
        Ok(FsStats {
            magic: BASEFS_MAGIC,
            block_size: 4096,
            blocks: generation.archive_len.div_ceil(4096),
            blocks_free: 0,
            blocks_available: 0,
            inodes: generation.index.len() as u64,
            inodes_free: 0,
            max_name_len: MAX_NAME_LEN as u32,
            read_only: true,
        })
    }
}
