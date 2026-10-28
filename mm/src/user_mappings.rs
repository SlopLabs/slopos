//! User-half mapping helpers, driving a process's OSTD `VmSpace` cursor over a
//! virtual address range: map, unmap, protect, the COW marker pair, and the
//! read-only queries the fault handlers ask. The kernel-half counterpart is
//! [`crate::kernel_mappings`].
//!
//! Every helper takes the address space it operates on, so the caller holds the
//! per-process lock for exactly as long as the cursor is open — see
//! `process_vm::process_vm_with_vm_space`.

use core::ops::ControlFlow;

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::klog_warn;
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{AnonymousMeta, FrameError, RingMeta};
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::page_size::Size4Kb;
use slopos_ostd::mm::page_table::PageTableLevel;
use slopos_ostd::mm::uframe::{AnyUFrameMeta, UFrame};
use slopos_ostd::mm::vm_space::{MapError, VmSpace};

use crate::paging_defs::PAGE_SIZE_4KB;

/// Bound on waiting out the readers of an address space. The readers are user
/// copies with preemption off, and a holder whose TLB flush waits on this
/// CPU's ack is served by the wait's own shootdown polling.
const VM_SPACE_MUT_SPINS: usize = 1_000_000;

/// The [`PageProperty::software`] bit marking a copy-on-write leaf: read-only,
/// and shared until the write fault that copies it.
pub const SOFTWARE_COW: u8 = 0b001;

const _: () = assert!(SOFTWARE_COW & PageProperty::SOFTWARE_NO_FRAME_REF == 0);

/// Whether `prop` marks a copy-on-write leaf.
#[inline]
pub fn is_cow(prop: &PageProperty) -> bool {
    prop.software & SOFTWARE_COW != 0
}

/// `prop` read-only and marked copy-on-write.
#[inline]
pub fn cow_marked(mut prop: PageProperty) -> PageProperty {
    prop.write = false;
    prop.software |= SOFTWARE_COW;
    prop
}

/// Advisory; `KArc::get_mut` is the authoritative check.
#[inline]
pub(crate) fn vm_space_is_exclusive(vm_space: &KArc<VmSpace>) -> bool {
    KArc::strong_count(vm_space) == 1 && KArc::weak_count(vm_space) == 0
}

/// Wait, under the per-process lock, for the readers holding `vm_space` to let
/// go. No reader can start meanwhile: taking a reference takes that lock.
/// `false` when the bound ran out first.
pub(crate) fn wait_vm_space_exclusive(vm_space: &KArc<VmSpace>) -> bool {
    for _ in 0..VM_SPACE_MUT_SPINS {
        if vm_space_is_exclusive(vm_space) {
            return true;
        }
        slopos_ostd::sync::spin_relax();
        core::hint::spin_loop();
    }
    vm_space_is_exclusive(vm_space)
}

#[inline]
fn vm_space_get_mut(vm_space: &mut KArc<VmSpace>) -> Result<&mut VmSpace, MapError> {
    if !wait_vm_space_exclusive(vm_space) {
        return Err(MapError::WouldBlock);
    }
    KArc::get_mut(vm_space).ok_or(MapError::WouldBlock)
}

fn log_frame_wrap_failure(what: &str, pa: PhysAddr, va: VirtAddr, e: FrameError) {
    let snap = slopos_ostd::mm::frame::slot_snapshot(pa);
    let (slots, max_pa, inited) = slopos_ostd::mm::frame::meta_slots_coverage();
    klog_warn!(
        "USERMAP: {}(Anon) FAILED pa=0x{:x} va=0x{:x} frame_err={:?} \
         slot_kind={:?} raw_rc=0x{:x} | META_SLOTS inited={} slots={} max_pa=0x{:x}",
        what,
        pa.as_u64(),
        va.as_u64(),
        e,
        snap.kind,
        snap.raw_ref_count,
        inited,
        slots,
        max_pa,
    );
}

