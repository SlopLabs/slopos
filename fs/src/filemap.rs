//! Per-inode page sets for file-backed `mmap` (G14).
//!
//! **A per-inode page set is the authority for the pages it holds populated
//! while a shared mapping is live.** Nothing here maps an ext2 `BlockCache`
//! frame, so the block cache stays the single device-facing cache, at the cost
//! of one 4 KiB copy per mapped page. `read(2)` and `write(2)` are routed
//! through the set for the ranges it covers ([`read_through`],
//! [`write_through`]), and writeback goes out through [`FileSystem::write`].
//!
//! A mapping reserves an *extent* ([`reserve_range`]) and populates nothing;
//! frames arrive from [`fault_page_in_set`], which runs on the faulting task's
//! kernel stack and may block on the filesystem. A slot holding
//! [`PhysAddr::NULL`] is a page nobody has faulted yet, for which the
//! filesystem is still the authority. Mapping a file therefore costs frames
//! only for the pages a process touches — and, for a set no mapping can
//! write, the [`READAHEAD_PAGES`] after each one it reads, in the same request.
//! The fault maps whatever of its aligned window the set already holds
//! ([`resident_in_set`]), and a `MAP_PRIVATE` mapping maps the set's frame
//! copy-on-write rather than copying it, so the only private pages are the
//! ones a process stored to.
//!
//! A set whose last mapping goes is *parked* rather than freed: a program
//! started again maps the same shared objects, and reading them back is most
//! of what starting it costs. A parked set is charged to nobody and gives its
//! frames back oldest first when the page ceiling or the reclaimer asks
//! ([`evict_idle`]).
//!
//! EOF is discovered per page, at fault time: a page starting at or past the
//! end is refused with [`FileMapError::PastEof`], and one straddling it is read
//! short and zero-filled, so a file whose size is not page-aligned maps.
//!
//! Writeback writes every *populated* page of a set a shared mapping reached: a
//! user store sets the PTE dirty bit and nothing here harvests it. A set that
//! only served `MAP_PRIVATE` copies holds what the filesystem holds, so its
//! writeback is skipped.
//!
//! When an inode's name is removed the VFS flushes its set and unkeys it
//! ([`detach_inode`]) *before* the removal: ext2 inode numbers carry no
//! generation, so unkeying at the one moment the number can be reused is what
//! stops a reallocated inode resolving to the previous file's pages. A live
//! mapping keeps the frames it has, its stores stop being written back, and a
//! fault on a page it never populated is refused.
//!
//! Lock order: [`FILEMAP_IO`] (sleeping, held across the filesystem calls) →
//! [`FILEMAP`] (spinning; a bounded scan, one index rebuild or one page copy) →
//! `CACHED_EXT2`. Exactly one page is populated per [`FILEMAP`] acquisition and
//! the read that fills it runs with [`FILEMAP`] dropped. [`release`] is reached
//! from process teardown under a preempt guard, so it queues and
//! [`drain_pending`] completes the work.
//!
//! The inode cap bounds the registry; the page ceiling is derived from usable
//! physical memory, because a page under a live user PTE is unreclaimable by
//! construction. A principal's share of each stops one process cornering the
//! budget. One slot holds one charge, so a set has one owner: whoever most
//! recently reserved or mapped it.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use slopos_abi::Errno;
use slopos_abi::addr::PhysAddr;
use slopos_abi::quota::PinnedBytesAxis;
use slopos_mm::filemap_hook::FileMapOps;
use slopos_mm::hhdm::PhysAddrHhdm;
use slopos_mm::page_alloc::{alloc_kernel_page, get_page_allocator_stats};
use slopos_mm::vma_region::FileMapRef;
use slopos_ostd::mm::frame::{claim_owned_anon_page, release_owned_anon_page};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{ChargeSlot, try_charge};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, Mutex, SpinLock};
use slopos_ostd::{KVec, klog_info, lock_class};

use crate::vfs::traits::same_filesystem;
use crate::vfs::{FileSystem, InodeId};

const PAGE_SIZE: u64 = 4096;
const PAGE_SIZE_USIZE: usize = 4096;

/// Inodes that may hold a page set at once. `rustc` maps every codegen unit's
/// object to archive a crate, 256 of them for an incremental one, and a `-j4`
/// build can be archiving four crates while the rest map rlibs.
pub(crate) const MAX_MAPPED_INODES: usize = 4096;

/// The registry is `MAX_MAPPED_INODES` of these, so each byte of one is 4 KiB
/// of BSS.
const _: () = assert!(core::mem::size_of::<PageSet>() <= 104);

/// Populated pages across every set, kept beside the sets so a fault's
/// admission check is one load rather than a walk of the registry.
static POPULATED_PAGES: AtomicU32 = AtomicU32::new(0);

/// The fraction of usable physical memory the registry may pin.
const MAPPED_PAGE_SHARE: u32 = 4;

/// The fraction of the registry's ceiling one principal may hold: a linker
/// maps every rlib of the kernel and its own output at once.
const PRINCIPAL_PAGE_SHARE: u32 = 2;

/// Floor for a machine whose usable memory is not known yet — the 4 MiB this
/// registry was fixed at before the ceiling was derived.
const MIN_MAPPED_PAGES: u32 = 1024;

/// Pages one set's index may describe. The index is a single `KVec<PhysAddr>`
/// against a 1 MiB `MAX_ALLOC_SIZE`, so this holds the allocation at 512 KiB
/// and still describes 256 MiB of file.
const MAX_SET_PAGES: u32 = 65536;

/// Slots one principal may hold. Kernel work (`AccountId::NONE`) is outside
/// the share, as it is outside ext2's block reserve: it is not a principal.
pub(crate) const MAX_INODES_PER_ACCOUNT: usize = MAX_MAPPED_INODES / 4;

/// Derived once from the page allocator, then cached. Zero is "not derived
/// yet": an unseeded allocator gets the floor and is asked again next time.
static PAGE_CEILING: AtomicU32 = AtomicU32::new(0);

/// Populated pages the whole registry may hold at once.
fn max_mapped_pages() -> u32 {
    let cached = PAGE_CEILING.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let stats = get_page_allocator_stats();
    // The seeded frames, not the highest index: reserved holes and the kernel
    // image never enter the buddy, and this sum survives allocation.
    let usable = stats.free.saturating_add(stats.allocated);
    if usable == 0 {
        return MIN_MAPPED_PAGES;
    }
    let derived = derive_page_ceiling(usable);
    PAGE_CEILING.store(derived, Ordering::Relaxed);
    derived
}

pub(crate) fn derive_page_ceiling(usable_frames: u32) -> u32 {
    (usable_frames / MAPPED_PAGE_SHARE).max(MIN_MAPPED_PAGES)
}

/// Populated pages one principal may hold.
fn max_pages_per_account() -> u32 {
    max_mapped_pages() / PRINCIPAL_PAGE_SHARE
}

