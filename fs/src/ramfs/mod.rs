use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_mm::page_alloc::get_page_allocator_stats;
use slopos_mm::slab::MAX_ALLOC_SIZE;
use slopos_ostd::{KBox, KVec};

use crate::MAX_NAME_LEN;
use crate::vfs::{FileStat, FileSystem, FileType, FsStats, InodeId, VfsError, VfsResult};
use slopos_ostd::sync::SpinLock;
use slopos_ostd::sync::lock_tracking::LockClassKey;

const PAGE_SIZE: u64 = 4096;

/// A file's bytes are held one page at a time: one chunk is one slab
/// allocation, so nothing about a file's storage is contiguous past a page.
pub(crate) const RAMFS_FILE_CHUNK: usize = PAGE_SIZE as usize;
type FileChunk = KBox<[u8; RAMFS_FILE_CHUNK]>;

/// Chunks one hold of the filesystem lock may install or give back.
///
/// `SpinLock::lock` disables interrupts, and a file at the ceiling is 131072
/// chunks: installing them in one hold — which one `ftruncate(2)` asks for —
/// would keep interrupts off across that many page allocations and 512 MiB of
/// zeroing. Allocation and freeing happen with no lock held, so a hold costs
/// this many pointer moves and one chunk's worth of zeroing.
pub(crate) const RAMFS_CHUNK_BATCH: usize = 32;

/// Chunks a symlink target can need: its body is bounded by `MAX_PATH_LEN`,
/// so one staging array covers every one of them.
const SYMLINK_CHUNKS: usize = (crate::MAX_PATH_LEN + RAMFS_FILE_CHUNK - 1) / RAMFS_FILE_CHUNK;

/// What this filesystem was fixed at before its bounds were derived from the
/// machine, kept as floors so a small one behaves exactly as it did.
pub(crate) const RAMFS_MIN_FILE_SIZE: usize = 16 * 1024 * 1024;
pub(crate) const RAMFS_MIN_INODES: usize = 4096;

/// A ramfs page is unswappable and nothing writes it anywhere, so what a file
/// holds is off the machine's budget until it is removed: one file may reach
/// an eighth of usable memory and no more.
pub(crate) const RAMFS_MEM_SHARE: u64 = 8;

/// The largest file this representation can describe, which is the chunk
/// index's own allocation and not any file's bytes: `MAX_ALLOC_SIZE` over one
/// pointer, rounded down to a power of two because the vector's growth
/// doubles — 131072 pages, 512 MiB.
pub(crate) const RAMFS_MAX_FILE_CEILING: usize = {
    let fits = MAX_ALLOC_SIZE / core::mem::size_of::<FileChunk>();
    (1usize << (usize::BITS - 1 - fits.leading_zeros())) * RAMFS_FILE_CHUNK
};

const _: () = assert!(RAMFS_MIN_FILE_SIZE <= RAMFS_MAX_FILE_CEILING);
const _: () = assert!(RAMFS_FILE_CHUNK <= 256 * 1024);
const _: () = assert!(
    (RAMFS_MAX_FILE_CEILING / RAMFS_FILE_CHUNK) * core::mem::size_of::<FileChunk>()
        <= MAX_ALLOC_SIZE
);

/// Inodes are budgeted by `mkfs`'s bytes-per-inode ratio: the table itself is
/// cheap, but each inode is a handle on more unswappable memory.
pub(crate) const RAMFS_BYTES_PER_INODE: u64 = 64 * 1024;

/// The pool grows on demand, but the bound keeps a malformed or hostile
/// initramfs from exhausting kernel memory. Its hard limit is the table's own
/// allocation: one contiguous `KVec<RamInode>` against `MAX_ALLOC_SIZE`, whose
/// growth doubles, so the largest reachable length is a power of two.
pub(crate) const RAMFS_MAX_INODES_CEILING: usize = {
    let fits = MAX_ALLOC_SIZE / core::mem::size_of::<RamInode>();
    1usize << (usize::BITS - 1 - fits.leading_zeros())
};

const _: () = assert!(RAMFS_MIN_INODES <= RAMFS_MAX_INODES_CEILING);

/// Derived once from the page allocator, then cached. Zero is "not derived
/// yet": an unseeded allocator gets the floor and is asked again next time.
static MAX_FILE_SIZE: AtomicUsize = AtomicUsize::new(0);
static MAX_INODES: AtomicUsize = AtomicUsize::new(0);

/// The seeded frames, not the highest index: reserved holes and the kernel
/// image never enter the buddy, and this sum survives allocation.
fn usable_bytes() -> Option<u64> {
    let stats = get_page_allocator_stats();
    let frames = u64::from(stats.free.saturating_add(stats.allocated));
    (frames != 0).then(|| frames * PAGE_SIZE)
}

/// Bytes one file may hold. Must be called with no ramfs lock held: the first
/// call takes the page allocator's own lock.
pub(crate) fn ramfs_max_file_size() -> usize {
    let cached = MAX_FILE_SIZE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let Some(usable) = usable_bytes() else {
        return RAMFS_MIN_FILE_SIZE;
    };
    let derived = derive_max_file_size(usable);
    MAX_FILE_SIZE.store(derived, Ordering::Relaxed);
    derived
}

/// Inodes one instance may hold; same locking rule as [`ramfs_max_file_size`].
pub(crate) fn ramfs_max_inodes() -> usize {
    let cached = MAX_INODES.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let Some(usable) = usable_bytes() else {
        return RAMFS_MIN_INODES;
    };
    let derived = derive_max_inodes(usable);
    MAX_INODES.store(derived, Ordering::Relaxed);
    derived
}