/// Map a 4 KiB user page the caller **owns** into `vm_space` at `va`.
///
/// Takes the owning [`UFrame`] rather than a `PhysAddr` so the page has one
/// owner at every instant: on success the leaf PTE holds it, and on refusal it
/// comes back in the error. A caller holding a bare paddr across the call
/// could not tell which, so every error path would free the page twice.
///
/// Use [`map_4kb_user_shared`] for a page owned elsewhere.
///
/// # Errors
///
/// Returns the frame beside `MapError::Overlap` if a leaf is already present
/// at `va`, `MapError::IntermediateAllocFailed` on intermediate page-table
/// allocation failure, or any other variant the cursor surfaces.
pub fn map_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    frame: UFrame<AnonymousMeta>,
    prop: PageProperty,
) -> Result<(), (UFrame<AnonymousMeta>, MapError)> {
    let vs = match vm_space_get_mut(vm_space) {
        Ok(vs) => vs,
        Err(e) => return Err((frame, e)),
    };
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = match vs.cursor_mut(range) {
        Ok(c) => c,
        Err(e) => return Err((frame, e)),
    };
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}

/// Allocate a fresh zeroed page and map it at `va`, freeing it if the map is
/// refused. The common shape for demand paging, `brk` and ELF load: the caller
/// wants a page there and owns it only for the duration of the call.
///
/// `Err(MapError::IntermediateAllocFailed)` also covers the buddy being empty,
/// since to the caller both are "no page is mapped at `va`".
pub fn map_4kb_user_fresh(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    prop: PageProperty,
) -> Result<PhysAddr, MapError> {
    let pa = crate::page_alloc::alloc_kernel_page();
    if pa.is_null() {
        return Err(MapError::IntermediateAllocFailed);
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(pa) {
        Ok(f) => f,
        Err(e) => {
            log_frame_wrap_failure("claim_user_paddr", pa, va, e);
            crate::page_alloc::free_page_frame(pa);
            return Err(MapError::PathCorrupt);
        }
    };
    // Sole ref, so dropping the refused frame is the free.
    map_4kb_user(vm_space, va, frame, prop).map_err(|(_, e)| e)?;
    Ok(pa)
}

/// Map a 4 KiB user page owned by someone else — a memfd's registry entry, or
/// the parent's PTE across fork. Takes a `PhysAddr` because the caller has no
/// handle to give: the ref this adds is the mapping's own, and the page lives
/// until every holder drops.
///
/// A refusal drops that added ref and nothing else, so unlike
/// [`map_4kb_user`] there is nothing to hand back.
pub fn map_4kb_user_shared(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    prop: PageProperty,
) -> Result<(), MapError> {
    let frame = match UFrame::<AnonymousMeta>::alias_user_paddr(pa) {
        Ok(f) => f,
        Err(e) => {
            log_frame_wrap_failure("alias_user_paddr", pa, va, e);
            return Err(MapError::PathCorrupt);
        }
    };
    map_4kb_user(vm_space, va, frame, prop).map_err(|(_, e)| e)
}

/// Swap the 4 KiB user leaf at `va` for the caller-owned `frame`, returning
/// the frame the leaf held. One cursor, one walk, and no window in which two
/// owned frames are live across a fallible step.
///
/// `replace` invalidates this CPU only, so the displaced frame must outlive a
/// cross-CPU shootdown of `va`.
#[must_use = "dropping the displaced frame before the TLB shootdown frees a page a peer may still translate"]
#[allow(clippy::type_complexity)]
pub fn replace_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    frame: UFrame<AnonymousMeta>,
    prop: PageProperty,
) -> Result<Option<UFrame<AnonymousMeta>>, (UFrame<AnonymousMeta>, MapError)> {
    let vs = match vm_space_get_mut(vm_space) {
        Ok(vs) => vs,
        Err(e) => return Err((frame, e)),
    };
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = match vs.cursor_mut(range) {
        Ok(c) => c,
        Err(e) => return Err((frame, e)),
    };
    cursor.replace::<Size4Kb, AnonymousMeta>(frame, prop)
}

/// Unmap a 4 KiB user page from `vm_space` at `va`. The `UFrame` is dropped
/// inline, releasing its META_SLOTS ref and returning the page to the
/// registered [`FrameAlloc`] once that count hits zero.
///
/// `Ok(false)` means the leaf was already absent.
pub fn unmap_4kb_user(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, AnonymousMeta>()
        .map(|opt| opt.is_some())?)
}