/// Make the per-principal share the `PinnedBytes` default of every account
/// row, so the ledger and the walk bound one number; the `abi` default was
/// sized for an appliance, and a compiler's shared objects exceed it.
pub fn install_pinned_default() -> u32 {
    let pages = max_pages_per_account().max(slopos_abi::quota::default_process_limit(
        slopos_abi::quota::ResourceKind::PinnedBytes,
    ));
    slopos_ostd::process::quota::set_derived_process_limit(
        slopos_abi::quota::ResourceKind::PinnedBytes,
        pages,
    );
    pages
}

/// Override the derived ceiling, so a test can reach a refusal without pinning
/// a quarter of the machine.
#[cfg(feature = "tests")]
pub(crate) fn set_page_ceiling_for_test(pages: u32) -> u32 {
    PAGE_CEILING.swap(pages, Ordering::Relaxed)
}

/// Why a page set could not be handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMapError {
    /// Every page-set slot is in use.
    TooManyInodes,
    /// The registry's page ceiling.
    TooManyPages,
    NoMemory,
    /// The request covers no page.
    EmptyRange,
    /// The page starts at or past the end of the file.
    PastEof,
    /// The handle names a recycled slot, or a set whose inode was unlinked.
    Stale,
    /// The inode refuses to be written: a writable page set on it would
    /// publish bytes the filesystem will never accept.
    WriteRefused,
    Io,
    Interrupted,
}

impl FileMapError {
    pub fn to_errno(self) -> Errno {
        match self {
            Self::TooManyInodes | Self::TooManyPages | Self::NoMemory => Errno::ENOMEM,
            Self::EmptyRange | Self::PastEof | Self::Stale => Errno::EINVAL,
            Self::WriteRefused => Errno::EACCES,
            Self::Io => Errno::EIO,
            Self::Interrupted => Errno::EINTR,
        }
    }
}

/// One inode's pages. A free slot has `fs == None`.
struct PageSet {
    fs: Option<&'static dyn FileSystem>,
    inode: InodeId,
    /// File-relative index of `pages[0]`.
    first_page: u64,
    /// The reserved extent, one entry per page. [`PhysAddr::NULL`] is a page
    /// the extent describes that nobody has faulted yet.
    pages: KVec<PhysAddr>,
    /// Non-null entries in `pages` — what the charge and both ceilings count.
    /// Not the reference count: a reserved page costs a reference, no frame.
    populated: u32,
    /// Mapping references, counted in pages, plus the one unit an in-flight
    /// fault holds until its caller has installed the PTE or given up.
    refs: u32,
    generation: u32,
    /// A shared mapping or a `write(2)` reached this set, so its pages may
    /// differ from the filesystem's.
    dirtyable: bool,
    /// The last reference went; writeback and the frame frees are owed.
    pending_release: bool,
    /// `msync(MS_ASYNC)`: writeback is owed, the set stays.
    pending_flush: bool,
    /// The inode's last name is going away: the set stops being findable by
    /// `(filesystem, inode)` and stops being written back, while every live
    /// mapping keeps reading the pages it already has.
    forgotten: bool,
    /// The principal the frames are charged to. One charge per slot, so a
    /// second principal takes the whole set rather than a share of it.
    owner: AccountId,
    /// The frames' charge, equal to `populated` while the set is live. A frame
    /// under a user PTE is pinned against reclaim, which is what the
    /// `PinnedBytes` axis counts.
    charge: ChargeSlot<PinnedBytesAxis>,
    /// Non-zero once nothing maps the set any more and its pages are clean:
    /// they are then a cache of the file, charged to nobody, served to
    /// `read(2)` and to the next mapping of the inode, and the first thing
    /// dropped when a slot, the page ceiling or the machine runs short. The
    /// value orders idle sets: the lowest went idle first and goes first.
    idle_since: u32,
}

impl PageSet {
    const EMPTY: Self = Self {
        fs: None,
        inode: 0,
        first_page: 0,
        pages: KVec::new(),
        populated: 0,
        refs: 0,
        generation: 0,
        dirtyable: false,
        pending_release: false,
        pending_flush: false,
        forgotten: false,
        owner: AccountId::NONE,
        charge: ChargeSlot::empty(),
        idle_since: 0,
    };

    /// Deliberately `false` once forgotten: the inode number may already have
    /// been reallocated to a different file.
    fn holds(&self, fs: &'static dyn FileSystem, inode: InodeId) -> bool {
        match self.fs {
            Some(mine) => !self.forgotten && self.inode == inode && same_filesystem(mine, fs),
            None => false,
        }
    }

    /// Index of `page` in the extent, whether or not it is populated.
    fn index_of(&self, page: u64) -> Option<usize> {
        if page < self.first_page {
            return None;
        }
        let idx = usize::try_from(page - self.first_page).ok()?;
        (idx < self.pages.len()).then_some(idx)
    }

    fn frame_at(&self, page: u64) -> Option<PhysAddr> {
        let pa = *self.pages.get(self.index_of(page)?)?;
        (!pa.is_null()).then_some(pa)
    }

    fn is_idle(&self) -> bool {
        self.idle_since != 0
    }

    fn extent_end(&self) -> u64 {
        self.first_page + self.pages.len() as u64
    }
}

/// Sleeping, because population and writeback reach the filesystem. Holding it
/// orders a fault's read against a `write(2)` and against an extent rebuild.
static FILEMAP_IO: Mutex<()> = Mutex::new((), lock_class!("FILEMAP_IO", LOCK_LEVEL_RESOURCE));

/// Spinning, and must stay so: [`release`] runs from a `Drop` the task-exit
/// path reaches under a preempt guard. No filesystem call is made under it.
static FILEMAP: SpinLock<[PageSet; MAX_MAPPED_INODES]> = SpinLock::new(
    [const { PageSet::EMPTY }; MAX_MAPPED_INODES],
    lock_class!("FILEMAP", LOCK_LEVEL_RESOURCE),
);

/// Sets owing writeback, so a flusher's wait predicate takes no lock.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Populated pages held by idle sets: what the reclaimer can give back.
static IDLE_PAGES: AtomicU32 = AtomicU32::new(0);
static IDLE_SEQ: AtomicU32 = AtomicU32::new(0);

/// Pages a fault reads ahead of the one it needs, in one filesystem read,
/// when they sit in the same read-only set and nobody has populated them: a
/// shared library's text faults in page order, and each page read alone is a
/// device round trip.
const READAHEAD_PAGES: u64 = 16;

fn io_lock() -> Result<slopos_ostd::sync::MutexGuard<'static, ()>, FileMapError> {
    FILEMAP_IO.lock().map_err(|_| FileMapError::Interrupted)
}

fn ref_for(slot: usize, generation: u32) -> FileMapRef {
    FileMapRef {
        slot: slot as u16,
        generation,
    }
}

fn resolve(sets: &mut [PageSet], map: FileMapRef) -> Option<&mut PageSet> {
    let entry = sets.get_mut(map.slot as usize)?;
    if entry.fs.is_none() || entry.generation != map.generation {
        return None;
    }
    Some(entry)
}