pub(crate) fn derive_max_file_size(usable_bytes: u64) -> usize {
    (usable_bytes / RAMFS_MEM_SHARE)
        .clamp(RAMFS_MIN_FILE_SIZE as u64, RAMFS_MAX_FILE_CEILING as u64) as usize
}

pub(crate) fn derive_max_inodes(usable_bytes: u64) -> usize {
    (usable_bytes / RAMFS_BYTES_PER_INODE)
        .clamp(RAMFS_MIN_INODES as u64, RAMFS_MAX_INODES_CEILING as u64) as usize
}

const ROOT_SLOT: usize = 1;

/// The name is heap-backed: at `MAX_NAME_LEN` = 255 an inline array would
/// pad the entry to 272 bytes and cap a directory near 3800 entries against
/// `MAX_ALLOC_SIZE`.
struct DirEntry {
    name: KVec<u8>,
    inode: InodeId,
}

impl DirEntry {
    fn new(name: &[u8], inode: InodeId) -> VfsResult<Self> {
        let mut stored = KVec::with_capacity(name.len()).map_err(|_| VfsError::NoSpace)?;
        stored
            .extend_from_slice(name)
            .map_err(|_| VfsError::NoSpace)?;
        Ok(Self {
            name: stored,
            inode,
        })
    }

    fn matches(&self, name: &[u8]) -> bool {
        self.name.as_slice() == name
    }
}

/// Slot index and generation packed into the [`InodeId`] a descriptor holds.
///
/// Slots are recycled the moment an inode is unlinked, so an id naming only a
/// slot would silently follow the *next* file created in it.
const INODE_GEN_SHIFT: u32 = 32;
const INODE_SLOT_MASK: u64 = 0xFFFF_FFFF;

#[inline]
fn pack_inode_id(slot: u64, generation: u32) -> InodeId {
    ((generation as u64) << INODE_GEN_SHIFT) | (slot & INODE_SLOT_MASK)
}

#[inline]
fn inode_slot(id: InodeId) -> usize {
    (id & INODE_SLOT_MASK) as usize
}

#[inline]
fn inode_generation(id: InodeId) -> u32 {
    (id >> INODE_GEN_SHIFT) as u32
}

/// A file's bytes, in page-sized chunks.
///
/// `len` is the file's size and the chunks cover it densely: `chunks.len()` is
/// `len` rounded up to a chunk every time the filesystem lock is dropped, and
/// every byte from `len` to the end of the last chunk is zero — so growing
/// back into a shrunk file reads zeros without clearing anything. A grow
/// therefore moves `len` batch by batch rather than once at the end, and a
/// shrink lowers it in step with the chunks it gives back.
struct FileData {
    chunks: KVec<FileChunk>,
    len: usize,
}

