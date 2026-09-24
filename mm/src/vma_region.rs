//! Type-safe Virtual Memory Area subsystem.
//!
//! Each region's backing is an enum variant (not a flags bitfield), so the
//! compiler enforces exhaustive handling.
//!
//! Overlaps are prevented structurally: the gap finder returns addresses from
//! gaps between existing entries, and `insert` merges compatible adjacent
//! regions automatically.

use slopos_abi::quota::{CommitPagesAxis, PagesAxis, ResidentPagesAxis};
use slopos_ostd::KBTreeMap;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{ChargeSlot, Reservation, TryChargeError, try_charge};

use crate::memfd::MemfdHandle;
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

/// A generation-checked slot in the filesystem's per-inode page set.
///
/// Lives in `mm` because [`RegionBacking`] must stay `PartialEq` and `mm`
/// cannot name an fs type; [`crate::filemap_hook`] resolves it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMapRef {
    pub slot: u16,
    pub generation: u32,
}

/// What backs a memory region's physical pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionBacking {
    /// Anonymous zero-fill on demand (heap, stack, mmap MAP_ANONYMOUS).
    Anonymous,
    /// Shared memfd — pages belong to the MemfdObject, not the process.
    /// Must not be freed on munmap; only mapcount decrement.
    SharedMemfd { handle: MemfdHandle },
    /// File-backed mapping. Pages come from the filesystem's per-inode page
    /// set; `first_page` is the file page index at the region's start, so a
    /// fault still names the right page after a split. `private` copies the
    /// set's page into a page of the process's own on first touch, which unmap
    /// then frees; the reservation on the set is released either way.
    File {
        map: FileMapRef,
        first_page: u64,
        private: bool,
    },
    /// SlopRing shared region (SLOPRING § 5.1). The kernel-side ring object
    /// owns the frames as `Frame<RingMeta>`s and the user PTE holds an
    /// independent `from_in_use` ref, so a mapping outliving the fd cannot
    /// UAF; this VMA only reserves the virtual range. Not inherited across
    /// fork (the ring fd is close-on-fork — SLOPRING § 14).
    Ring,
}

/// Page protection bits. Separate from backing/state to prevent conflation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protection {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Protection {
    pub const RW: Self = Self {
        read: true,
        write: true,
        exec: false,
    };
    pub const RO: Self = Self {
        read: true,
        write: false,
        exec: false,
    };
    pub const RX: Self = Self {
        read: true,
        write: false,
        exec: true,
    };
    pub const NONE: Self = Self {
        read: false,
        write: false,
        exec: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionPurpose {
    /// Generic mmap'd region.
    General,
    /// brk-managed heap.
    Heap,
    /// Process stack.
    Stack,
    /// ELF .text (read+exec).
    Code,
    /// ELF .data/.bss (read+write).
    Data,
}

/// How a region's private pages are promised against the commit ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Commit {
    /// Costs nothing: the pages are owned elsewhere (a shared object, a file's
    /// page set) or no access can populate them (`PROT_NONE`).
    Unreserved,
    /// The whole span was charged when the region was created; a fault in it
    /// can therefore always find its frame accounted for.
    Extent,
    /// Each page is charged as it is placed — by the loader, by the stack
    /// growth fault, by fork's snapshot — and a refusal there is the one
    /// road that still ends in `SIGBUS`. Also what `MAP_NORESERVE` asks for.
    Frames,
}

/// A virtual memory region with typed backing, protection, and purpose.
#[derive(Clone, Debug)]
pub struct VmaRegion {
    pub protection: Protection,
    pub backing: RegionBacking,
    /// Demand-paged: physical pages not yet allocated (fault-on-access).
    pub lazy: bool,
    /// Copy-on-write: shared read-only until written (fork).
    pub cow: bool,
    /// User-mode accessible (Ring 3).
    pub user: bool,
    pub purpose: RegionPurpose,
    pub commit: Commit,
    /// `MAP_NORESERVE`: access makes this region [`Commit::Frames`], never
    /// [`Commit::Extent`], however its protection is later rewritten.
    pub noreserve: bool,
}

impl VmaRegion {
    /// A user region, its commit class derived from what can populate it.
    pub fn new(
        protection: Protection,
        backing: RegionBacking,
        lazy: bool,
        purpose: RegionPurpose,
    ) -> Self {
        let commit = Self::classify(&protection, &backing, lazy, purpose, false);
        Self {
            protection,
            backing,
            lazy,
            cow: false,
            user: true,
            purpose,
            commit,
            noreserve: false,
        }
    }