/// Reserve `[first_page, first_page + page_count)` of `inode` and answer a
/// handle to the set that will hold it.
///
/// Nothing is read and no frame is claimed; the pages arrive from
/// [`fault_page_in_set`]. The file's size is deliberately not consulted: a
/// mapping may reach past the end, and each page discovers that when faulted.
///
/// A writable set on a sealed inode is refused here as well as by `open(2)`:
/// the set is what `read(2)` is routed through, so bytes stored into it are
/// published to every reader of the file.
///
/// The returned [`FileMapRef`] carries `page_count` reference units, given back
/// with `release(map, pages)` at teardown — or at once, if no mapping lands.
///
/// `owner` is the principal the frames are charged to and whose share the
/// request is measured against: the mapping process, not the calling task.
pub fn reserve_range(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    page_count: u32,
    writable: bool,
    owner: AccountId,
) -> Result<FileMapRef, FileMapError> {
    if page_count == 0 {
        return Err(FileMapError::EmptyRange);
    }
    if page_count > MAX_SET_PAGES {
        return Err(FileMapError::TooManyPages);
    }
    // A set queued by a process that exited has nobody else to complete it on
    // a boot that runs no ext2 flusher, and the slot it holds would refuse
    // every later mapping.
    drain_pending();
    if writable && fs.stat(inode).map_err(|_| FileMapError::Io)?.sealed {
        return Err(FileMapError::WriteRefused);
    }
    let _io = io_lock()?;
    reserve_slot(fs, inode, first_page, page_count, owner)
}

/// The bookkeeping half of [`reserve_range`]: claim the slot, widen the extent
/// and take the mapping's references.
#[inline(never)]
fn reserve_slot(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    page_count: u32,
    owner: AccountId,
) -> Result<FileMapRef, FileMapError> {
    let mut sets = FILEMAP.lock();

    let mut owned_sets = 0usize;
    let mut owned_pages = 0u32;
    let mut existing = None;
    let mut free = None;
    for (idx, entry) in sets.iter().enumerate() {
        if entry.fs.is_some() && !owner.is_none() && entry.owner == owner {
            owned_sets += 1;
            owned_pages = owned_pages.saturating_add(entry.populated);
        }
        if entry.holds(fs, inode) {
            existing = Some(idx);
        } else if entry.fs.is_none() && free.is_none() {
            free = Some(idx);
        }
    }

    let (slot, fresh) = match existing {
        Some(idx) => (idx, false),
        None => match free {
            Some(idx) => (idx, true),
            // Every slot is in use: an idle set's cache is worth less than a
            // mapping.
            None => match oldest_idle(sets.as_slice(), None) {
                Some(idx) => {
                    drop_set(&mut sets[idx]);
                    (idx, true)
                }
                None => return Err(FileMapError::TooManyInodes),
            },
        },
    };

    let (union_first, union_count) = if fresh {
        (first_page, page_count)
    } else {
        let entry = &sets[slot];
        let end = entry.extent_end().max(first_page + page_count as u64);
        let start = entry.first_page.min(first_page);
        let count = u32::try_from(end - start).map_err(|_| FileMapError::TooManyPages)?;
        (start, count)
    };
    if union_count > MAX_SET_PAGES {
        return Err(FileMapError::TooManyPages);
    }

    // A set the caller does not own is re-homed whole, so it costs a slot and
    // every frame it already holds. A fresh slot is the same case: its owner is
    // nobody, and it holds nothing.
    let rehome = sets[slot].owner != owner;
    if !owner.is_none() && rehome && owned_sets >= MAX_INODES_PER_ACCOUNT {
        return Err(FileMapError::TooManyInodes);
    }
    let over_share = |taking: u32| {
        !owner.is_none() && owned_pages.saturating_add(taking) > max_pages_per_account()
    };
    let mut taking = if rehome { sets[slot].populated } else { 0 };
    // An idle set's pages are a cache: when the caller cannot take them on, the
    // mapping starts empty rather than failing. Decided here, done below the
    // last refusable step.
    let mut shed = false;
    let sheddable = sets[slot].is_idle() && sets[slot].refs == 0;
    if sheddable && over_share(taking) {
        shed = true;
        taking = 0;
    }
    if over_share(taking) {
        return Err(FileMapError::TooManyPages);
    }

    let widened =
        if union_first != sets[slot].first_page || union_count as usize != sets[slot].pages.len() {
            Some(widen_index(&sets[slot], union_first, union_count)?)
        } else {
            None
        };

    // Last refusable step: everything after this mutates the slot.
    let reservation = if taking > 0 {
        match try_charge::<PinnedBytesAxis>(owner, taking) {
            Ok(reservation) => Some(reservation),
            Err(_) if sheddable => {
                shed = true;
                None
            }
            Err(_) => return Err(FileMapError::TooManyPages),
        }
    } else {
        None
    };

    let entry = &mut sets[slot];
    if let Some(pages) = widened {
        entry.pages = pages;
        entry.first_page = union_first;
    }
    if shed {
        shed_pages(entry);
    }
    if rehome {
        match reservation {
            Some(reservation) => entry.charge.put(reservation),
            None => entry.charge.take(),
        }
        entry.owner = owner;
    }
    if fresh {
        entry.fs = Some(fs);
        entry.inode = inode;
        entry.generation = entry.generation.wrapping_add(1);
        entry.dirtyable = false;
    }
    take_refs(entry, page_count);
    revive(entry);

    Ok(ref_for(slot, entry.generation))
}

/// The idle set that went idle first, other than `except`.
fn oldest_idle(sets: &[PageSet], except: Option<u32>) -> Option<usize> {
    sets.iter()
        .enumerate()
        .filter(|(idx, e)| {
            e.fs.is_some() && e.is_idle() && e.refs == 0 && Some(*idx as u32) != except
        })
        .min_by_key(|(_, e)| e.idle_since)
        .map(|(idx, _)| idx)
}

/// Drop the oldest idle sets until `want` pages have gone back, answering how
/// many did.
fn evict_idle(sets: &mut [PageSet], want: u32) -> u32 {
    evict_idle_except(sets, want, None)
}

/// [`evict_idle`], sparing the set in slot `except`.
fn evict_idle_except(sets: &mut [PageSet], want: u32, except: Option<u32>) -> u32 {
    let mut freed = 0u32;
    while freed < want {
        let Some(idx) = oldest_idle(sets, except) else {
            break;
        };
        freed = freed.saturating_add(sets[idx].populated);
        drop_set(&mut sets[idx]);
    }
    freed
}

/// Keep an idle set's frames as a cache of the file rather than freeing them.
/// Its charge goes: nothing maps the frames, so nothing pins them.
fn park_set(entry: &mut PageSet) {
    entry.charge.take();
    entry.owner = AccountId::NONE;
    entry.dirtyable = false;
    // Zero means live, so the counter skips it when it wraps.
    entry.idle_since = IDLE_SEQ
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .max(1);
    IDLE_PAGES.fetch_add(entry.populated, Ordering::Relaxed);
}

fn unpark(entry: &mut PageSet) {
    if entry.is_idle() {
        entry.idle_since = 0;
        IDLE_PAGES.fetch_sub(entry.populated, Ordering::Relaxed);
    }
}

