//! Demand paging: the page-fault handler calls in here to allocate and map a
//! physical page the first time a lazy-anonymous VMA is touched.

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};

use slopos_ostd::mm::frame::{AnonymousMeta, Paddr};
use slopos_ostd::mm::uframe::UFrame;

use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm;
use crate::tlb;
use crate::user_mappings::{
    ostd_map_4kb_user, ostd_map_4kb_user_shared, ostd_virt_to_phys_4kb, wait_vm_space_exclusive,
};
use crate::vma_region::{Commit, FileMapRef, RegionPurpose, VmaMap, VmaRegion};

/// A demand-paging fault: the page is absent and `region` is lazily backed.
/// The file arm is serviced in two lock holds by [`plan_file_fault`] /
/// [`install_file_page`], because the read blocks.
pub fn is_demand_fault_in_region(error_code: u64, region: &VmaRegion) -> bool {
    let is_present = (error_code & 0x01) != 0;
    if is_present {
        return false;
    }
    region.is_demand_paged() && (region.is_anonymous() || region.filemap_ref().is_some())
}

/// [`is_demand_fault_in_region`] for a caller that has only a process.
pub fn is_demand_fault(
    error_code: u64,
    process: slopos_ostd::process::ProcessId,
    fault_addr: u64,
) -> bool {
    let Some(region) = process_vm::process_vm_get_region(process, fault_addr) else {
        return false;
    };
    is_demand_fault_in_region(error_code, &region)
}

pub fn can_satisfy_fault(error_code: u64, region: &VmaRegion) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_user = (error_code & 0x04) != 0;
    let is_ifetch = (error_code & 0x10) != 0;

    if is_user && !region.user {
        return false;
    }

    if is_write && !region.protection.write {
        return false;
    }

    if is_ifetch && !region.protection.exec {
        return false;
    }

    // A region with no protection at all is `PROT_NONE` — a guard page. The
    // arms above each test only the access the error code names, and a read
    // names none of them, so a read fault used to walk straight through. x86
    // has no read-disable bit: a frame installed here is published
    // `PRESENT | USER` and the guard reads back. Mirrors Linux's
    // `vma_is_accessible`; write-only stays readable, as it is on this
    // hardware.
    if !(region.protection.read || region.protection.write || region.protection.exec) {
        return false;
    }

    true
}

pub fn handle_demand_fault(
    vm_space: &mut KArc<VmSpace>,
    map: &mut VmaMap,
    fault_addr: u64,
    error_code: u64,
    region: &VmaRegion,
) -> Result<(), MmError> {
    let aligned_addr = fault_addr & !(PAGE_SIZE_4KB - 1);

    if !region.is_demand_paged() || !region.is_anonymous() {
        return Err(MmError::NotDemandPaged);
    }

    if !can_satisfy_fault(error_code, region) {
        return Err(MmError::PermissionDenied);
    }

    let existing_phys = ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(aligned_addr));
    if !existing_phys.is_null() {
        return Ok(());
    }

    // Before the allocation, so a wait that runs out frees nothing.
    if !wait_vm_space_exclusive(vm_space) {
        return Err(MmError::Retry);
    }

    let placed = region.commit == Commit::Frames;
    if placed && map.charge_frames(1).is_err() {
        return Err(MmError::NoMemory);
    }
    let outcome = place_fresh_page(vm_space, aligned_addr, region);
    if placed && outcome.is_err() {
        map.refund_frames(1);
    }
    outcome
}

/// Invalidate after mapping `va` where nothing was mapped. Only a translation
/// an earlier lazy unmap left on some CPU can be stale, so the shootdown is
/// owed only while one may linger; owing it also asks the epoch to close, so
/// the next fresh mapping is free again.
fn flush_fresh_mapping(vm_space: &VmSpace, va: VirtAddr) {
    flush_fresh_range(vm_space, va, VirtAddr::new(va.as_u64() + PAGE_SIZE_4KB));
}