    /// The caller declined a reservation (`MAP_NORESERVE`): the pages are
    /// charged as they are touched and a shortfall is theirs to take.
    pub fn noreserve(mut self) -> Self {
        self.noreserve = true;
        self.commit = Self::classify(
            &self.protection,
            &self.backing,
            self.lazy,
            self.purpose,
            true,
        );
        self
    }

    fn classify(
        protection: &Protection,
        backing: &RegionBacking,
        lazy: bool,
        purpose: RegionPurpose,
        noreserve: bool,
    ) -> Commit {
        let accessible = protection.read || protection.write || protection.exec;
        match backing {
            RegionBacking::SharedMemfd { .. }
            | RegionBacking::Ring
            | RegionBacking::File { private: false, .. } => Commit::Unreserved,
            _ if !lazy => Commit::Frames,
            RegionBacking::Anonymous if purpose == RegionPurpose::Stack => Commit::Frames,
            RegionBacking::Anonymous if accessible && noreserve => Commit::Frames,
            RegionBacking::Anonymous if accessible => Commit::Extent,
            RegionBacking::File { private: true, .. } if protection.write => Commit::Extent,
            _ => Commit::Unreserved,
        }
    }

    /// The class the region holds once its protection is `prot`. A class is
    /// never given back: only an unreserved region moves, the first time a
    /// protection lets its pages be populated.
    pub fn commit_under(&self, prot: Protection) -> Commit {
        match self.commit {
            Commit::Unreserved => Self::classify(
                &prot,
                &self.backing,
                self.lazy,
                self.purpose,
                self.noreserve,
            ),
            held => held,
        }
    }

    /// Whether `prot` is what first makes this region owe its span.
    pub fn reserves_under(&self, prot: Protection) -> bool {
        self.commit == Commit::Unreserved && self.commit_under(prot) == Commit::Extent
    }

    /// Mergeable ignoring file position — every attribute but `first_page`.
    fn attributes_match(&self, other: &VmaRegion) -> bool {
        self.protection == other.protection
            && self.lazy == other.lazy
            && self.cow == other.cow
            && self.user == other.user
            && self.purpose == other.purpose
            && self.commit == other.commit
            && self.noreserve == other.noreserve
    }

    pub fn can_merge_with(&self, other: &VmaRegion) -> bool {
        self.attributes_match(other) && self.backing == other.backing
    }

    /// `self` spans `self_pages` and `other` begins where it ends: mergeable if
    /// every attribute matches and, for a file backing, the halves are
    /// consecutive in the file.
    ///
    /// `can_merge_with` cannot answer this — a split rebases the tail's
    /// `first_page`, so the halves never compare equal — and without it a
    /// `mprotect` that restored its protection would leave an entry per call.
    pub fn can_merge_before(&self, self_pages: u64, other: &VmaRegion) -> bool {
        if !self.attributes_match(other) {
            return false;
        }
        match (&self.backing, &other.backing) {
            (
                RegionBacking::File {
                    map,
                    first_page,
                    private,
                },
                RegionBacking::File {
                    map: other_map,
                    first_page: other_first,
                    private: other_private,
                },
            ) => {
                map == other_map
                    && private == other_private
                    && first_page.saturating_add(self_pages) == *other_first
            }
            _ => self.backing == other.backing,
        }
    }

    pub fn is_demand_paged(&self) -> bool {
        self.lazy
    }

    pub fn is_anonymous(&self) -> bool {
        matches!(self.backing, RegionBacking::Anonymous)
    }

    /// `true` iff the pages are owned outside this address space: unmap drops
    /// only this alias, and fork maps them verbatim instead of COW-marking.
    pub fn is_shared(&self) -> bool {
        matches!(
            self.backing,
            RegionBacking::SharedMemfd { .. } | RegionBacking::File { private: false, .. }
        )
    }

    /// `true` iff this region is a SlopRing shared mapping; like `is_shared()`,
    /// its PTEs must be unmapped *without* the anonymous-frame free path.
    pub fn is_ring(&self) -> bool {
        matches!(self.backing, RegionBacking::Ring)
    }

