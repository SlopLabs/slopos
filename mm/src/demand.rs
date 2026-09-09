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
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::process_vm;
use crate::tlb;
use crate::user_mappings::{
    ostd_map_4kb_user, ostd_map_4kb_user_shared, ostd_virt_to_phys_4kb, vm_space_is_exclusive,
};
use crate::vma_region::{FileMapRef, VmaRegion};

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

    true
}

pub fn handle_demand_fault(
    vm_space: &mut KArc<VmSpace>,
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

    // Before the allocation: a retry storm past it would churn the buddy and reclaim.
    if !vm_space_is_exclusive(vm_space) {
        return Err(MmError::Retry);
    }

    // One bounded reclaim-and-retry: a demand fault has no syscall return
    // path to back off on. Here and not inside `try_charge` — the account
    // arena takes no locks by construction, and a reclaim hook there would
    // give it an inbound edge from every charge site at once.
    let mut phys = alloc_kernel_page();
    if phys.is_null() {
        if slopos_ostd::mm::reclaim::run(1) != 0 {
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

    tlb::flush_page(VirtAddr::new(aligned_addr));

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

/// Install a page the filesystem just read, revalidated against the region
/// still covering the address: the mapping can have been unmapped or replaced
/// while the read was in flight.
///
/// `cached` is the set's frame, held alive by the caller's extra page
/// reference; a private mapping copies it into a page of its own.
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
    if !vm_space_is_exclusive(vm_space) {
        return Err(MmError::Retry);
    }

    if !plan.private {
        return match ostd_map_4kb_user_shared(vm_space, va, cached, pte_flags) {
            Ok(()) => {
                tlb::flush_page(va);
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
            tlb::flush_page(va);
            Ok(())
        }
        Err((_, MapError::WouldBlock)) => Err(MmError::Retry),
        Err((_, err)) => {
            slopos_ostd::klog_info!("demand::install_file_page: private map failed: {:?}", err);
            Err(MmError::MappingFailed)
        }
    }
}