/// Unmap a 4 KiB user leaf and return its frame instead of letting it drop
/// inline. The caller must hold the [`UFrame`] until after a cross-CPU TLB
/// shootdown of the range, so a freed frame cannot be reused while a peer CPU
/// still caches a stale translation. The cursor does the local invalidation;
/// the caller's half is to hold the frames, drop the lock, then issue one
/// shootdown for the whole range rather than one per page.
pub fn unmap_4kb_user_take(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
) -> Result<Option<UFrame<AnonymousMeta>>, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    cursor.unmap::<Size4Kb, AnonymousMeta>()
}

/// Map a 4 KiB SlopRing page into `vm_space` at `va`. The `RingMeta` slot at
/// `pa` must already be live — the ring object holds the first ref — and this
/// leaks a second one into the user PTE, so the page is freed
/// only once both drop and a mapping outliving the ring fd cannot UAF.
pub fn map_ring_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    va: VirtAddr,
    pa: PhysAddr,
    prop: PageProperty,
) -> Result<(), MapError> {
    // A dead `RingMeta` slot and a failed page-table walk both surface as
    // PathCorrupt, so the two failures are logged distinctly.
    let frame = match UFrame::<RingMeta>::from_in_use(pa) {
        Ok(f) => f,
        Err(e) => {
            let snap = slopos_ostd::mm::frame::slot_snapshot(pa);
            let (slots, max_pa, inited) = slopos_ostd::mm::frame::meta_slots_coverage();
            klog_warn!(
                "RINGMAP: from_in_use(RingMeta) FAILED pa=0x{:x} va=0x{:x} frame_err={:?} \
                 slot_kind={:?} raw_rc=0x{:x} vtable=0x{:x} | META_SLOTS inited={} \
                 slots={} max_pa=0x{:x}",
                pa.as_u64(),
                va.as_u64(),
                e,
                snap.kind,
                snap.raw_ref_count,
                snap.vtable_addr,
                inited,
                slots,
                max_pa,
            );
            return Err(MapError::PathCorrupt);
        }
    };
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    match cursor.map::<Size4Kb, RingMeta>(frame, prop) {
        Ok(()) => Ok(()),
        // Dropping the returned frame is correct here and only here: it is an
        // alias taken by `from_in_use`, so this releases the second ref the
        // failed map would have leaked into the PTE. The ring object's own ref
        // keeps the page alive.
        Err((_, e)) => {
            klog_warn!(
                "RINGMAP: cursor.map(RingMeta) FAILED pa=0x{:x} va=0x{:x} map_err={:?}",
                pa.as_u64(),
                va.as_u64(),
                e,
            );
            Err(e)
        }
    }
}

/// Unmap a 4 KiB SlopRing page from `vm_space` at `va`, releasing the PTE's
/// ref; the frame survives until the ring object drops its own, and vice
/// versa. `Ok(true)` means a leaf was present.
pub fn unmap_ring_4kb_user(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let range = va..VirtAddr::new(va.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    Ok(cursor
        .unmap::<Size4Kb, RingMeta>()
        .map(|opt| opt.is_some())?)
}

/// Apply `new`'s write and execute permissions to every 4 KiB leaf in
/// `[start, end)` that is currently present. Huge leaves are skipped.
pub fn protect_range_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    new: PageProperty,
) -> Result<(), MapError> {
    if end.as_u64() <= start.as_u64() {
        return Ok(());
    }
    let vs = vm_space_get_mut(vm_space)?;
    let mut cursor = vs.cursor_mut(start..end)?;
    let mut va = start.as_u64();
    while va < end.as_u64() {
        let entry = cursor.query();
        let cur = match entry {
            Ok(e) => e,
            Err(_) => return Ok(()),
        };
        if cur.paddr.is_some() {
            if cur.level == PageTableLevel::One {
                let mut prop = cur.property;
                // A COW leaf's frame is shared: it becomes writable only by
                // the fault that copies it, never by a protection change.
                prop.write = new.write && !is_cow(&prop);
                prop.execute = new.execute;
                cursor.protect::<Size4Kb>(prop)?;
            }
        }
        va = va.wrapping_add(PAGE_SIZE_4KB);
        cursor.advance(PAGE_SIZE_4KB)?;
    }
    Ok(())
}