/// Take `n` references on a set. A parked set is unparked first: eviction and
/// shedding free an idle set's frames, so a set anyone holds must never read
/// as idle — a fault that found it parked is about to map those frames.
fn take_refs(entry: &mut PageSet, n: u32) {
    unpark(entry);
    entry.refs = entry.refs.saturating_add(n);
}

/// Free every frame a set holds while keeping its extent and identity, for an
/// idle set whose cache nobody can take on.
fn shed_pages(entry: &mut PageSet) {
    unpark(entry);
    for pa in entry.pages.iter_mut() {
        if !pa.is_null() {
            release_owned_anon_page(*pa);
            *pa = PhysAddr::NULL;
        }
    }
    POPULATED_PAGES.fetch_sub(entry.populated, Ordering::Relaxed);
    entry.populated = 0;
    entry.charge.take();
}

/// Rebuild the index over `[union_first, union_first + union_count)`, every
/// held frame at its new position. Fallible before anything is mutated.
fn widen_index(
    entry: &PageSet,
    union_first: u64,
    union_count: u32,
) -> Result<KVec<PhysAddr>, FileMapError> {
    let mut out =
        KVec::filled(PhysAddr::NULL, union_count as usize).map_err(|_| FileMapError::NoMemory)?;
    let shift = usize::try_from(entry.first_page.saturating_sub(union_first))
        .map_err(|_| FileMapError::TooManyPages)?;
    let slots = out.as_mut_slice();
    for (i, pa) in entry.pages.iter().enumerate() {
        let Some(dst) = slots.get_mut(shift + i) else {
            return Err(FileMapError::TooManyPages);
        };
        *dst = *pa;
    }
    Ok(out)
}