/// [`flush_fresh_mapping`] for every page in `[start, end)` at once: a
/// fault-around pays one shootdown for its window, not one per page.
fn flush_fresh_range(vm_space: &VmSpace, start: VirtAddr, end: VirtAddr) {
    if end.as_u64() <= start.as_u64() || !crate::mmu::luf::may_hold_stale(vm_space.mm_ctx_handle())
    {
        return;
    }
    if end.as_u64() - start.as_u64() == PAGE_SIZE_4KB {
        tlb::flush_page(start);
    } else {
        tlb::flush_range(start, end);
    }
    crate::mmu::quiesce::request_advance();
}

/// Pages an anonymous fault maps ahead of the one it needs, to the end of this
/// aligned window: heaps and allocator arenas are touched in address order,
/// and each page faulted alone costs a trap and two holds of the per-process
/// lock. Linux maps anonymous memory a page or a huge page at a time; a window
/// is what a kernel without huge pages can do instead.
pub const ANON_FAULT_AROUND_PAGES: u64 = 32;

/// Place every page of `aligned_addr`'s aligned window that the region covers
/// and nothing maps yet — before the fault as well as after it, since an
/// allocator is as likely to walk a fresh span downwards — stopping at the
/// first page that cannot be placed.
///
/// Only for regions charged whole when they were created — prefaulting a
/// per-page charge would promise frames the process never asked for — and
/// only heap and anonymous `mmap` memory: a stack grows downwards, and the
/// loader's segments are placed eagerly already. Never reclaims: a prefault
/// is worth nothing to a machine short of memory.
pub fn fault_around_anon(
    vm_space: &mut KArc<VmSpace>,
    vma_start: u64,
    vma_end: u64,
    aligned_addr: u64,
    region: &VmaRegion,
) {
    if region.commit != Commit::Extent
        || !region.is_anonymous()
        || !region.is_demand_paged()
        || !matches!(region.purpose, RegionPurpose::General | RegionPurpose::Heap)
    {
        return;
    }
    let window = ANON_FAULT_AROUND_PAGES * PAGE_SIZE_4KB;
    let start = (aligned_addr & !(window - 1)).max(vma_start);
    let end = ((aligned_addr & !(window - 1)) + window).min(vma_end);
    let mut placed = (u64::MAX, 0u64);
    let mut va = start;
    while va < end {
        if va != aligned_addr && ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(va)).is_null() {
            if place_page(vm_space, va, region, false).is_err() {
                break;
            }
            placed = (placed.0.min(va), va + PAGE_SIZE_4KB);
        }
        va += PAGE_SIZE_4KB;
    }
    if placed.0 < placed.1 {
        flush_fresh_range(vm_space, VirtAddr::new(placed.0), VirtAddr::new(placed.1));
    }
}

fn place_fresh_page(
    vm_space: &mut KArc<VmSpace>,
    aligned_addr: u64,
    region: &VmaRegion,
) -> Result<(), MmError> {
    place_page(vm_space, aligned_addr, region, true)?;
    flush_fresh_mapping(vm_space, VirtAddr::new(aligned_addr));
    Ok(())
}

/// Map a fresh zeroed page at `aligned_addr`. Leaves the invalidation a
/// lazily unmapped predecessor may owe to the caller.
fn place_page(
    vm_space: &mut KArc<VmSpace>,
    aligned_addr: u64,
    region: &VmaRegion,
    reclaim: bool,
) -> Result<(), MmError> {
    // One bounded reclaim-and-retry: a demand fault has no syscall return
    // path to back off on. Here and not inside `try_charge` — the account
    // arena takes no locks by construction, and a reclaim hook there would
    // give it an inbound edge from every charge site at once.
    let mut phys = alloc_kernel_page();
    if phys.is_null() {
        if reclaim && slopos_ostd::mm::reclaim::run(1) != 0 {
            phys = alloc_kernel_page();
        }
        if phys.is_null() {
            return Err(MmError::NoMemory);
        }
    }

    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(phys.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(phys);
            slopos_ostd::klog_info!("demand::handle_demand_fault: claim failed: {:?}", e);
            return Err(MmError::MappingFailed);
        }
    };

    let pte_flags = region.to_page_flags().bits();
    // Sole ref, so dropping the refused frame is the free.
    if let Err((_, err)) =
        ostd_map_4kb_user(vm_space, VirtAddr::new(aligned_addr), frame, pte_flags)
    {
        if err == MapError::WouldBlock {
            return Err(MmError::Retry);
        }
        slopos_ostd::klog_info!("demand::handle_demand_fault: OSTD map failed: {:?}", err);
        return Err(MmError::MappingFailed);
    }
    Ok(())
}