impl FileData {
    const fn new() -> Self {
        Self {
            chunks: KVec::new(),
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// What a shrink gives back: bytes held in chunks, not the file's size.
    #[cfg(feature = "tests")]
    fn resident_bytes(&self) -> usize {
        self.chunks.len() * RAMFS_FILE_CHUNK
    }

    /// Chunks `len` needs, which is how many the file holds.
    fn coverage(&self) -> usize {
        self.len.div_ceil(RAMFS_FILE_CHUNK)
    }

    /// Chunks a grow to `new_len` still needs.
    fn short_of(&self, new_len: usize) -> usize {
        new_len
            .div_ceil(RAMFS_FILE_CHUNK)
            .saturating_sub(self.chunks.len())
    }

    fn read(&self, offset: usize, buf: &mut [u8]) -> usize {
        if offset >= self.len {
            return 0;
        }
        let total = buf.len().min(self.len - offset);
        let mut done = 0;
        while done < total {
            let pos = offset + done;
            let within = pos % RAMFS_FILE_CHUNK;
            let n = (RAMFS_FILE_CHUNK - within).min(total - done);
            let chunk = &self.chunks.as_slice()[pos / RAMFS_FILE_CHUNK];
            buf[done..done + n].copy_from_slice(&chunk.as_ref()[within..within + n]);
            done += n;
        }
        total
    }

    /// Copy what the file's size already covers, answering how many bytes
    /// that was: the caller installs chunks for the rest and comes back.
    fn write_within(&mut self, offset: usize, src: &[u8]) -> usize {
        if offset >= self.len {
            return 0;
        }
        let total = src.len().min(self.len - offset);
        let mut done = 0;
        while done < total {
            let pos = offset + done;
            let within = pos % RAMFS_FILE_CHUNK;
            let n = (RAMFS_FILE_CHUNK - within).min(total - done);
            let chunk = &mut self.chunks.as_mut_slice()[pos / RAMFS_FILE_CHUNK];
            chunk.as_mut()[within..within + n].copy_from_slice(&src[done..done + n]);
            done += n;
        }
        total
    }

    /// Install staged chunks, taking the size they cover, and answer how many
    /// went in.
    ///
    /// Index room for the whole remainder is taken here, so a grow costs one
    /// index allocation however many batches it needs. Chunks are
    /// interchangeable zeroed pages, so they come off the back of `staged` and
    /// leave the rest a prefix the caller can offer again.
    fn install(&mut self, staged: &mut [Option<FileChunk>], new_len: usize) -> VfsResult<usize> {
        let short = self.short_of(new_len);
        let mut installed = 0;
        if short > 0 {
            self.chunks
                .try_reserve(short)
                .map_err(|_| VfsError::NoSpace)?;
            for slot in staged.iter_mut().rev() {
                if installed == short {
                    break;
                }
                let Some(chunk) = slot.take() else {
                    break;
                };
                if self.chunks.push(chunk).is_err() {
                    break;
                }
                installed += 1;
            }
        }
        self.len = self
            .len
            .max(new_len.min(self.chunks.len() * RAMFS_FILE_CHUNK));
        Ok(installed)
    }

    /// Take `new_len` as the size, zeroing the tail of the chunk that keeps
    /// the last byte. The chunks past it are [`Self::park_released`]'s.
    fn step_down(&mut self, new_len: usize) {
        self.len = new_len;
        let coverage = self.coverage();
        if coverage > 0 {
            let keep = new_len - (coverage - 1) * RAMFS_FILE_CHUNK;
            self.chunks.as_mut_slice()[coverage - 1].as_mut()[keep..].fill(0);
        }
    }

    /// Move the chunks `len` no longer covers into `parked`, answering how
    /// many: the caller drops them with no lock held, because freeing a
    /// file's worth of chunks is allocator work.
    fn park_released(&mut self, parked: &mut [Option<FileChunk>]) -> usize {
        let coverage = self.coverage();
        let mut moved = 0;
        for slot in parked.iter_mut() {
            if self.chunks.len() <= coverage {
                break;
            }
            let Some(chunk) = self.chunks.pop() else {
                break;
            };
            *slot = Some(chunk);
            moved += 1;
        }
        moved
    }

    /// Hand the whole body out, which is how an unlink gives a file's chunks
    /// and its index back without freeing either under the lock.
    #[must_use]
    fn take_body(&mut self) -> KVec<FileChunk> {
        self.len = 0;
        core::mem::replace(&mut self.chunks, KVec::new())
    }
}

/// Allocate up to `staged.len()` chunks into it, answering how many.
///
/// Called with no filesystem lock held: a page allocation and a page of
/// zeroing per chunk is exactly the work that must not run under it.
fn stage_chunks(staged: &mut [Option<FileChunk>]) -> usize {
    let mut staged_count = 0;
    for slot in staged.iter_mut() {
        probe::note_alloc();
        let Ok(chunk) = KBox::<[u8; RAMFS_FILE_CHUNK]>::zeroed() else {
            break;
        };
        *slot = Some(chunk);
        staged_count += 1;
    }
    staged_count
}

/// Where the chunk work ran, so a test can assert the lock discipline
/// instead of a timing. Nothing is counted without the `tests` feature.
pub(crate) mod probe {
    #[cfg(feature = "tests")]
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(feature = "tests")]
    static ALLOCS: AtomicUsize = AtomicUsize::new(0);
    #[cfg(feature = "tests")]
    static ALLOCS_IRQ_OFF: AtomicUsize = AtomicUsize::new(0);
    #[cfg(feature = "tests")]
    static MOST_PER_HOLD: AtomicUsize = AtomicUsize::new(0);

    /// One chunk allocation, and whether it ran with interrupts off — which
    /// is what holding the filesystem's spinlock across it would mean.
    #[cfg(feature = "tests")]
    pub(super) fn note_alloc() {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        if !slopos_ostd::cpu::x86_64::interrupts::are_interrupts_enabled() {
            ALLOCS_IRQ_OFF.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[cfg(not(feature = "tests"))]
    pub(super) fn note_alloc() {}

    /// Chunks installed or given back under one hold of the lock.
    #[cfg(feature = "tests")]
    pub(super) fn note_hold(chunks: usize) {
        MOST_PER_HOLD.fetch_max(chunks, Ordering::Relaxed);
    }

    #[cfg(not(feature = "tests"))]
    pub(super) fn note_hold(_chunks: usize) {}

    /// What the counters saw, for a test to assert against the batch bound.
    #[cfg(feature = "tests")]
    pub(crate) struct ChunkWork {
        pub allocs: usize,
        pub allocs_irq_off: usize,
        pub most_per_hold: usize,
    }

    /// Read the counters and clear them, so one test's figures are its own.
    #[cfg(feature = "tests")]
    pub(crate) fn take_chunk_work() -> ChunkWork {
        ChunkWork {
            allocs: ALLOCS.swap(0, Ordering::Relaxed),
            allocs_irq_off: ALLOCS_IRQ_OFF.swap(0, Ordering::Relaxed),
            most_per_hold: MOST_PER_HOLD.swap(0, Ordering::Relaxed),
        }
    }
}

struct RamInode {
    in_use: bool,
    file_type: FileType,
    data: FileData,
    dir_entries: KVec<DirEntry>,
    parent: InodeId,
    mode: u16,
    nlink: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
    /// Refuses every mutation once set; never cleared while the inode lives.
    sealed: bool,
    /// Bumped on every reset, so a stale id fails to resolve.
    generation: u32,
}

impl RamInode {
    fn new() -> Self {
        Self {
            in_use: false,
            file_type: FileType::Regular,
            data: FileData::new(),
            dir_entries: KVec::new(),
            parent: 0,
            mode: 0o644,
            nlink: 1,
            atime: 0,
            mtime: 0,
            ctime: 0,
            sealed: false,
            generation: 1,
        }
    }

    /// Answers the file's body, which the caller drops with no lock held:
    /// freeing a big file's chunks is allocator work.
    #[must_use]
    fn reset(&mut self) -> KVec<FileChunk> {
        self.in_use = false;
        self.file_type = FileType::Regular;
        let body = self.data.take_body();
        self.dir_entries.clear();
        self.dir_entries.shrink_to_fit();
        self.parent = 0;
        self.mode = 0o644;
        self.nlink = 1;
        self.atime = 0;
        self.mtime = 0;
        self.ctime = 0;
        self.sealed = false;
        self.generation = self.generation.wrapping_add(1);
        body
    }

    fn data_len(&self) -> usize {
        self.data.len()
    }

    fn dir_entry_count(&self) -> usize {
        self.dir_entries.len()
    }

    fn touch_modified(&mut self) {
        stamp(&mut self.mtime);
        stamp(&mut self.ctime);
    }

    fn add_dir_entry(&mut self, name: &[u8], inode: InodeId) -> VfsResult<()> {
        // Truncating instead would store a name no lookup can match, since
        // every comparison here tests the whole stored name.
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if name.is_empty() {
            return Err(VfsError::InvalidPath);
        }
        for entry in self.dir_entries.iter() {
            if entry.matches(name) {
                return Err(VfsError::AlreadyExists);
            }
        }

        let entry = DirEntry::new(name, inode)?;
        self.dir_entries
            .push(entry)
            .map_err(|_| VfsError::NoSpace)?;

        Ok(())
    }

    fn remove_dir_entry(&mut self, name: &[u8]) -> VfsResult<InodeId> {
        for i in 0..self.dir_entries.len() {
            if self.dir_entries[i].matches(name) {
                let inode = self.dir_entries[i].inode;
                self.dir_entries.swap_remove(i);
                return Ok(inode);
            }
        }
        Err(VfsError::NotFound)
    }

    fn lookup(&self, name: &[u8]) -> VfsResult<InodeId> {
        for entry in self.dir_entries.iter() {
            if entry.matches(name) {
                return Ok(entry.inode);
            }
        }
        Err(VfsError::NotFound)
    }
}

/// Stamp a timestamp only when the wall clock can answer, so a boot without
/// one leaves the field unset rather than claiming 1970, as ext2 does.
fn stamp(field: &mut u64) {
    if let Some(now) = slopos_kernel_services::clock::realtime_unix_secs() {
        *field = u64::from(now);
    }
}

struct RamFsInner {
    inodes: KVec<RamInode>,
    initialized: bool,
}

impl RamFsInner {
    fn ensure_initialized(&mut self) {
        if self.initialized {
            return;
        }
        // Index 0 is a reserved sentinel; inode slots start at ROOT_SLOT (1).
        while self.inodes.len() <= ROOT_SLOT {
            self.inodes.push(RamInode::new()).expect("ramfs: alloc");
        }
        self.initialized = true;

        let root_id = pack_inode_id(ROOT_SLOT as u64, self.inodes[ROOT_SLOT].generation);
        let root = &mut self.inodes[ROOT_SLOT];
        root.in_use = true;
        root.file_type = FileType::Directory;
        root.mode = 0o755;
        root.nlink = 2;
        root.parent = root_id;
        stamp(&mut root.atime);
        stamp(&mut root.mtime);
        stamp(&mut root.ctime);

        root.add_dir_entry(b".", root_id).ok();
        root.add_dir_entry(b"..", root_id).ok();
    }

    fn alloc_inode(&mut self, max_inodes: usize) -> VfsResult<InodeId> {
        for slot in (ROOT_SLOT + 1)..self.inodes.len() {
            if !self.inodes[slot].in_use {
                return Ok(pack_inode_id(slot as u64, self.inodes[slot].generation));
            }
        }
        if self.inodes.len() >= max_inodes {
            return Err(VfsError::NoSpace);
        }
        let slot = self.inodes.len();
        self.inodes
            .push(RamInode::new())
            .map_err(|_| VfsError::NoSpace)?;
        Ok(pack_inode_id(slot as u64, self.inodes[slot].generation))
    }

    fn get_inode(&self, id: InodeId) -> VfsResult<&RamInode> {
        let slot = inode_slot(id);
        if slot >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &self.inodes[slot];
        if !inode.in_use || inode.generation != inode_generation(id) {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    fn get_inode_mut(&mut self, id: InodeId) -> VfsResult<&mut RamInode> {
        let slot = inode_slot(id);
        if slot >= self.inodes.len() {
            return Err(VfsError::NotFound);
        }
        let inode = &mut self.inodes[slot];
        if !inode.in_use || inode.generation != inode_generation(id) {
            return Err(VfsError::NotFound);
        }
        Ok(inode)
    }

    /// Is `maybe_ancestor` at or above `start` in the directory tree?
    ///
    /// Walks parent links, bounded by the table size so a pre-existing cycle
    /// terminates the walk instead of hanging it.
    fn is_ancestor_of(&self, maybe_ancestor: InodeId, start: InodeId) -> VfsResult<bool> {
        let root = self.root_id();
        let mut current = start;
        for _ in 0..self.inodes.len() {
            if current == maybe_ancestor {
                return Ok(true);
            }
            if current == root {
                return Ok(false);
            }
            current = self.get_inode(current)?.parent;
        }
        Ok(true)
    }

    /// The root's id carries its generation like any other.
    fn root_id(&self) -> InodeId {
        let generation = self
            .inodes
            .get(ROOT_SLOT)
            .map(|i| i.generation)
            .unwrap_or(1);
        pack_inode_id(ROOT_SLOT as u64, generation)
    }
}

pub struct RamFs {
    inner: SpinLock<RamFsInner>,
}

impl RamFs {
    /// Inode storage is allocated lazily on first access. The lock class comes
    /// from the caller: two instances sharing one would make a path walk that
    /// crosses a mount point look like an unordered self-nest.
    pub const fn new_const(class: &'static LockClassKey) -> Self {
        Self {
            inner: SpinLock::new(
                RamFsInner {
                    inodes: KVec::new(),
                    initialized: false,
                },
                class,
            ),
        }
    }

    fn with_inner<R>(&self, f: impl FnOnce(&RamFsInner) -> R) -> R {
        let mut inner = self.inner.lock();
        inner.ensure_initialized();
        f(&*inner)
    }

    fn with_inner_mut<R>(&self, f: impl FnOnce(&mut RamFsInner) -> R) -> R {
        let mut inner = self.inner.lock();
        inner.ensure_initialized();
        f(&mut *inner)
    }

    /// Drop every inode, so a pooled instance handed to a later `mount(2)`
    /// cannot serve the previous mount's contents.
    pub fn reset(&self) {
        // The table leaves the lock as one pointer: clearing it under the
        // lock would free every chunk of every file with interrupts off.
        let _stale = {
            let mut inner = self.inner.lock();
            inner.initialized = false;
            core::mem::replace(&mut inner.inodes, KVec::new())
        };
    }

    /// Bytes this instance holds in file chunks, which is what a truncate
    /// down is supposed to give back.
    #[cfg(feature = "tests")]
    pub(crate) fn resident_bytes(&self) -> usize {
        self.with_inner(|inner| inner.inodes.iter().map(|i| i.data.resident_bytes()).sum())
    }

    /// Remove a name, reclaiming the inode's slot now or leaving it for a
    /// later [`FileSystem::release_detached`].
    ///
    /// Answers the inode whose reclaim was deferred. A deferred slot keeps
    /// `in_use` and its generation, so a descriptor holding the id still
    /// resolves — POSIX's unlinked-but-open rule on a filesystem whose inode
    /// is a slot. `nlink` zero is what says the slot is unreachable by name.
    fn remove_name(
        &self,
        parent: InodeId,
        name: &[u8],
        reclaim: Reclaim,
    ) -> VfsResult<Option<InodeId>> {
        // The removed file's body leaves the lock as one pointer and is
        // dropped out here.
        let mut body: KVec<FileChunk> = KVec::new();
        self.with_inner_mut(|inner| {
            let target_id = {
                let parent_inode = inner.get_inode(parent)?;
                if parent_inode.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if parent_inode.sealed {
                    return Err(VfsError::PermissionDenied);
                }
                parent_inode.lookup(name)?
            };

            let is_dir = {
                let target = inner.get_inode(target_id)?;
                if target.file_type == FileType::Directory && target.dir_entry_count() > 2 {
                    return Err(VfsError::NotEmpty);
                }
                target.file_type == FileType::Directory
            };

            inner.get_inode_mut(parent)?.remove_dir_entry(name)?;

            if is_dir {
                inner.get_inode_mut(parent)?.nlink -= 1;
            }

            // A directory is never deferred: `open` refuses one, so nothing
            // can be holding it.
            if is_dir || reclaim == Reclaim::Now {
                body = inner.inodes[inode_slot(target_id)].reset();
                return Ok(None);
            }
            inner.get_inode_mut(target_id)?.nlink = 0;
            Ok(Some(target_id))
        })
    }

    /// What a mutation of a regular file's body is refused for. Every hold of
    /// a multi-hold grow repeats this: between two of them the inode may have
    /// been sealed, retyped or recycled.
    fn body_writable(inode: &RamInode) -> VfsResult<()> {
        if inode.sealed {
            return Err(VfsError::PermissionDenied);
        }
        if inode.file_type == FileType::Directory {
            return Err(VfsError::IsDirectory);
        }
        Ok(())
    }

    /// The file's own refusals, answering the size a refused grow puts back.
    fn check_body(&self, inode: InodeId) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;
            Self::body_writable(ram_inode)?;
            Ok(ram_inode.data_len())
        })
    }

    /// Grow `inode` to `new_len`, taking `src` as its last `src.len()` bytes —
    /// which is what a write past the end is, and an empty `src` a truncate up.
    ///
    /// One pass reads the shortfall under the lock, drops it to allocate at
    /// most `RAMFS_CHUNK_BATCH` chunks, then retakes it to install the ones
    /// the file still wants — a concurrent truncate or unlink may have changed
    /// that. A chunk that cannot be had puts the file back to `entry_len`.
    fn grow_body(
        &self,
        inode: InodeId,
        entry_len: usize,
        new_len: usize,
        src: &[u8],
    ) -> VfsResult<usize> {
        let offset = new_len - src.len();
        let mut staged: [Option<FileChunk>; RAMFS_CHUNK_BATCH] =
            [const { None }; RAMFS_CHUNK_BATCH];
        let mut staged_count = 0usize;
        let mut copied = 0usize;
        loop {
            let pass = self.with_inner_mut(|inner| {
                let ram_inode = inner.get_inode_mut(inode)?;
                Self::body_writable(ram_inode)?;
                let data = &mut ram_inode.data;
                let installed = data.install(&mut staged[..staged_count], new_len)?;
                copied += data.write_within(offset + copied, &src[copied..]);
                Ok((installed, data.short_of(new_len)))
            });
            let (installed, short) = match pass {
                Ok(pass) => pass,
                // A seal or an unlink that landed mid-grow is not this call's
                // to undo; the refusal it caused itself is.
                Err(VfsError::NoSpace) => {
                    self.shrink_body(inode, entry_len);
                    return Err(VfsError::NoSpace);
                }
                Err(e) => return Err(e),
            };
            // What one hold moved, for the test to assert against the batch.
            // What the file did not want stays staged and is dropped outside.
            probe::note_hold(installed);
            if short == 0 {
                return Ok(copied);
            }
            staged_count = stage_chunks(&mut staged[..short.min(RAMFS_CHUNK_BATCH)]);
            if staged_count == 0 {
                self.shrink_body(inode, entry_len);
                return Err(VfsError::NoSpace);
            }
        }
    }

    /// Shrink `inode` to `new_len`, giving its chunks back in bounded steps:
    /// `len` and the chunks descend together, so the size is what the chunks
    /// cover every time the lock is dropped.
    fn shrink_body(&self, inode: InodeId, new_len: usize) {
        loop {
            let mut parked: [Option<FileChunk>; RAMFS_CHUNK_BATCH] =
                [const { None }; RAMFS_CHUNK_BATCH];
            let (moved, more, _index) = self.with_inner_mut(|inner| {
                let Ok(ram_inode) = inner.get_inode_mut(inode) else {
                    return (0, false, KVec::new());
                };
                let data = &mut ram_inode.data;
                if data.len() <= new_len {
                    return (0, false, KVec::new());
                }
                let floor = new_len.div_ceil(RAMFS_FILE_CHUNK);
                let step = data.coverage().saturating_sub(RAMFS_CHUNK_BATCH).max(floor);
                let step_len = if step == floor {
                    new_len
                } else {
                    step * RAMFS_FILE_CHUNK
                };
                data.step_down(step_len);
                let moved = data.park_released(&mut parked);
                // An empty file gives its index back too, and that free is
                // the caller's like every other one here.
                let index = if data.len() == 0 {
                    data.take_body()
                } else {
                    KVec::new()
                };
                (moved, step_len > new_len, index)
            });
            probe::note_hold(moved);
            if !more {
                return;
            }
        }
    }
}

/// When [`RamFs::remove_name`] gives an inode's slot back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reclaim {
    Now,
    Deferred,
}

impl FileSystem for RamFs {
    fn name(&self) -> &'static str {
        "ramfs"
    }