/// A set queued for writeback is revived rather than replaced: it still holds
/// the authoritative bytes for its pages.
fn revive(entry: &mut PageSet) {
    if entry.pending_release || entry.pending_flush {
        entry.pending_release = false;
        entry.pending_flush = false;
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Populate `page_index` of the set `map` names, for a fault on a mapping that
/// has already reserved its extent.
///
/// Blocks. Answers the frame with one page reference taken, which the caller
/// gives back with `release(map, 1)`. A page another CPU populated first is
/// answered rather than copied, and the loser's frame goes back.
///
/// [`FileMapError::Stale`] means the handle no longer names a live set, whose
/// blocks may already belong to another file.
pub fn fault_page_in_set(map: FileMapRef, page_index: u64) -> Result<PhysAddr, FileMapError> {
    drain_pending();
    // A populated page needs no read, so it is answered without queueing
    // behind whichever fault or writeback holds the I/O mutex.
    if let FaultProbe::Present(pa) = probe_fault(map, page_index)? {
        return Ok(pa);
    }
    let _io = io_lock()?;
    let (fs, inode, window) = match probe_fault(map, page_index)? {
        FaultProbe::Present(pa) => return Ok(pa),
        FaultProbe::Missing(fs, inode, window) => (fs, inode, window),
    };

    let size = fs.stat(inode).map_err(|_| FileMapError::Io)?.size;
    if page_index.saturating_mul(PAGE_SIZE) >= size {
        return Err(FileMapError::PastEof);
    }
    let wide = window.min(size.div_ceil(PAGE_SIZE) - page_index).max(1);

    // Readahead is best effort: a wide buffer the heap cannot find, or a
    // write to the file racing the read, costs it and nothing else.
    let (mut pages, mut staging) = match KVec::<u8>::zeroed(wide as usize * PAGE_SIZE_USIZE) {
        Ok(buf) => (wide, buf),
        Err(_) => (
            1,
            KVec::<u8>::zeroed(PAGE_SIZE_USIZE).map_err(|_| FileMapError::NoMemory)?,
        ),
    };
    let writes = write_seq(inode).load(Ordering::Acquire);
    read_range_into(fs, inode, page_index, size, staging.as_mut_slice())?;
    if writes & 1 != 0 || write_seq(inode).load(Ordering::Acquire) != writes {
        pages = 1;
    }
    let pa = claim_page()?;
    if let Err(e) = fill_frame(pa, &staging.as_slice()[..PAGE_SIZE_USIZE]) {
        release_owned_anon_page(pa);
        return Err(e);
    }
    let answer = install_page(map, page_index, pa)?;
    // Best effort: a readahead page that finds no frame or no room is simply
    // left for its own fault.
    for k in 1..pages {
        let bytes = &staging.as_slice()[k as usize * PAGE_SIZE_USIZE..][..PAGE_SIZE_USIZE];
        let Ok(extra) = claim_page() else {
            break;
        };
        if fill_frame(extra, bytes).is_err() || !install_readahead(map, page_index + k, extra) {
            break;
        }
    }
    Ok(answer)
}

/// The frames the set holds for `out.len()` pages from `first_page`, null
/// where it holds none. Reads nothing: the caller holds a reference on the
/// set from [`fault_page_in_set`], which keeps every frame answered alive
/// until it releases.
pub fn resident_in_set(map: FileMapRef, first_page: u64, out: &mut [PhysAddr]) {
    let mut sets = FILEMAP.lock();
    let entry = resolve(sets.as_mut_slice(), map);
    for (page, slot) in (first_page..).zip(out.iter_mut()) {
        *slot = entry
            .as_ref()
            .and_then(|e| e.frame_at(page))
            .unwrap_or(PhysAddr::NULL);
    }
}

/// What the set already holds for the faulting page.
enum FaultProbe {
    /// Populated; the reference is already taken.
    Present(PhysAddr),
    /// Not populated, and neither are the pages after it up to the window.
    Missing(&'static dyn FileSystem, InodeId, u64),
}

fn probe_fault(map: FileMapRef, page_index: u64) -> Result<FaultProbe, FileMapError> {
    let mut sets = FILEMAP.lock();
    let entry = resolve(sets.as_mut_slice(), map).ok_or(FileMapError::Stale)?;
    let fs = entry.fs.ok_or(FileMapError::Stale)?;
    if entry.index_of(page_index).is_none() {
        return Err(FileMapError::Stale);
    }
    if let Some(pa) = entry.frame_at(page_index) {
        take_refs(entry, 1);
        revive(entry);
        return Ok(FaultProbe::Present(pa));
    }
    if entry.forgotten {
        return Err(FileMapError::Stale);
    }
    // A writable set is written back whole, so a page it reads ahead is a
    // page it later writes.
    let mut window = 1u64;
    while !entry.dirtyable
        && window < READAHEAD_PAGES
        && entry.index_of(page_index + window).is_some()
        && entry.frame_at(page_index + window).is_none()
    {
        window += 1;
    }
    Ok(FaultProbe::Missing(fs, entry.inode, window))
}

/// What [`install_page`] decided about the frame it was handed.
enum Install {
    Took(PhysAddr),
    /// Another CPU populated the page first; its frame is the answer.
    Lost(PhysAddr),
    Refused(FileMapError),
}

/// Publish one frame into the set, charging it to the set's owner.
fn install_page(map: FileMapRef, page_index: u64, pa: PhysAddr) -> Result<PhysAddr, FileMapError> {
    let ceiling = max_mapped_pages();
    let verdict = {
        let mut sets = FILEMAP.lock();
        let resolved = match resolve(sets.as_mut_slice(), map) {
            Some(entry) => entry.index_of(page_index).map(|idx| (entry.owner, idx)),
            None => None,
        };
        match resolved {
            None => Install::Refused(FileMapError::Stale),
            Some((owner, idx)) => {
                let mut held = POPULATED_PAGES.load(Ordering::Relaxed);
                if held >= ceiling && sets[map.slot as usize].pages[idx].is_null() {
                    // Never this set: a fault that found it parked is about to
                    // reference it.
                    evict_idle_except(
                        sets.as_mut_slice(),
                        held - ceiling + 1,
                        Some(u32::from(map.slot)),
                    );
                    held = POPULATED_PAGES.load(Ordering::Relaxed);
                }
                let entry = &mut sets[map.slot as usize];
                let taken = entry.pages[idx];
                if !taken.is_null() {
                    take_refs(entry, 1);
                    Install::Lost(taken)
                } else if held >= ceiling {
                    Install::Refused(FileMapError::TooManyPages)
                } else {
                    match try_charge::<PinnedBytesAxis>(owner, 1) {
                        Err(_) => Install::Refused(FileMapError::TooManyPages),
                        Ok(reservation) => {
                            entry.charge.grow(reservation);
                            entry.pages[idx] = pa;
                            entry.populated = entry.populated.saturating_add(1);
                            POPULATED_PAGES.fetch_add(1, Ordering::Relaxed);
                            take_refs(entry, 1);
                            Install::Took(pa)
                        }
                    }
                }
            }
        }
    };
    match verdict {
        Install::Took(pa) => Ok(pa),
        Install::Lost(winner) => {
            release_owned_anon_page(pa);
            Ok(winner)
        }
        Install::Refused(e) => {
            release_owned_anon_page(pa);
            Err(e)
        }
    }
}

/// Publish a page read ahead of any fault: no reference is taken, and a page
/// that lost a race or finds no room gives its frame back. Answers whether
/// the page went in.
fn install_readahead(map: FileMapRef, page_index: u64, pa: PhysAddr) -> bool {
    let ceiling = max_mapped_pages();
    let took = {
        let mut sets = FILEMAP.lock();
        match resolve(sets.as_mut_slice(), map) {
            Some(entry) => match entry.index_of(page_index) {
                Some(idx)
                    if entry.pages[idx].is_null()
                        && POPULATED_PAGES.load(Ordering::Relaxed) < ceiling =>
                {
                    match try_charge::<PinnedBytesAxis>(entry.owner, 1) {
                        Ok(reservation) => {
                            entry.charge.grow(reservation);
                            entry.pages[idx] = pa;
                            entry.populated = entry.populated.saturating_add(1);
                            POPULATED_PAGES.fetch_add(1, Ordering::Relaxed);
                            true
                        }
                        Err(_) => false,
                    }
                }
                _ => false,
            },
            None => false,
        }
    };
    if !took {
        release_owned_anon_page(pa);
    }
    took
}

/// One owned frame, claimed the way a memfd claims its pages, so it outlives
/// every mapping of it and aliasing it into a user PTE is legal.
fn claim_page() -> Result<PhysAddr, FileMapError> {
    let pa = alloc_kernel_page();
    if pa.is_null() {
        return Err(FileMapError::NoMemory);
    }
    if !claim_owned_anon_page(pa) {
        klog_info!("filemap: allocator returned a non-UNUSED frame; leaking it");
        return Err(FileMapError::NoMemory);
    }
    Ok(pa)
}

/// Bumped around every `write(2)` to a regular file that goes to the
/// filesystem rather than into a page set, odd while one is in flight, one
/// counter per inode hash. A fault that reads pages ahead installs them only
/// if the counter did not move across its read: such a write is ordered
/// against nothing else a fault holds, and a page read ahead of it would hide
/// it.
static WRITE_SEQ: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];

fn write_seq(inode: InodeId) -> &'static AtomicU32 {
    &WRITE_SEQ[((inode as u32).wrapping_mul(0x9E37_79B9) >> 26) as usize]
}

/// Bracket a filesystem write the page sets do not see: see [`WRITE_SEQ`].
pub fn around_uncovered_write<R>(inode: InodeId, write: impl FnOnce() -> R) -> R {
    let seq = write_seq(inode);
    seq.fetch_add(1, Ordering::AcqRel);
    let result = write();
    seq.fetch_add(1, Ordering::AcqRel);
    result
}

/// Read the file from page `page` into `staging`, whole pages, zero-filling
/// past EOF.
#[inline(never)]
fn read_range_into(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    page: u64,
    size: u64,
    staging: &mut [u8],
) -> Result<(), FileMapError> {
    let offset = page * PAGE_SIZE;
    let want = usize::try_from(size - offset)
        .unwrap_or(usize::MAX)
        .min(staging.len());
    let mut done = 0usize;
    while done < want {
        match fs.read(inode, offset + done as u64, &mut staging[done..want]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => return Err(FileMapError::Io),
        }
    }
    Ok(())
}

fn fill_frame(pa: PhysAddr, bytes: &[u8]) -> Result<(), FileMapError> {
    let virt = pa.try_to_virt().ok_or(FileMapError::Io)?;
    if !slopos_ostd::mm::hhdm_bytes::write_bytes(virt, 0, bytes) {
        return Err(FileMapError::Io);
    }
    Ok(())
}

/// Add `pages` mapping references; `false` if the handle is stale.
///
/// A *writable* mapping arms the writeback: a user store sets the CPU's PTE
/// dirty bit and nothing in this kernel harvests it, so every populated page of
/// such a set must be assumed written. Arming on a read-only mapping would
/// rewrite an unmodified file, stamping its timestamps and un-attesting its
/// blocks.
///
/// The set is re-homed to `holder` within that principal's share, because a
/// set whose owner has exited is charged to nobody and counted against
/// nobody's share. `fork` is the case that decides the timing: the child
/// retains while the parent is still alive, so waiting for an owner to die
/// means never re-homing at all. A holder at its share re-homes nothing, so
/// what a principal *holds* can exceed what it may *reserve* — `fork` is not
/// refusable for an accounting reason.
pub fn retain(map: FileMapRef, pages: u32, writable: bool, holder: AccountId) -> bool {
    let per_account = max_pages_per_account();
    let mut sets = FILEMAP.lock();
    let (held_sets, held_pages) = owned_by(sets.as_slice(), holder);
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return false;
    };
    take_refs(entry, pages);
    if writable {
        entry.dirtyable = true;
    }
    rehome(entry, holder, held_sets, held_pages, per_account);
    revive(entry);
    true
}

/// Sets and populated pages `owner` is charged for.
fn owned_by(sets: &[PageSet], owner: AccountId) -> (usize, u32) {
    if owner.is_none() {
        return (0, 0);
    }
    sets.iter()
        .filter(|entry| entry.fs.is_some() && entry.owner == owner)
        .fold((0usize, 0u32), |(count, pages), entry| {
            (count + 1, pages.saturating_add(entry.populated))
        })
}