/// What a file-backed fault needs after the per-process lock is dropped.
///
/// Deliberately no protection flags: an `mprotect` between the two holds
/// rewrites the region without touching this leaf, which is absent, so the
/// protection is re-read at install time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFaultPlan {
    pub map: FileMapRef,
    pub page_index: u64,
    pub private: bool,
    pub aligned_addr: u64,
    pub error_code: u64,
}

/// Decide a file-backed fault under the per-process lock. `Ok(None)` means the
/// page arrived while the fault was in flight.
pub fn plan_file_fault(
    vm_space: &KArc<VmSpace>,
    vma_start: u64,
    fault_addr: u64,
    error_code: u64,
    region: &VmaRegion,
) -> Result<Option<FileFaultPlan>, MmError> {
    let aligned_addr = fault_addr & !(PAGE_SIZE_4KB - 1);

    if !region.is_demand_paged() {
        return Err(MmError::NotDemandPaged);
    }
    if !can_satisfy_fault(error_code, region) {
        return Err(MmError::PermissionDenied);
    }
    if !ostd_virt_to_phys_4kb(vm_space, VirtAddr::new(aligned_addr)).is_null() {
        return Ok(None);
    }

    let offset_pages = (aligned_addr.saturating_sub(vma_start)) / PAGE_SIZE_4KB;
    let Some((map, page_index, private)) = region.file_page_at(offset_pages) else {
        return Err(MmError::NotDemandPaged);
    };

    Ok(Some(FileFaultPlan {
        map,
        page_index,
        private,
        aligned_addr,
        error_code,
    }))
}

/// Pages a file fault considers mapping: the aligned window around the one
/// that faulted, of which it maps what the page set already holds. Loaded
/// code is touched a page here and a page there, and each touch that finds
/// its page already mapped is a fault not taken.
pub const FAULT_AROUND_PAGES: usize = 32;

/// The first file page of the fault-around window holding `page_index`.
pub fn fault_around_first(page_index: u64) -> u64 {
    page_index & !(FAULT_AROUND_PAGES as u64 - 1)
}

/// How a page set's frame is published into `region`. A private mapping takes
/// it read-only and COW-marked: its first store copies it, which is the only
/// point at which a private mapping needs a page of its own.
fn set_frame_flags(region: &VmaRegion, private: bool) -> u64 {
    let flags = region.to_page_flags();
    if private {
        flags
            .difference(PageFlags::WRITABLE)
            .union(PageFlags::COW)
            .bits()
    } else {
        flags.bits()
    }
}