    pub fn memfd_handle(&self) -> Option<MemfdHandle> {
        match &self.backing {
            RegionBacking::SharedMemfd { handle } => Some(*handle),
            _ => None,
        }
    }

    pub fn filemap_ref(&self) -> Option<FileMapRef> {
        match &self.backing {
            RegionBacking::File { map, .. } => Some(*map),
            _ => None,
        }
    }

    /// The absolute file page index backing `offset_pages` pages into this
    /// region, or `None` when the region is not file-backed.
    pub fn file_page_at(&self, offset_pages: u64) -> Option<(FileMapRef, u64, bool)> {
        match &self.backing {
            RegionBacking::File {
                map,
                first_page,
                private,
            } => Some((*map, first_page.saturating_add(offset_pages), *private)),
            _ => None,
        }
    }

    /// The same region `offset_pages` further into the file. Splitting a
    /// file-backed VMA must rebase the tail, or its faults read the wrong page.
    pub fn rebased(&self, offset_pages: u64) -> Self {
        let mut out = self.clone();
        if let RegionBacking::File { first_page, .. } = &mut out.backing {
            *first_page = first_page.saturating_add(offset_pages);
        }
        out
    }

    pub fn to_page_flags(&self) -> PageFlags {
        let mut pf = PageFlags::PRESENT;
        if self.user {
            pf = pf.union(PageFlags::USER);
        }
        if self.cow {
            pf = pf.union(PageFlags::COW);
        } else if self.protection.write {
            pf = pf.union(PageFlags::WRITABLE);
        }
        if !self.protection.exec {
            pf = pf.union(PageFlags::NO_EXECUTE);
        }
        pf
    }
}

/// Pages spanned by the half-open range `[start, end)`.
#[inline]
fn range_pages(start: u64, end: u64) -> u32 {
    let bytes = end.saturating_sub(start);
    u32::try_from(bytes.div_ceil(PAGE_SIZE_4KB)).unwrap_or(u32::MAX)
}

/// A sorted map of non-overlapping virtual memory regions.
///
/// Key = start address, value = (end address, region).
/// All intervals are half-open: [start, end).
/// Invariant: no two entries overlap; maintained by construction.
///
/// One [`ChargeSlot<PagesAxis>`] covers the whole map rather than one per
/// [`VmaRegion`]: a scalar charge on a region cannot survive being split,
/// whereas the map itself is the carved set. [`link`](Self::link) and
/// [`unlink`](Self::unlink) are the only writers of both the tree and
/// `mapped_pages`, so the charge cannot drift; [`audit`](Self::audit) checks
/// that at runtime anyway.
///
/// The commit charge is the same shape over a different sum: the
/// [`Commit::Extent`] spans, kept by `link`/`unlink`, plus the pages the
/// [`Commit::Frames`] regions have placed, kept by `charge_frames`/`refund_frames`.
pub struct VmaMap {
    map: KBTreeMap<u64, (u64, VmaRegion)>,
    /// Pages the tree currently spans. Maintained incrementally by
    /// `link`/`unlink` rather than recomputed, so a mutation stays O(log n).
    mapped_pages: u32,
    extent_pages: u32,
    frame_pages: u32,
    /// Commit advanced to the loader ahead of the frames it will place.
    prepaid: u32,
    /// The account [`mapped_pages`](Self::mapped_pages) is charged to, kept
    /// separately because an empty slot names no account.
    account: AccountId,
    charge: ChargeSlot<PagesAxis>,
    commit: ChargeSlot<CommitPagesAxis>,
    /// Resident pages, synced from the address space's own leaf count: the
    /// cursor is the only place a user leaf appears, so a second count drifts.
    resident: ChargeSlot<ResidentPagesAxis>,
    /// The most leaves this process has held at once, across `execve`.
    peak_resident: u32,
}

impl VmaMap {
    pub const fn new() -> Self {
        Self {
            map: KBTreeMap::new(),
            mapped_pages: 0,
            extent_pages: 0,
            prepaid: 0,
            frame_pages: 0,
            account: AccountId::NONE,
            charge: ChargeSlot::empty(),
            commit: ChargeSlot::empty(),
            resident: ChargeSlot::empty(),
            peak_resident: 0,
        }
    }