/// Mark a single 4 KiB user page as copy-on-write: read-only, carrying
/// [`SOFTWARE_COW`].
pub fn mark_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<(), MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    if cur.paddr.is_some() && cur.level == PageTableLevel::One {
        cursor.protect::<Size4Kb>(cow_marked(cur.property))?;
    }
    Ok(())
}

/// Resolve a copy-on-write page for the single-ref case: writable, without
/// [`SOFTWARE_COW`]. `Ok(true)` if a 4 KiB leaf was present and updated.
pub fn resolve_cow_4kb(vm_space: &mut KArc<VmSpace>, va: VirtAddr) -> Result<bool, MapError> {
    let vs = vm_space_get_mut(vm_space)?;
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let mut cursor = vs.cursor_mut(range)?;
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return Ok(false),
    };
    if cur.paddr.is_some() && cur.level == PageTableLevel::One {
        let mut prop = cur.property;
        prop.write = true;
        prop.software &= !SOFTWARE_COW;
        cursor.protect::<Size4Kb>(prop)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// The property of the 4 KiB leaf at `va`, or `None` if no leaf is present at
/// that level.
pub fn leaf_property_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> Option<PageProperty> {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let cur = cursor.query().ok()?;
    if cur.paddr.is_none() {
        return None;
    }
    if cur.level != PageTableLevel::One {
        return None;
    }
    Some(cur.property)
}

/// Is the 4 KiB leaf at `va` both mapped and user-accessible? A kernel-half
/// page (USER bit clear) is mapped but answers `false`.
pub fn is_user_accessible_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> bool {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return false,
    };
    cur.paddr.is_some() && cur.level == PageTableLevel::One && cur.property.user
}

/// The first present 4 KiB leaf in `[from, to)`: its address, frame and
/// property.
/// An empty subtree is stepped over whole, so a sparse range costs what it
/// maps rather than its span; a huge leaf is stepped over like an empty one.
pub fn next_leaf_4kb(
    vm_space: &KArc<VmSpace>,
    from: VirtAddr,
    to: VirtAddr,
) -> Result<Option<(VirtAddr, PhysAddr, PageProperty)>, MapError> {
    if from >= to {
        return Ok(None);
    }
    let mut cursor = vm_space.cursor(from..to)?;
    loop {
        let entry = cursor.query()?;
        match entry.paddr {
            Some(paddr) if entry.level == PageTableLevel::One => {
                return Ok(Some((entry.vaddr, paddr, entry.property)));
            }
            _ => {
                let Some(next) = (entry.vaddr.as_u64() & entry.level.align_mask())
                    .checked_add(entry.level.entry_size())
                    .filter(|next| *next < to.as_u64())
                else {
                    return Ok(None);
                };
                cursor.seek(VirtAddr::new(next))?;
            }
        }
    }
}

/// Unmap every present 4 KiB leaf of `[start, end)` in one pass over its leaf
/// tables, handing each frame to `each` as [`unmap_4kb_user_take`] would,
/// with the property its leaf carried. An address space with nothing mapped
/// there is not waited on. `Err` names the address the failure struck at;
/// every leaf below it is gone.
pub fn unmap_present_4kb<M: AnyUFrameMeta>(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    mut each: impl FnMut(VirtAddr, PageProperty, UFrame<M>),
) -> Result<(), (MapError, VirtAddr)> {
    let Some((first, _, _)) = next_leaf_4kb(vm_space, start, end).map_err(|e| (e, start))? else {
        return Ok(());
    };
    let vs = vm_space_get_mut(vm_space).map_err(|e| (e, first))?;
    let mut cursor = vs.cursor_mut(first..end).map_err(|e| (e, first))?;
    cursor
        .unmap_present(|va, property, frame| each(va, property, frame))
        .map_err(|e| (e, cursor.vaddr()))
}