/// Install a page the filesystem just read, revalidated against the region
/// still covering the address: the mapping can have been unmapped or replaced
/// while the read was in flight.
///
/// `cached` is the set's frame, held alive by the caller's extra page
/// reference. A private mapping copies it into a page of its own only for a
/// write; a read maps the set's frame copy-on-write.
pub fn install_file_page(
    vm_space: &mut KArc<VmSpace>,
    vma_start: u64,
    plan: &FileFaultPlan,
    cached: PhysAddr,
    region: &VmaRegion,
) -> Result<(), MmError> {
    let offset_pages = (plan.aligned_addr.saturating_sub(vma_start)) / PAGE_SIZE_4KB;
    match region.file_page_at(offset_pages) {
        Some((map, page_index, private))
            if map == plan.map && page_index == plan.page_index && private == plan.private => {}
        _ => return Err(MmError::Retry),
    }
    // The region that authorised the read is not necessarily this one.
    if !can_satisfy_fault(plan.error_code, region) {
        return Err(MmError::PermissionDenied);
    }
    let pte_flags = region.to_page_flags().bits();

    let va = VirtAddr::new(plan.aligned_addr);
    if !ostd_virt_to_phys_4kb(vm_space, va).is_null() {
        return Ok(());
    }
    if !wait_vm_space_exclusive(vm_space) {
        return Err(MmError::Retry);
    }

    let is_write = plan.error_code & 0x02 != 0;
    if !plan.private || !is_write {
        let pte_flags = set_frame_flags(region, plan.private);
        return match ostd_map_4kb_user_shared(vm_space, va, cached, pte_flags) {
            Ok(()) => {
                flush_fresh_mapping(vm_space, va);
                Ok(())
            }
            Err(MapError::WouldBlock) => Err(MmError::Retry),
            Err(err) => {
                slopos_ostd::klog_info!("demand::install_file_page: shared map failed: {:?}", err);
                Err(MmError::MappingFailed)
            }
        };
    }

    let phys = alloc_kernel_page();
    if phys.is_null() {
        return Err(MmError::NoMemory);
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(phys.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(phys);
            slopos_ostd::klog_info!("demand::install_file_page: claim failed: {:?}", e);
            return Err(MmError::MappingFailed);
        }
    };
    let _ = slopos_ostd::mm::hhdm_bytes::copy_page(cached.to_virt(), phys.to_virt());

    match ostd_map_4kb_user(vm_space, va, frame, pte_flags) {
        Ok(()) => {
            flush_fresh_mapping(vm_space, va);
            Ok(())
        }
        Err((_, MapError::WouldBlock)) => Err(MmError::Retry),
        Err((_, err)) => {
            slopos_ostd::klog_info!("demand::install_file_page: private map failed: {:?}", err);
            Err(MmError::MappingFailed)
        }
    }
}

/// Map the pages of the fault-around window that the set already holds and
/// nothing maps yet. `frames[i]` is file page `first_page + i`, each held
/// alive by the caller's reference on the set. Best effort: a page outside
/// `[vma_start, vma_end)`, one the region no longer maps from this set, or a
/// refusal from the page tables simply stays absent for its own fault.
pub fn map_resident_around(
    vm_space: &mut KArc<VmSpace>,
    vma_start: u64,
    vma_end: u64,
    plan: &FileFaultPlan,
    first_page: u64,
    frames: &[PhysAddr],
    region: &VmaRegion,
) {
    let pte_flags = set_frame_flags(region, plan.private);
    let mut mapped = (u64::MAX, 0u64);
    for (page, &frame) in (first_page..).zip(frames) {
        if frame.is_null() || page == plan.page_index {
            continue;
        }
        let va = if page >= plan.page_index {
            (page - plan.page_index)
                .checked_mul(PAGE_SIZE_4KB)
                .and_then(|delta| plan.aligned_addr.checked_add(delta))
        } else {
            (plan.page_index - page)
                .checked_mul(PAGE_SIZE_4KB)
                .and_then(|delta| plan.aligned_addr.checked_sub(delta))
        };
        let Some(va) = va else {
            continue;
        };
        if va < vma_start || va >= vma_end {
            continue;
        }
        match region.file_page_at((va - vma_start) / PAGE_SIZE_4KB) {
            Some((map, index, private))
                if map == plan.map && index == page && private == plan.private => {}
            _ => continue,
        }
        let va = VirtAddr::new(va);
        if !ostd_virt_to_phys_4kb(vm_space, va).is_null() {
            continue;
        }
        if ostd_map_4kb_user_shared(vm_space, va, frame, pte_flags).is_err() {
            break;
        }
        mapped = (mapped.0.min(va.as_u64()), va.as_u64() + PAGE_SIZE_4KB);
    }
    if mapped.0 < mapped.1 {
        flush_fresh_range(vm_space, VirtAddr::new(mapped.0), VirtAddr::new(mapped.1));
    }
}
