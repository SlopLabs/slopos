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
//! frames arrive one page at a time from [`fault_page_in_set`], which runs on
//! the faulting task's kernel stack and may block on the filesystem. A slot
//! holding [`PhysAddr::NULL`] is a page nobody has faulted yet, for which the
//! filesystem is still the authority. Mapping a file therefore costs frames
//! only for the pages a process touches.
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

/// Inodes that may hold a page set at once.
pub(crate) const MAX_MAPPED_INODES: usize = 128;

/// The registry is `MAX_MAPPED_INODES` of these, so another field is a
/// deliberate 128 bytes of BSS rather than an accident.
const _: () = assert!(core::mem::size_of::<PageSet>() <= 96);

/// The fraction of usable physical memory the registry may pin, and the
/// fraction of that ceiling one principal may hold.
const MAPPED_PAGE_SHARE: u32 = 4;

/// Floor for a machine whose usable memory is not known yet — the 4 MiB this
/// registry was fixed at before the ceiling was derived.
const MIN_MAPPED_PAGES: u32 = 1024;

/// Pages one set's index may describe. The index is a single `KVec<PhysAddr>`
/// against a 1 MiB `MAX_ALLOC_SIZE`, so this holds the allocation at 512 KiB
/// and still describes 256 MiB of file.
const MAX_SET_PAGES: u32 = 65536;

/// Slots one principal may hold. Kernel work (`AccountId::NONE`) is outside
/// the share, as it is outside ext2's block reserve: it is not a principal.
pub(crate) const MAX_INODES_PER_ACCOUNT: usize = MAX_MAPPED_INODES / MAPPED_PAGE_SHARE as usize;

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
    max_mapped_pages() / MAPPED_PAGE_SHARE
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
        None => (free.ok_or(FileMapError::TooManyInodes)?, true),
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
    let taking = if rehome { sets[slot].populated } else { 0 };
    if !owner.is_none() && owned_pages.saturating_add(taking) > max_pages_per_account() {
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
        Some(try_charge::<PinnedBytesAxis>(owner, taking).map_err(|_| FileMapError::TooManyPages)?)
    } else {
        None
    };

    let entry = &mut sets[slot];
    if rehome {
        match reservation {
            Some(reservation) => entry.charge.put(reservation),
            None => entry.charge.take(),
        }
        entry.owner = owner;
    }
    if let Some(pages) = widened {
        entry.pages = pages;
        entry.first_page = union_first;
    }
    if fresh {
        entry.fs = Some(fs);
        entry.inode = inode;
        entry.generation = entry.generation.wrapping_add(1);
        entry.dirtyable = false;
    }
    entry.refs = entry.refs.saturating_add(page_count);
    revive(entry);

    Ok(ref_for(slot, entry.generation))
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
    let _io = io_lock()?;
    let (fs, inode) = match probe_fault(map, page_index)? {
        FaultProbe::Present(pa) => return Ok(pa),
        FaultProbe::Missing(fs, inode) => (fs, inode),
    };

    let size = fs.stat(inode).map_err(|_| FileMapError::Io)?.size;
    if page_index.saturating_mul(PAGE_SIZE) >= size {
        return Err(FileMapError::PastEof);
    }

    let mut staging = KVec::<u8>::zeroed(PAGE_SIZE_USIZE).map_err(|_| FileMapError::NoMemory)?;
    let pa = claim_page()?;
    if let Err(e) = read_page_into(fs, inode, page_index, size, pa, staging.as_mut_slice()) {
        release_owned_anon_page(pa);
        return Err(e);
    }
    install_page(map, page_index, pa)
}