    fn root_inode(&self) -> InodeId {
        self.with_inner(|inner| inner.root_id())
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        self.with_inner(|inner| {
            let parent_inode = inner.get_inode(parent)?;

            if parent_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }

            parent_inode.lookup(name)
        })
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            Ok(FileStat {
                inode,
                file_type: ram_inode.file_type,
                size: ram_inode.data_len() as u64,
                mode: ram_inode.mode,
                nlink: ram_inode.nlink,
                uid: 0,
                gid: 0,
                atime: ram_inode.atime,
                mtime: ram_inode.mtime,
                ctime: ram_inode.ctime,
                dev_major: 0,
                dev_minor: 0,
                sealed: ram_inode.sealed,
            })
        })
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            if ram_inode.file_type == FileType::Directory {
                return Err(VfsError::IsDirectory);
            }

            let Ok(offset) = usize::try_from(offset) else {
                return Ok(0);
            };
            Ok(ram_inode.data.read(offset, buf))
        })
    }

    fn write(&self, inode: InodeId, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let max_size = ramfs_max_file_size();
        // The file's own refusals come before the cap's.
        let entry_len = self.check_body(inode)?;

        let offset = usize::try_from(offset).map_err(|_| VfsError::NoSpace)?;
        let Some(end) = offset.checked_add(buf.len()) else {
            return Err(VfsError::NoSpace);
        };
        if end > max_size {
            return Err(VfsError::NoSpace);
        }

        let written = self.grow_body(inode, entry_len, end, buf)?;
        self.with_inner_mut(|inner| {
            inner.get_inode_mut(inode)?.touch_modified();
            Ok(written)
        })
    }

    fn create(&self, parent: InodeId, name: &[u8], file_type: FileType) -> VfsResult<InodeId> {
        let max_inodes = ramfs_max_inodes();
        self.with_inner_mut(|inner| {
            {
                let parent_inode = inner.get_inode(parent)?;
                if parent_inode.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if parent_inode.sealed {
                    return Err(VfsError::PermissionDenied);
                }
                if parent_inode.lookup(name).is_ok() {
                    return Err(VfsError::AlreadyExists);
                }
            }

            let new_id = inner.alloc_inode(max_inodes)?;

            {
                let new_inode = &mut inner.inodes[inode_slot(new_id)];
                new_inode.in_use = true;
                new_inode.file_type = file_type;
                new_inode.dir_entries.clear();
                new_inode.parent = parent;

                match file_type {
                    FileType::Directory => {
                        new_inode.mode = 0o755;
                        new_inode.nlink = 2;
                        new_inode.add_dir_entry(b".", new_id)?;
                        new_inode.add_dir_entry(b"..", parent)?;
                    }
                    _ => {
                        new_inode.mode = 0o644;
                        new_inode.nlink = 1;
                    }
                }
                stamp(&mut new_inode.atime);
                stamp(&mut new_inode.mtime);
                stamp(&mut new_inode.ctime);
            }

            let parent_inode = inner.get_inode_mut(parent)?;
            parent_inode.add_dir_entry(name, new_id)?;
            parent_inode.touch_modified();

            if file_type == FileType::Directory {
                inner.get_inode_mut(parent)?.nlink += 1;
            }

            Ok(new_id)
        })
    }

    fn unlink(&self, parent: InodeId, name: &[u8]) -> VfsResult<()> {
        self.remove_name(parent, name, Reclaim::Now).map(|_| ())
    }

    fn detach(&self, parent: InodeId, name: &[u8]) -> VfsResult<Option<InodeId>> {
        self.remove_name(parent, name, Reclaim::Deferred)
    }

    /// The deferred half of [`Self::detach`]: the slot is reset now that
    /// nothing holds it. Idempotent, and refuses a slot that has been reused
    /// — the generation in the id is what makes that check possible.
    fn release_detached(&self, inode: InodeId) -> VfsResult<()> {
        // As in `remove_name`: the body is dropped out here.
        let _body = self.with_inner_mut(|inner| {
            let slot = inode_slot(inode);
            if slot >= inner.inodes.len() || slot == ROOT_SLOT {
                return KVec::new();
            }
            let target = &mut inner.inodes[slot];
            // A live nlink means a name came back, and a moved generation
            // means the slot is somebody else's now. Either way this is not
            // the inode the deferral was for.
            if !target.in_use || target.generation != inode_generation(inode) || target.nlink > 0 {
                return KVec::new();
            }
            target.reset()
        });
        Ok(())
    }

    /// A slot reset hands the file's body out and the frees happen with no
    /// lock held, so the last close reclaims inline. Deferring it would need a
    /// writeback thread ramfs does not have.
    fn release_detached_blocks(&self) -> bool {
        false
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;

            if ram_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }

            let mut count = 0;
            for i in offset..ram_inode.dir_entries.len() {
                let entry = &ram_inode.dir_entries[i];
                let entry_inode = match inner.get_inode(entry.inode) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let name = entry.name.as_slice();
                if !callback(name, entry.inode, entry_inode.file_type) {
                    break;
                }
                count += 1;
            }

            Ok(count)
        })
    }

    /// Overridden because [`Self::readdir`] skips an entry whose inode fails
    /// to resolve without invoking the callback: the trait's default counts
    /// callbacks, so its cookie would lag the index it feeds back and the next
    /// page would repeat a name. Here the cookie *is* the index.
    fn readdir_cookie(
        &self,
        inode: InodeId,
        cookie: u64,
        callback: &mut dyn FnMut(u64, &[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<u64> {
        let start = usize::try_from(cookie).map_err(|_| VfsError::InvalidArgument)?;
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;
            if ram_inode.file_type != FileType::Directory {
                return Err(VfsError::NotDirectory);
            }
            let mut index = start;
            while index < ram_inode.dir_entries.len() {
                let entry = &ram_inode.dir_entries[index];
                index += 1;
                let Ok(entry_inode) = inner.get_inode(entry.inode) else {
                    continue;
                };
                let name = entry.name.as_slice();
                if !callback(index as u64, name, entry.inode, entry_inode.file_type) {
                    break;
                }
            }
            Ok(index as u64)
        })
    }

    fn truncate(&self, inode: InodeId, size: u64) -> VfsResult<()> {
        let max_size = ramfs_max_file_size();
        let entry_len = self.check_body(inode)?;
        let new_size = usize::try_from(size).unwrap_or(usize::MAX).min(max_size);

        if new_size > entry_len {
            self.grow_body(inode, entry_len, new_size, &[])?;
        } else {
            self.shrink_body(inode, new_size);
        }
        self.with_inner_mut(|inner| {
            inner.get_inode_mut(inode)?.touch_modified();
            Ok(())
        })
    }

    fn rename(
        &self,
        old_parent: InodeId,
        old_name: &[u8],
        new_parent: InodeId,
        new_name: &[u8],
    ) -> VfsResult<()> {
        // The overwritten file's body leaves the lock as one pointer and is
        // dropped out here.
        let mut displaced_body: KVec<FileChunk> = KVec::new();
        self.with_inner_mut(|inner| {
            if old_parent == new_parent && old_name == new_name {
                return Ok(());
            }

            let target_inode = {
                let old_parent_node = inner.get_inode(old_parent)?;
                if old_parent_node.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if old_parent_node.sealed {
                    return Err(VfsError::PermissionDenied);
                }
                old_parent_node.lookup(old_name)?
            };

            let source_type = inner.get_inode(target_inode)?.file_type;

            // Splicing a directory into its own descendant detaches the whole
            // subtree: it becomes unreachable from the root and unremovable.
            if source_type == FileType::Directory
                && inner.is_ancestor_of(target_inode, new_parent)?
            {
                return Err(VfsError::InvalidPath);
            }

            let displaced = {
                let new_parent_node = inner.get_inode(new_parent)?;
                if new_parent_node.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if new_parent_node.sealed {
                    return Err(VfsError::PermissionDenied);
                }
                match new_parent_node.lookup(new_name) {
                    Ok(existing) => Some(existing),
                    Err(_) => None,
                }
            };

            if let Some(existing) = displaced {
                let existing_type = inner.get_inode(existing)?.file_type;
                match (source_type, existing_type) {
                    (FileType::Directory, FileType::Directory) => {
                        if inner.get_inode(existing)?.dir_entry_count() > 2 {
                            return Err(VfsError::NotEmpty);
                        }
                    }
                    (FileType::Directory, _) => return Err(VfsError::NotDirectory),
                    (_, FileType::Directory) => return Err(VfsError::IsDirectory),
                    _ => {}
                }
                inner
                    .get_inode_mut(new_parent)?
                    .remove_dir_entry(new_name)?;
                // Unlinking the entry alone leaks the inode out of the fixed
                // table, which repeated overwrites then exhaust.
                displaced_body = inner.inodes[inode_slot(existing)].reset();
            }

            inner
                .get_inode_mut(old_parent)?
                .remove_dir_entry(old_name)?;
            inner
                .get_inode_mut(new_parent)?
                .add_dir_entry(new_name, target_inode)?;

            let is_dir = inner.get_inode(target_inode)?.file_type == FileType::Directory;
            if is_dir {
                let target_node = inner.get_inode_mut(target_inode)?;
                for i in 0..target_node.dir_entries.len() {
                    if target_node.dir_entries[i].matches(b"..") {
                        target_node.dir_entries[i].inode = new_parent;
                        break;
                    }
                }
                target_node.parent = new_parent;

                if old_parent != new_parent {
                    let old_nlink = inner.get_inode(old_parent)?.nlink;
                    inner.get_inode_mut(old_parent)?.nlink = old_nlink.saturating_sub(1);

                    let new_nlink = inner.get_inode(new_parent)?.nlink;
                    inner.get_inode_mut(new_parent)?.nlink = new_nlink.saturating_add(1);
                }
            }

            Ok(())
        })
    }

    fn set_mode(&self, inode: InodeId, mode: u16) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;
            if ram_inode.sealed {
                return Err(VfsError::PermissionDenied);
            }
            ram_inode.mode = mode & 0o7777;
            Ok(())
        })
    }

    fn readlink(&self, inode: InodeId, buf: &mut [u8]) -> VfsResult<usize> {
        self.with_inner(|inner| {
            let ram_inode = inner.get_inode(inode)?;
            if ram_inode.file_type != FileType::Symlink {
                return Err(VfsError::InvalidArgument);
            }
            Ok(ram_inode.data.read(0, buf))
        })
    }

    /// The target is the inode's data, which is what makes `stat().size` its
    /// length — the resolver sizes its read buffer from that.
    fn symlink(&self, parent: InodeId, name: &[u8], target: &[u8]) -> VfsResult<InodeId> {
        if target.is_empty() || target.len() > crate::MAX_PATH_LEN {
            return Err(VfsError::InvalidArgument);
        }
        let max_inodes = ramfs_max_inodes();
        // The target's chunks are staged here, like any other body's: what
        // the lock covers is installing them, not allocating them.
        let needed = target.len().div_ceil(RAMFS_FILE_CHUNK);
        let mut staged: [Option<FileChunk>; SYMLINK_CHUNKS] = [const { None }; SYMLINK_CHUNKS];
        if stage_chunks(&mut staged[..needed]) != needed {
            return Err(VfsError::NoSpace);
        }
        let mut orphan: KVec<FileChunk> = KVec::new();
        self.with_inner_mut(|inner| {
            {
                let parent_inode = inner.get_inode(parent)?;
                if parent_inode.file_type != FileType::Directory {
                    return Err(VfsError::NotDirectory);
                }
                if parent_inode.sealed {
                    return Err(VfsError::PermissionDenied);
                }
                if parent_inode.lookup(name).is_ok() {
                    return Err(VfsError::AlreadyExists);
                }
            }

            let new_id = inner.alloc_inode(max_inodes)?;
            {
                let new_inode = &mut inner.inodes[inode_slot(new_id)];
                new_inode.in_use = true;
                new_inode.file_type = FileType::Symlink;
                new_inode.dir_entries.clear();
                new_inode.parent = parent;
                new_inode.mode = 0o777;
                new_inode.nlink = 1;
                if new_inode
                    .data
                    .install(&mut staged[..needed], target.len())
                    .is_err()
                {
                    orphan = new_inode.reset();
                    return Err(VfsError::NoSpace);
                }
                new_inode.data.write_within(0, target);
                stamp(&mut new_inode.atime);
                stamp(&mut new_inode.mtime);
                stamp(&mut new_inode.ctime);
            }

            if let Err(e) = inner.get_inode_mut(parent)?.add_dir_entry(name, new_id) {
                orphan = inner.inodes[inode_slot(new_id)].reset();
                return Err(e);
            }
            inner.get_inode_mut(parent)?.touch_modified();
            Ok(new_id)
        })
    }

    fn set_times(&self, inode: InodeId, atime: Option<u64>, mtime: Option<u64>) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            let ram_inode = inner.get_inode_mut(inode)?;
            if ram_inode.sealed {
                return Err(VfsError::PermissionDenied);
            }
            if let Some(atime) = atime {
                ram_inode.atime = atime;
            }
            if let Some(mtime) = mtime {
                ram_inode.mtime = mtime;
            }
            stamp(&mut ram_inode.ctime);
            Ok(())
        })
    }

    fn set_sealed(&self, inode: InodeId) -> VfsResult<()> {
        self.with_inner_mut(|inner| {
            inner.get_inode_mut(inode)?.sealed = true;
            Ok(())
        })
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }

    /// Inode totals are real; block counts are zero.
    ///
    /// A heap-backed filesystem with no size limit has no capacity of its own
    /// to report — Linux's ramfs reports zeros too. `block_size` stays the
    /// page size so a byte count computed from it is zero, not a division by
    /// zero.
    fn statfs(&self) -> VfsResult<FsStats> {
        let used = self.with_inner(|inner| inner.inodes.iter().filter(|i| i.in_use).count() as u64);
        // Slot 0 is a sentinel, so it is not one of the inodes on offer.
        let total = (ramfs_max_inodes() - ROOT_SLOT) as u64;
        Ok(FsStats {
            magic: slopos_abi::fs::RAMFS_MAGIC,
            block_size: PAGE_SIZE as u32,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            inodes: total,
            inodes_free: total.saturating_sub(used),
            max_name_len: MAX_NAME_LEN as u32,
            read_only: false,
        })
    }
}