/// Charge the set to `holder` instead of its current owner, when that fits in
/// `holder`'s share. A refused charge leaves the set as it was: failing the
/// retain would tear down a mapping over accounting.
fn rehome(
    entry: &mut PageSet,
    holder: AccountId,
    held_sets: usize,
    held_pages: u32,
    per_account: u32,
) {
    if holder.is_none() || entry.owner == holder {
        return;
    }
    let pages = entry.populated;
    if held_sets >= MAX_INODES_PER_ACCOUNT || held_pages.saturating_add(pages) > per_account {
        return;
    }
    if let Ok(reservation) = try_charge::<PinnedBytesAxis>(holder, pages) {
        entry.charge.put(reservation);
        entry.owner = holder;
    }
}

/// Drop `pages` mapping references, queueing the writeback and the frame frees
/// when the last one goes.
///
/// Must not block, allocate or reach the filesystem: it is reached from process
/// teardown, under a preempt guard. A forgotten set owes no writeback, so its
/// frames go back here rather than through the queue.
pub fn release(map: FileMapRef, pages: u32) {
    let mut sets = FILEMAP.lock();
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return;
    };
    entry.refs = entry.refs.saturating_sub(pages);
    if entry.refs != 0 {
        return;
    }
    if entry.forgotten {
        drop_set(entry);
        return;
    }
    if !entry.pending_release {
        if entry.pending_flush {
            entry.pending_flush = false;
        } else {
            PENDING.fetch_add(1, Ordering::Relaxed);
        }
        entry.pending_release = true;
    }
}

/// Flush an inode's pages and then unkey the set, for a name that is about to
/// be removed. Must run **before** the removal, while the inode's blocks are
/// still its own, and with no filesystem lock held.
pub fn detach_inode(fs: &'static dyn FileSystem, inode: InodeId) {
    let _ = flush_inode(fs, inode);
    forget_inode(fs, inode);
}