/// What the set already holds for the faulting page.
enum FaultProbe {
    /// Populated; the reference is already taken.
    Present(PhysAddr),
    Missing(&'static dyn FileSystem, InodeId),
}

fn probe_fault(map: FileMapRef, page_index: u64) -> Result<FaultProbe, FileMapError> {
    let mut sets = FILEMAP.lock();
    let entry = resolve(sets.as_mut_slice(), map).ok_or(FileMapError::Stale)?;
    let fs = entry.fs.ok_or(FileMapError::Stale)?;
    if entry.index_of(page_index).is_none() {
        return Err(FileMapError::Stale);
    }
    if let Some(pa) = entry.frame_at(page_index) {
        entry.refs = entry.refs.saturating_add(1);
        revive(entry);
        return Ok(FaultProbe::Present(pa));
    }
    if entry.forgotten {
        return Err(FileMapError::Stale);
    }
    Ok(FaultProbe::Missing(fs, entry.inode))
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
    let per_account = max_pages_per_account();
    let verdict = {
        let mut sets = FILEMAP.lock();
        let resolved = match resolve(sets.as_mut_slice(), map) {
            Some(entry) => entry.index_of(page_index).map(|idx| (entry.owner, idx)),
            None => None,
        };
        match resolved {
            None => Install::Refused(FileMapError::Stale),
            Some((owner, idx)) => {
                let mut held = 0u32;
                let mut owned = 0u32;
                for entry in sets.iter() {
                    held = held.saturating_add(entry.populated);
                    if entry.fs.is_some() && !owner.is_none() && entry.owner == owner {
                        owned = owned.saturating_add(entry.populated);
                    }
                }
                let entry = &mut sets[map.slot as usize];
                let taken = entry.pages[idx];
                if !taken.is_null() {
                    entry.refs = entry.refs.saturating_add(1);
                    Install::Lost(taken)
                } else if held >= ceiling || (!owner.is_none() && owned >= per_account) {
                    Install::Refused(FileMapError::TooManyPages)
                } else {
                    match try_charge::<PinnedBytesAxis>(owner, 1) {
                        Err(_) => Install::Refused(FileMapError::TooManyPages),
                        Ok(reservation) => {
                            entry.charge.grow(reservation);
                            entry.pages[idx] = pa;
                            entry.populated = entry.populated.saturating_add(1);
                            entry.refs = entry.refs.saturating_add(1);
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

/// Read one file page into `pa`, zero-filling past EOF.
#[inline(never)]
fn read_page_into(
    fs: &'static dyn FileSystem,
    inode: InodeId,
    page: u64,
    size: u64,
    pa: PhysAddr,
    staging: &mut [u8],
) -> Result<(), FileMapError> {
    let offset = page * PAGE_SIZE;
    let want = usize::try_from((size - offset).min(PAGE_SIZE)).unwrap_or(PAGE_SIZE_USIZE);
    staging.fill(0);
    let mut done = 0usize;
    while done < want {
        match fs.read(inode, offset + done as u64, &mut staging[done..want]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(_) => return Err(FileMapError::Io),
        }
    }
    let virt = pa.try_to_virt().ok_or(FileMapError::Io)?;
    if !slopos_ostd::mm::hhdm_bytes::write_bytes(virt, 0, staging) {
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
    entry.refs = entry.refs.saturating_add(pages);
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

/// Free a set's frames and retire its slot. The generation bump is what makes
/// every outstanding [`FileMapRef`] for it resolve to a miss.
fn drop_set(entry: &mut PageSet) {
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
        finish_release(job);
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

/// Free the frames and the slot, once the writeback has gone out.
fn finish_release(job: &WriteJob) {
    let mut sets = FILEMAP.lock();
    let entry = &mut sets[job.slot];
    if entry.generation != job.generation || entry.refs != 0 || entry.fs.is_none() {
        return;
    }
    drop_set(entry);
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
}

/// The registry, for `mm` to call on unmap, fork, fault and teardown.
pub fn filemap_ops() -> &'static dyn FileMapOps {
    &FILEMAP_HOOK
}

/// Page sets currently held, for tests and diagnostics.
pub fn mapped_inode_count() -> usize {
    FILEMAP.lock().iter().filter(|e| e.fs.is_some()).count()
}

/// Frames the registry holds; reserved-but-unfaulted pages are not among them.
pub fn populated_page_count() -> u32 {
    FILEMAP
        .lock()
        .iter()
        .fold(0u32, |acc, e| acc.saturating_add(e.populated))
}