    /// Name the principal this address space's pages are charged to.
    ///
    /// Anything already mapped is re-charged against the new account, so the
    /// binding order is not load-bearing.
    pub fn bind_account(&mut self, account: AccountId) {
        if self.account == account {
            return;
        }
        self.charge.take();
        self.commit.take();
        self.resident.take();
        self.account = account;
        if self.mapped_pages != 0
            && let Ok(reservation) = try_charge::<PagesAxis>(account, self.mapped_pages)
        {
            self.charge.put(reservation);
        }
        let committed = self
            .extent_pages
            .saturating_add(self.frame_pages)
            .saturating_add(self.prepaid);
        if committed != 0
            && let Ok(reservation) = try_charge::<CommitPagesAxis>(account, committed)
        {
            self.commit.put(reservation);
        }
    }

    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    #[inline]
    pub fn mapped_pages(&self) -> u32 {
        self.mapped_pages
    }

    /// Bring the resident charge in line with the address space's own count of
    /// present user leaves.
    ///
    /// Called leaving every hold of the per-process lock, so the ledger lags a
    /// mapping change by at most one hold. The axis is unlimited by default: a
    /// report of what is held, not a second ceiling on top of `Pages`.
    pub fn sync_resident(&mut self, resident: u32) {
        self.peak_resident = self.peak_resident.max(resident);
        let held = self.resident.amount();
        if resident > held {
            if let Ok(reservation) = try_charge::<ResidentPagesAxis>(self.account, resident - held)
            {
                self.resident.grow(reservation);
            }
        } else if resident < held {
            self.resident.shrink(held - resident);
        }
    }

    #[inline]
    pub fn resident_pages(&self) -> u32 {
        self.resident.amount()
    }

    #[inline]
    pub fn peak_resident_pages(&self) -> u32 {
        self.peak_resident
    }

    /// Pages the charge token currently holds.
    #[inline]
    pub fn charged_pages(&self) -> u32 {
        self.charge.amount()
    }

    /// Pages promised against the commit ceiling.
    #[inline]
    pub fn committed_pages(&self) -> u32 {
        self.commit.amount()
    }

    /// Promise `n` more pages for a [`Commit::Frames`] region, before they
    /// are placed. A refusal leaves the ledger untouched.
    pub fn charge_frames(&mut self, n: u32) -> Result<(), TryChargeError> {
        let advanced = n.min(self.prepaid);
        let rest = n - advanced;
        if rest != 0 {
            let reservation = try_charge::<CommitPagesAxis>(self.account, rest)?;
            self.commit.grow(reservation);
        }
        self.prepaid -= advanced;
        self.frame_pages = self.frame_pages.saturating_add(n);
        Ok(())
    }

    /// Hold `funds` as an advance on frames about to be placed: a charge
    /// taken while the previous image was still charged, so the loader that
    /// follows cannot be refused what its caller was already promised.
    pub fn prepay(&mut self, funds: Reservation<CommitPagesAxis>) {
        debug_assert_eq!(
            funds.account(),
            self.account,
            "VmaMap::prepay: an advance against another principal's row"
        );
        if funds.account() != self.account {
            return;
        }
        self.prepaid = self.prepaid.saturating_add(funds.amount());
        self.commit.grow(funds);
    }

    /// Give back whatever advance was not drawn.
    pub fn end_prepay(&mut self) {
        self.prepaid = 0;
        self.settle();
    }

    /// Give back the promise for `n` placed pages that are gone.
    pub fn refund_frames(&mut self, n: u32) {
        self.frame_pages = self.frame_pages.saturating_sub(n);
        self.settle();
    }

    /// Recompute the tree's span and report it beside `mapped_pages` and the
    /// charge — the runtime form of "the charge equals the map".
    pub fn audit(&self) -> (u32, u32, u32) {
        let walked = self.map.iter().fold(0u32, |acc, entry| {
            acc.saturating_add(range_pages(*entry.0, entry.1.0))
        });
        (walked, self.mapped_pages, self.charge.amount())
    }

    /// Add one entry to the tree. Tracks the spans; never touches a charge,
    /// which only [`settle`](Self::settle) and an `insert`'s reservation move.
    fn link(&mut self, start: u64, end: u64, region: VmaRegion) {
        let pages = range_pages(start, end);
        self.mapped_pages = self.mapped_pages.saturating_add(pages);
        if region.commit == Commit::Extent {
            self.extent_pages = self.extent_pages.saturating_add(pages);
        }
        self.map.insert(start, (end, region));
    }