/// Take the set for `(fs, inode)` out of lookup and out of writeback.
///
/// Not a free: a live mapping keeps reading the frames, which go back when the
/// last mapping does. What ends here is the *identity* — the inode number may
/// be reallocated to another file the moment its name is gone.
pub fn forget_inode(fs: &'static dyn FileSystem, inode: InodeId) {
    let mut sets = FILEMAP.lock();
    let Some(entry) = sets.iter_mut().find(|e| e.holds(fs, inode)) else {
        return;
    };
    if entry.pending_release || entry.pending_flush {
        entry.pending_release = false;
        entry.pending_flush = false;
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    entry.forgotten = true;
    entry.dirtyable = false;
    if entry.refs == 0 {
        drop_set(entry);
    }
}

/// Take every set of `fs` out of lookup, for a filesystem instance about to
/// be handed to another mount: an idle set is dropped, a live one is
/// forgotten as an unlinked inode's is.
pub fn forget_filesystem(fs: &'static dyn FileSystem) {
    let mut sets = FILEMAP.lock();
    for entry in sets.iter_mut() {
        let Some(mine) = entry.fs else {
            continue;
        };
        if !same_filesystem(mine, fs) {
            continue;
        }
        if entry.pending_release || entry.pending_flush {
            entry.pending_release = false;
            entry.pending_flush = false;
            PENDING.fetch_sub(1, Ordering::Relaxed);
        }
        entry.forgotten = true;
        entry.dirtyable = false;
        if entry.refs == 0 {
            drop_set(entry);
        }
    }
}

/// Free a set's frames and retire its slot. The generation bump is what makes
/// every outstanding [`FileMapRef`] for it resolve to a miss.
fn drop_set(entry: &mut PageSet) {
    unpark(entry);
    // `PENDING` counts the sets carrying a flag, so clearing one here without
    // the matching decrement leaves the flusher's park predicate true forever.
    if entry.pending_release || entry.pending_flush {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    for pa in entry.pages.iter() {
        if pa.is_null() {
            continue;
        }
        // The set's own MetaSlot ref, claimed in `claim_page`; with no mapping
        // left this is the last, so the frame returns to the buddy.
        release_owned_anon_page(*pa);
    }
    entry.pages = KVec::new();
    POPULATED_PAGES.fetch_sub(entry.populated, Ordering::Relaxed);
    entry.populated = 0;
    entry.fs = None;
    entry.dirtyable = false;
    entry.pending_release = false;
    entry.pending_flush = false;
    entry.forgotten = false;
    entry.charge.take();
    entry.owner = AccountId::NONE;
    entry.generation = entry.generation.wrapping_add(1);
}

/// Write one set's pages back and wait for the filesystem to take them.
///
/// A live set that owes nothing — a forgotten one, whose pages are
/// deliberately not written back — answers `Ok`; only a handle naming no set
/// at all is [`FileMapError::Stale`].
pub fn flush(map: FileMapRef) -> Result<(), FileMapError> {
    let _io = io_lock()?;
    if !handle_is_live(map) {
        return Err(FileMapError::Stale);
    }
    let Some(job) = take_job_by_ref(map) else {
        return Ok(());
    };
    write_back(&job)
}

/// Does `map` still name a set, forgotten or not?
fn handle_is_live(map: FileMapRef) -> bool {
    let sets = FILEMAP.lock();
    sets.get(map.slot as usize)
        .is_some_and(|e| e.fs.is_some() && e.generation == map.generation)
}

/// [`flush`] for every set naming `inode`, for `fsync`/`sync`.
pub fn flush_inode(fs: &'static dyn FileSystem, inode: InodeId) -> Result<(), FileMapError> {
    let _io = io_lock()?;
    let Some(job) = take_job_by_inode(fs, inode) else {
        return Ok(());
    };
    write_back(&job)
}

/// Queue a writeback for `msync(MS_ASYNC)`; `false` if the handle names no set.
///
/// A forgotten set takes no queue entry: nothing would ever pick it up, and
/// the flusher's park predicate reads exactly that count.
pub fn queue_flush(map: FileMapRef) -> bool {
    let mut sets = FILEMAP.lock();
    let Some(entry) = resolve(sets.as_mut_slice(), map) else {
        return false;
    };
    if entry.forgotten {
        return true;
    }
    if !entry.pending_flush && !entry.pending_release {
        entry.pending_flush = true;
        PENDING.fetch_add(1, Ordering::Relaxed);
    }
    true
}

/// Sets owing writeback. Read by a writeback thread's wait predicate, which
/// must take no lock.
pub fn pending_count() -> usize {
    PENDING.load(Ordering::Relaxed)
}

/// One set's pages, lifted out from under the bookkeeping lock so the
/// writeback runs without it. Unpopulated pages keep their slot, so an index
/// is still `first_page`-relative.
struct WriteJob {
    slot: usize,
    generation: u32,
    fs: &'static dyn FileSystem,
    inode: InodeId,
    first_page: u64,
    pages: KVec<PhysAddr>,
    dirtyable: bool,
    release: bool,
}

/// Snapshot the set in `slot` for writeback, clearing whatever queue entry it
/// had — a set whose obligation is dropped here re-queues on its next
/// [`release`], so the loop in [`drain_pending`] always terminates.
///
/// A forgotten set is never a job: the blocks its pages came from may already
/// belong to something else.
fn take_job_slot(slot: usize) -> Option<WriteJob> {
    let mut sets = FILEMAP.lock();
    let entry = sets.get_mut(slot)?;
    let fs = entry.fs?;
    if entry.forgotten {
        return None;
    }
    // Staged **before** the obligation is cleared: a failure after the clear
    // would leave a set with no queue entry and nobody to free it.
    let mut pages: KVec<PhysAddr> = KVec::new();
    if pages.try_reserve_exact(entry.pages.len()).is_err() {
        return None;
    }
    for pa in entry.pages.iter() {
        if pages.push(*pa).is_err() {
            return None;
        }
    }
    if entry.pending_release || entry.pending_flush {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
    let release = entry.pending_release && entry.refs == 0;
    entry.pending_flush = false;
    entry.pending_release = false;
    Some(WriteJob {
        slot,
        generation: entry.generation,
        fs,
        inode: entry.inode,
        first_page: entry.first_page,
        pages,
        dirtyable: entry.dirtyable && entry.populated > 0,
        release,
    })
}

/// The set `map` names, if it is still live.
fn take_job_by_ref(map: FileMapRef) -> Option<WriteJob> {
    {
        let sets = FILEMAP.lock();
        let entry = sets.get(map.slot as usize)?;
        if entry.fs.is_none() || entry.generation != map.generation {
            return None;
        }
    }
    take_job_slot(map.slot as usize)
}

/// The set holding `(fs, inode)`, if any.
fn take_job_by_inode(fs: &'static dyn FileSystem, inode: InodeId) -> Option<WriteJob> {
    let slot = {
        let sets = FILEMAP.lock();
        sets.iter().position(|e| e.holds(fs, inode))?
    };
    take_job_slot(slot)
}

/// The next set owing writeback.
fn take_queued_job() -> Option<WriteJob> {
    let slot = {
        let sets = FILEMAP.lock();
        sets.iter()
            .position(|e| (e.pending_release || e.pending_flush) && !e.forgotten)?
    };
    take_job_slot(slot)
}

/// Write a job's pages out, then complete a queued release.
fn write_back(job: &WriteJob) -> Result<(), FileMapError> {
    let result = if job.dirtyable {
        write_pages(job)
    } else {
        Ok(())
    };
    if job.release {
        finish_release(job, result.is_ok());
    }
    result
}

/// The device-facing half, clamped to the file's current size: the last page of
/// a mapping may extend past EOF, and writing all of it would grow the file by
/// whatever the zero-fill put there.
///
/// A page nobody faulted is skipped: the set never held its bytes.
#[inline(never)]
fn write_pages(job: &WriteJob) -> Result<(), FileMapError> {
    let size = job.fs.stat(job.inode).map_err(|_| FileMapError::Io)?.size;
    let mut staging = KVec::<u8>::zeroed(PAGE_SIZE_USIZE).map_err(|_| FileMapError::NoMemory)?;
    for (i, pa) in job.pages.iter().enumerate() {
        if pa.is_null() {
            continue;
        }
        let offset = (job.first_page + i as u64) * PAGE_SIZE;
        if offset >= size {
            break;
        }
        let len = usize::try_from((size - offset).min(PAGE_SIZE)).unwrap_or(PAGE_SIZE_USIZE);
        let virt = pa.try_to_virt().ok_or(FileMapError::Io)?;
        if !slopos_ostd::mm::hhdm_bytes::read_bytes(virt, 0, &mut staging.as_mut_slice()[..len]) {
            return Err(FileMapError::Io);
        }
        let mut done = 0usize;
        while done < len {
            match job.fs.write(
                job.inode,
                offset + done as u64,
                &staging.as_slice()[done..len],
            ) {
                Ok(0) => return Err(FileMapError::Io),
                Ok(n) => done += n,
                Err(_) => return Err(FileMapError::Io),
            }
        }
    }
    Ok(())
}

/// The last mapping is gone and the writeback is out: keep the pages as an
/// idle cache of the file, or free the slot if there is nothing to keep — or
/// if the writeback failed, since the pages then hold bytes the filesystem
/// never took and a cache of them would answer reads with what it lost.
fn finish_release(job: &WriteJob, written: bool) {
    let mut sets = FILEMAP.lock();
    let entry = &mut sets[job.slot];
    if entry.generation != job.generation
        || entry.refs != 0
        || entry.fs.is_none()
        || entry.is_idle()
    {
        return;
    }
    if entry.populated == 0 || entry.forgotten || !written {
        drop_set(entry);
    } else {
        park_set(entry);
    }
}

/// Complete every queued writeback and frame free.
///
/// Blocks and reaches the filesystem, so the caller must be able to sleep and
/// hold no filesystem lock.
pub fn drain_pending() {
    if PENDING.load(Ordering::Relaxed) == 0 {
        return;
    }
    let Ok(_io) = io_lock() else {
        return;
    };
    while let Some(job) = take_queued_job() {
        run_job(&job);
    }
}

/// Write back every set, queued or live, and complete the queued frees — the
/// scope `sync(2)` and shutdown need, where a mapped page that never reached
/// the filesystem would be lost while the image was marked clean.
pub fn flush_all() {
    let Ok(_io) = io_lock() else {
        return;
    };
    for slot in 0..MAX_MAPPED_INODES {
        if let Some(job) = take_job_slot(slot) {
            run_job(&job);
        }
    }
}

/// The bytes are lost either way on a failure; re-queueing would spin against
/// a filesystem that is refusing writes.
fn run_job(job: &WriteJob) {
    if let Err(e) = write_back(job) {
        klog_info!("filemap: writeback of inode {} failed: {:?}", job.inode, e);
    }
}

/// What a page set holds around a file offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// The set holds this offset; it is the authority for the bytes there.
    Here,
    /// Nothing here, and possibly a page at this higher offset, so a chunk
    /// starting below must be cut there rather than run into it.
    Above(u64),
    /// Nothing here or above: the filesystem answers the whole chunk.
    Absent,
}

/// Where the page set for `(fs, inode)` stands relative to `offset`.
///
/// One lookup for both questions the `read(2)`/`write(2)` hooks ask, on the
/// path every regular-file chunk takes: an unmapped file pays one bounded scan
/// under a spinlock and no filesystem call.
///
/// An unfaulted page answers [`Coverage::Above`] the next page boundary rather
/// than scanning for the next populated one: the filesystem serves that page
/// and the caller asks again, which is O(1) per chunk however sparse the set.
pub fn coverage_at(fs: &'static dyn FileSystem, inode: InodeId, offset: u64) -> Coverage {
    let sets = FILEMAP.lock();
    let Some(entry) = sets.iter().find(|e| e.holds(fs, inode)) else {
        return Coverage::Absent;
    };
    if entry.populated == 0 {
        return Coverage::Absent;
    }
    if page_cursor(entry, offset).is_some() {
        return Coverage::Here;
    }
    let page = offset / PAGE_SIZE;
    if page < entry.first_page {
        Coverage::Above(entry.first_page * PAGE_SIZE)
    } else if page < entry.extent_end() {
        Coverage::Above((page + 1) * PAGE_SIZE)
    } else {
        Coverage::Absent
    }
}

/// Does the set hold `offset` itself?
pub fn covers_offset(fs: &'static dyn FileSystem, inode: InodeId, offset: u64) -> bool {
    coverage_at(fs, inode, offset) == Coverage::Here
}

/// Serve a `read(2)` from the page set, when it covers `offset`.
///
/// `None` means the caller must read the filesystem instead. A short answer is
/// the end of the covered range, not EOF. `buf` must already be clipped to the
/// file's length: the set holds whole pages, whose tail past EOF is zero-fill.
pub fn read_through(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
) -> Option<usize> {
    if buf.is_empty() {
        return None;
    }
    let sets = FILEMAP.lock();
    let entry = sets.iter().find(|e| e.holds(fs, inode))?;
    let (mut idx, mut page_off) = page_cursor(entry, offset)?;
    let mut done = 0usize;
    while done < buf.len() && idx < entry.pages.len() {
        let pa = entry.pages[idx];
        if pa.is_null() {
            break;
        }
        let take = (PAGE_SIZE_USIZE - page_off).min(buf.len() - done);
        let virt = pa.try_to_virt()?;
        if !slopos_ostd::mm::hhdm_bytes::read_bytes(virt, page_off, &mut buf[done..done + take]) {
            break;
        }
        done += take;
        idx += 1;
        page_off = 0;
    }
    (done > 0).then_some(done)
}

/// What [`write_through`] did with a `write(2)` chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteThrough {
    /// Bytes written into the page set; the caller advances by this many.
    Served(usize),
    /// No page set covers this offset — the caller writes the filesystem.
    NotCovered,
    /// The caller was killed while waiting for the writeback mutex. The
    /// filesystem must *not* be written behind the set's back: bytes landing
    /// there would leave every mapper reading stale pages.
    Interrupted,
}

/// Apply a `write(2)` to the page set, when it covers `offset`.
///
/// The bytes are not passed to the filesystem here: the set is the authority
/// for the pages it holds, and its writeback is what puts them on the device.
/// The answer stops at the first page nobody has faulted — the filesystem is
/// still the authority there, and the caller writes the rest itself.
pub fn write_through(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    offset: u64,
    buf: &[u8],
) -> WriteThrough {
    if buf.is_empty() {
        return WriteThrough::NotCovered;
    }
    // Ordered against an in-flight fault, whose read would otherwise land over
    // a write that arrived mid-read.
    let Ok(_io) = io_lock() else {
        return WriteThrough::Interrupted;
    };
    let mut sets = FILEMAP.lock();
    let Some(entry) = sets.iter_mut().find(|e| e.holds(fs, inode)) else {
        return WriteThrough::NotCovered;
    };
    // An idle set is a read cache: nothing would write it back, so a write
    // retires it and goes to the filesystem.
    if entry.is_idle() {
        drop_set(entry);
        return WriteThrough::NotCovered;
    }
    let Some((mut idx, mut page_off)) = page_cursor(entry, offset) else {
        return WriteThrough::NotCovered;
    };
    let mut done = 0usize;
    while done < buf.len() && idx < entry.pages.len() {
        let pa = entry.pages[idx];
        if pa.is_null() {
            break;
        }
        let take = (PAGE_SIZE_USIZE - page_off).min(buf.len() - done);
        let Some(virt) = pa.try_to_virt() else {
            break;
        };
        if !slopos_ostd::mm::hhdm_bytes::write_bytes(virt, page_off, &buf[done..done + take]) {
            break;
        }
        done += take;
        idx += 1;
        page_off = 0;
    }
    if done == 0 {
        return WriteThrough::NotCovered;
    }
    entry.dirtyable = true;
    WriteThrough::Served(done)
}

/// `(index into `pages`, byte offset in that page)` for a file offset the set
/// holds a frame for.
fn page_cursor(entry: &PageSet, offset: u64) -> Option<(usize, usize)> {
    let idx = entry.index_of(offset / PAGE_SIZE)?;
    if entry.pages[idx].is_null() {
        return None;
    }
    Some((idx, (offset % PAGE_SIZE) as usize))
}

struct FileMapHook;

static FILEMAP_HOOK: FileMapHook = FileMapHook;

impl FileMapOps for FileMapHook {
    fn retain(&self, map: FileMapRef, pages: u32, writable: bool, holder: AccountId) -> bool {
        retain(map, pages, writable, holder)
    }

    fn release(&self, map: FileMapRef, pages: u32) {
        release(map, pages);
    }

    fn drain(&self) {
        drain_pending();
    }

    fn fault_page(&self, map: FileMapRef, page_index: u64) -> Result<PhysAddr, i32> {
        fault_page_in_set(map, page_index).map_err(|e| e.to_errno().raw())
    }

    fn resident(&self, map: FileMapRef, first_page: u64, out: &mut [PhysAddr]) {
        resident_in_set(map, first_page, out);
    }
}

/// The registry, for `mm` to call on unmap, fork, fault and teardown.
pub fn filemap_ops() -> &'static dyn FileMapOps {
    &FILEMAP_HOOK
}