/// Offer every present 4 KiB leaf of `[start, end)` to `visit` as
/// `(vaddr, paddr, property)`, in one pass over its leaf tables, and mark
/// copy-on-write, as [`mark_cow_4kb`] does, each one it answers
/// `Continue(true)` for. `Ok(false)` when `visit` broke off.
pub fn visit_mark_cow_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    mut visit: impl FnMut(VirtAddr, PhysAddr, PageProperty) -> ControlFlow<(), bool>,
) -> Result<bool, MapError> {
    let Some((first, _, _)) = next_leaf_4kb(vm_space, start, end)? else {
        return Ok(true);
    };
    let vs = vm_space_get_mut(vm_space)?;
    let mut cursor = vs.cursor_mut(first..end)?;
    cursor.update_present(|entry| {
        let Some(paddr) = entry.paddr else {
            return ControlFlow::Continue(None);
        };
        match visit(entry.vaddr, paddr, entry.property) {
            ControlFlow::Continue(mark) => {
                ControlFlow::Continue(mark.then(|| cow_marked(entry.property)))
            }
            ControlFlow::Break(()) => ControlFlow::Break(()),
        }
    })
}

/// Which 4 KiB pages of `[start, end)` hold a present leaf, as bit `i` for
/// the page `i` pages past `start`, from one pass over the range's leaf
/// tables. A huge leaf reads as absent, as [`virt_to_phys_4kb`] has it.
/// The range spans at most 64 pages.
pub fn present_mask_4kb(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
) -> Result<u64, MapError> {
    debug_assert!(end.as_u64() - start.as_u64() <= 64 * PAGE_SIZE_4KB);
    let vs = vm_space_get_mut(vm_space)?;
    let mut cursor = vs.cursor_mut(start..end)?;
    let mut mask = 0u64;
    cursor.update_present(|entry| {
        mask |= 1 << ((entry.vaddr.as_u64() - start.as_u64()) / PAGE_SIZE_4KB);
        ControlFlow::Continue(None)
    })?;
    Ok(mask)
}

/// Map each `(vaddr, frame, property)` into `[start, end)` as
/// [`map_4kb_user`] would, descending once per leaf table the vaddrs
/// share. Returns how many were mapped; `Err` names the vaddr refused, whose
/// frame is dropped with the items not reached.
pub fn map_each_4kb_user(
    vm_space: &mut KArc<VmSpace>,
    start: VirtAddr,
    end: VirtAddr,
    items: impl IntoIterator<Item = (VirtAddr, UFrame<AnonymousMeta>, PageProperty)>,
) -> Result<usize, (MapError, VirtAddr)> {
    let vs = vm_space_get_mut(vm_space).map_err(|e| (e, start))?;
    let mut cursor = vs.cursor_mut(start..end).map_err(|e| (e, start))?;
    cursor.map_each(items).map_err(|(_, va, e)| (e, va))
}

/// Physical address backing the 4 KiB user leaf at `va`, or `PhysAddr::null()`
/// if no 4 KiB leaf is present — a huge leaf reads as null too.
pub fn virt_to_phys_4kb(vm_space: &KArc<VmSpace>, va: VirtAddr) -> PhysAddr {
    let aligned = VirtAddr::new(va.as_u64() & !(PAGE_SIZE_4KB - 1));
    let range = aligned..VirtAddr::new(aligned.as_u64().wrapping_add(PAGE_SIZE_4KB));
    let cursor = match vm_space.cursor(range) {
        Ok(c) => c,
        Err(_) => return PhysAddr::new(0),
    };
    let cur = match cursor.query() {
        Ok(e) => e,
        Err(_) => return PhysAddr::new(0),
    };
    match cur.paddr {
        Some(p) if cur.level == PageTableLevel::One => {
            // The result is byte-exact, so the page offset goes back in.
            let off = va.as_u64() & (PAGE_SIZE_4KB - 1);
            PhysAddr::new(p.as_u64() | off)
        }
        _ => PhysAddr::new(0),
    }
}