    /// Remove one entry from the tree. Tracks the spans; never touches a charge.
    fn unlink(&mut self, start: u64) -> Option<(u64, VmaRegion)> {
        let (end, region) = self.map.remove(&start)?;
        let pages = range_pages(start, end);
        self.mapped_pages = self.mapped_pages.saturating_sub(pages);
        if region.commit == Commit::Extent {
            self.extent_pages = self.extent_pages.saturating_sub(pages);
        }
        Some((end, region))
    }

    /// Give back whatever each charge holds above what the tree accounts for.
    ///
    /// Only ever a shrink, so it is infallible: growth is always pre-reserved
    /// by the caller that wanted it, and a `munmap` must not be refusable
    /// against a ceiling it is *reducing* the use of.
    fn settle(&mut self) {
        self.charge
            .shrink(self.charge.amount().saturating_sub(self.mapped_pages));
        let committed = self
            .extent_pages
            .saturating_add(self.frame_pages)
            .saturating_add(self.prepaid);
        self.commit
            .shrink(self.commit.amount().saturating_sub(committed));
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Insert a region, merging with compatible adjacent regions.
    ///
    /// Charges `[start, end)` against this map's account before touching the
    /// tree — its span, and its commit too when the region reserves one — so
    /// a refusal leaves the address space exactly as it found it. A merge
    /// absorbs entries whose pages are already charged and widens the new
    /// entry by exactly as much, so the reservations taken here are the net
    /// growth however many neighbours merge.
    pub fn insert(
        &mut self,
        start: u64,
        end: u64,
        region: VmaRegion,
    ) -> Result<(), TryChargeError> {
        let reserved = self.reserve_pages(start, end)?;
        if region.commit == Commit::Extent {
            let committed = try_charge::<CommitPagesAxis>(self.account, range_pages(start, end))?;
            self.commit.grow(committed);
        }
        self.place(start, end, region, reserved);
        Ok(())
    }

    /// [`insert`](Self::insert) for a region that owes no span, with its page
    /// charge already taken.
    pub fn insert_unreserved(
        &mut self,
        start: u64,
        end: u64,
        region: VmaRegion,
        reservation: Reservation<PagesAxis>,
    ) {
        debug_assert_ne!(
            region.commit,
            Commit::Extent,
            "VmaMap::insert_unreserved: an extent region owes its span"
        );
        self.place(start, end, region, reservation);
    }

    /// Take the page charge for `[start, end)` without touching the tree.
    ///
    /// For a caller that must map before it can link: a refusal then happens
    /// before any page-table write. The reservation refunds itself if dropped.
    pub fn reserve_pages(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Reservation<PagesAxis>, TryChargeError> {
        try_charge::<PagesAxis>(self.account, range_pages(start, end))
    }

    /// Link `region` over `[start, end)`, merging with compatible neighbours.
    /// Every charge is already held: the pages by `reservation`, the span by
    /// the commit slot.
    fn place(
        &mut self,
        mut start: u64,
        mut end: u64,
        region: VmaRegion,
        reservation: Reservation<PagesAxis>,
    ) {
        let merge_pred = self
            .map
            .range(..start)
            .next_back()
            .filter(|entry| {
                entry.1.0 == start
                    && entry
                        .1
                        .1
                        .can_merge_before(range_pages(*entry.0, start) as u64, &region)
            })
            .map(|entry| *entry.0);
        // The absorbed neighbour's pages stay charged: they are re-linked below
        // as part of the widened entry, a move rather than a removal.
        if let Some(pred_start) = merge_pred {
            start = pred_start;
            self.unlink(pred_start);
        }

        let merge_succ = self
            .map
            .range(end..)
            .next()
            .filter(|entry| {
                *entry.0 == end
                    && region.can_merge_before(range_pages(start, end) as u64, &entry.1.1)
            })
            .map(|entry| (*entry.0, entry.1.0));
        if let Some((succ_start, succ_end)) = merge_succ {
            end = succ_end;
            self.unlink(succ_start);
        }

        #[cfg(debug_assertions)]
        {
            for entry in self.map.range(..end) {
                let s = *entry.0;
                let e = entry.1.0;
                debug_assert!(
                    e <= start,
                    "VmaMap::insert: overlap detected [{:#x},{:#x}) vs [{:#x},{:#x})",
                    s,
                    e,
                    start,
                    end
                );
            }
        }

        self.charge.grow(reservation);
        self.link(start, end, region);
        debug_assert_eq!(
            self.charge.amount(),
            self.mapped_pages,
            "VmaMap::insert left the page charge disagreeing with the tree"
        );
        debug_assert_eq!(
            self.commit.amount(),
            self.extent_pages
                .saturating_add(self.frame_pages)
                .saturating_add(self.prepaid),
            "VmaMap::insert left the commit charge disagreeing with the tree"
        );
    }

    /// Find the region containing address `addr`.
    pub fn find_containing(&self, addr: u64) -> Option<(u64, u64, &VmaRegion)> {
        let entry = self.map.range(..=addr).next_back()?;
        let start = *entry.0;
        let end = entry.1.0;
        let region = &entry.1.1;
        if addr < end {
            Some((start, end, region))
        } else {
            None
        }
    }

    /// Find a region that fully covers [start, end).
    pub fn find_covering(&self, start: u64, end: u64) -> Option<(u64, u64, &VmaRegion)> {
        let entry = self.map.range(..=start).next_back()?;
        let vma_start = *entry.0;
        let vma_end = entry.1.0;
        let region = &entry.1.1;
        if vma_start <= start && vma_end >= end {
            Some((vma_start, vma_end, region))
        } else {
            None
        }
    }

    /// Make `addr` an entry boundary by splitting the region that contains it.
    ///
    /// `false` when `addr` is already a boundary, is unaligned, or lies in no
    /// region. Page counts are additive across a page-aligned cut, so the
    /// tree's span and its charge are unchanged.
    pub fn split_at(&mut self, addr: u64) -> bool {
        if addr % PAGE_SIZE_4KB != 0 {
            return false;
        }
        let Some((vma_start, vma_end)) = self
            .find_containing(addr)
            .map(|(vma_start, vma_end, _)| (vma_start, vma_end))
        else {
            return false;
        };
        if addr == vma_start {
            return false;
        }
        let Some((_, region)) = self.unlink(vma_start) else {
            return false;
        };
        let shift = (addr - vma_start) / PAGE_SIZE_4KB;
        self.link(vma_start, addr, region.clone());
        self.link(addr, vma_end, region.rebased(shift));
        true
    }

    /// Rewrite the protection of every page in `[start, end)`.
    ///
    /// Splits at both ends so a sub-range does not rewrite its whole enclosing
    /// region, then re-merges them so repeated calls cannot grow the tree
    /// without bound. A region that `prot` makes populatable for the first
    /// time has its span committed here, before anything is rewritten, and
    /// keeps that commit whatever a later `mprotect` narrows it to.
    pub fn protect_range(
        &mut self,
        start: u64,
        end: u64,
        prot: Protection,
    ) -> Result<(), ProtectError> {
        let mut cursor = start;
        let mut owed = 0u32;
        while cursor < end {
            let Some((vma_start, vma_end, region)) = self.find_containing(cursor) else {
                return Err(ProtectError::Hole(cursor));
            };
            if region.reserves_under(prot) {
                owed = owed.saturating_add(range_pages(cursor.max(vma_start), vma_end.min(end)));
            }
            cursor = vma_end;
        }
        if owed != 0 {
            let commit =
                try_charge::<CommitPagesAxis>(self.account, owed).map_err(ProtectError::Commit)?;
            self.commit.grow(commit);
        }

        self.split_at(start);
        self.split_at(end);

        // Both ends are boundaries and the range is gap-free, so every cursor
        // value below is a key; `settle` holds the charge to what was linked.
        let mut cursor = start;
        while cursor < end
            && let Some((vma_end, mut region)) = self.unlink(cursor)
        {
            region.commit = region.commit_under(prot);
            region.protection = prot;
            self.link(cursor, vma_end, region);
            cursor = vma_end;
        }
        self.settle();

        self.coalesce_at(start);
        self.coalesce_at(end);
        Ok(())
    }

    /// Merge the entry ending at `addr` with the one starting there, when they
    /// have become compatible.
    fn coalesce_at(&mut self, addr: u64) {
        let Some(pred_start) = self
            .map
            .range(..addr)
            .next_back()
            .filter(|entry| entry.1.0 == addr)
            .map(|entry| *entry.0)
        else {
            return;
        };
        let pred_pages = (addr - pred_start) / PAGE_SIZE_4KB;
        let Some((succ_end, mergeable)) = self.map.get(&addr).and_then(|succ| {
            let pred = self.map.get(&pred_start)?;
            Some((succ.0, pred.1.can_merge_before(pred_pages, &succ.1)))
        }) else {
            return;
        };
        if !mergeable {
            return;
        }
        let Some((_, region)) = self.unlink(pred_start) else {
            return;
        };
        self.unlink(addr);
        self.link(pred_start, succ_end, region);
    }

    /// Find the first gap >= `size` bytes in [from, limit).
    pub fn find_gap(&self, from: u64, limit: u64, size: u64) -> Option<u64> {
        if size == 0 {
            return None;
        }

        let mut candidate = from;

        if let Some(entry) = self.map.range(..from).next_back() {
            let pred_end = entry.1.0;
            if pred_end > candidate {
                candidate = pred_end;
            }
        }

        for entry in self.map.range(from..) {
            let vma_start = *entry.0;
            let vma_end = entry.1.0;
            // Against `limit` too: a VMA past it -- the stack's growth
            // extent always is -- would otherwise make the gap before it look
            // unbounded.
            if candidate + size <= vma_start.min(limit) {
                return Some(candidate);
            }
            if vma_end > candidate {
                candidate = vma_end;
            }
        }

        if candidate + size <= limit {
            Some(candidate)
        } else {
            None
        }
    }

    /// Iterate all regions in address order: (start, end, &region).
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64, &VmaRegion)> {
        self.map
            .iter()
            .map(|entry| (*entry.0, entry.1.0, &entry.1.1))
    }

    /// Remove all regions overlapping [start, end), splitting at boundaries.
    /// Calls `on_removed(overlap_start, overlap_end, &region)` for each
    /// removed portion.
    ///
    /// Allocation-free: each round re-finds the first overlap rather than
    /// snapshotting a key list, so a wide unmap cannot fail for want of memory
    /// — `munmap` and teardown have no failure channel.
    pub fn remove_range(
        &mut self,
        start: u64,
        end: u64,
        mut on_removed: impl FnMut(u64, u64, &VmaRegion),
    ) {
        while let Some((vma_start, vma_end)) = self.first_overlapping(start, end) {
            let Some((_, region)) = self.unlink(vma_start) else {
                break;
            };
            on_removed(vma_start.max(start), vma_end.min(end), &region);

            if vma_start < start {
                self.link(vma_start, start, region.clone());
            }
            if vma_end > end {
                let shift = (end - vma_start) / PAGE_SIZE_4KB;
                self.link(end, vma_end, region.rebased(shift));
            }
        }
        // One settle for the whole range: the remnants are re-linked before the
        // charge is reconciled, so a split refunds exactly the carved hole.
        self.settle();
    }

    /// The first entry overlapping `[start, end)`. Each remnant a
    /// `remove_range` round re-links is outside the range, so iterating on this
    /// terminates.
    fn first_overlapping(&self, start: u64, end: u64) -> Option<(u64, u64)> {
        if let Some(entry) = self.map.range(..start).next_back()
            && entry.1.0 > start
        {
            return Some((*entry.0, entry.1.0));
        }
        self.map
            .range(start..end)
            .next()
            .map(|entry| (*entry.0, entry.1.0))
    }

    /// Drain all regions, calling `on_each(start, end, &region)` before removal.
    pub fn drain(&mut self, mut on_each: impl FnMut(u64, u64, &VmaRegion)) {
        while let Some(key) = self.map.keys().next().copied() {
            let Some((end, region)) = self.unlink(key) else {
                break;
            };
            on_each(key, end, &region);
        }
        self.frame_pages = 0;
        self.prepaid = 0;
        self.settle();
    }

    /// Clear all regions without callbacks, refunding every page.
    pub fn clear(&mut self) {
        self.map.clear();
        self.mapped_pages = 0;
        self.extent_pages = 0;
        self.frame_pages = 0;
        self.prepaid = 0;
        self.charge.take();
        self.commit.take();
        self.resident.take();
        self.peak_resident = 0;
    }
}

/// Why [`VmaMap::protect_range`] left the map as it found it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectError {
    /// The first address in the range no region covers.
    Hole(u64),
    /// The span the new protection would commit was refused.
    Commit(TryChargeError),
}