/// Page sets a mapping holds or a writeback owes, for tests and diagnostics.
/// Idle sets are a cache and not counted.
pub fn mapped_inode_count() -> usize {
    FILEMAP
        .lock()
        .iter()
        .filter(|e| e.fs.is_some() && !e.is_idle())
        .count()
}

/// Frames live sets hold; reserved-but-unfaulted pages and idle caches are not
/// among them.
pub fn populated_page_count() -> u32 {
    FILEMAP
        .lock()
        .iter()
        .filter(|e| !e.is_idle())
        .fold(0u32, |acc, e| acc.saturating_add(e.populated))
}

/// Pages idle sets hold as a cache of their files.
pub fn idle_page_count() -> u32 {
    IDLE_PAGES.load(Ordering::Relaxed)
}

/// Drop every idle set's pages. A test that measures frames or slots from a
/// known state starts here.
pub fn drop_idle_sets() -> u32 {
    let mut sets = FILEMAP.lock();
    evict_idle(sets.as_mut_slice(), u32::MAX)
}

struct FileMapReclaim;

impl slopos_ostd::mm::reclaim::Reclaimable for FileMapReclaim {
    fn name(&self) -> &'static str {
        "filemap-idle"
    }

    fn reclaimable_pages(&self) -> u32 {
        IDLE_PAGES.load(Ordering::Relaxed)
    }

    fn reclaim(&self, want: u32) -> u32 {
        match FILEMAP.try_lock() {
            Some(mut sets) => evict_idle(sets.as_mut_slice(), want),
            None => 0,
        }
    }
}

static FILEMAP_RECLAIM: FileMapReclaim = FileMapReclaim;

pub fn register_reclaim(token: &slopos_ostd::sync::BspToken<'_>) {
    slopos_ostd::mm::reclaim::register(token, &FILEMAP_RECLAIM);
}
