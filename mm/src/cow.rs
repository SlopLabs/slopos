use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::mm::KArc;
use slopos_ostd::mm::frame::{AnonymousMeta, reference_count_at};
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::uframe::UFrame;
use slopos_ostd::mm::vm_space::{MapError, VmSpace};

use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::tlb;
use crate::user_mappings::{
    SOFTWARE_COW, is_cow, leaf_property_4kb, replace_4kb_user, resolve_cow_4kb, virt_to_phys_4kb,
    wait_vm_space_exclusive,
};
use crate::vma_region::{Commit, VmaMap, VmaRegion};

/// Copy a full 4 KiB page through the HHDM mapping. Both `src` and `dst`
/// must be live HHDM-mapped virtual addresses pointing at distinct pages.
#[inline]
fn copy_full_page(src: VirtAddr, dst: VirtAddr) {
    let _ = slopos_ostd::mm::hhdm_bytes::copy_page(src, dst);
}

/// Break COW at `fault_addr`. `prop` is what the covering region publishes a
/// leaf with, so a copy keeps the region's execute permission.
pub fn handle_cow_fault(
    vm_space: &mut KArc<VmSpace>,
    fault_addr: u64,
    prop: PageProperty,
) -> Result<(), MmError> {
    let vaddr = VirtAddr::new(fault_addr);
    let aligned_vaddr = VirtAddr::new(fault_addr & !(PAGE_SIZE_4KB - 1));

    if !leaf_property_4kb(vm_space, vaddr).is_some_and(|leaf| is_cow(&leaf)) {
        return Err(MmError::NotCowPage);
    }

    let old_phys = virt_to_phys_4kb(vm_space, aligned_vaddr);
    if old_phys.is_null() {
        return Err(MmError::InvalidAddress);
    }

    if !wait_vm_space_exclusive(vm_space) {
        return Err(MmError::Retry);
    }

    let ref_count = reference_count_at(PhysAddr::new(old_phys.as_u64() & !(PAGE_SIZE_4KB - 1)));

    if ref_count <= 1 {
        return resolve_single_ref(vm_space, aligned_vaddr);
    }

    let writable = PageProperty {
        write: true,
        software: prop.software & !SOFTWARE_COW,
        ..prop
    };
    resolve_multi_ref(vm_space, aligned_vaddr, old_phys, writable)
}

/// [`handle_cow_fault`] in `region`, charging the page through `map` when the
/// region is a forked copy: a broken page is this address space's own from
/// then on, whether the break copied it or found the frame no longer shared.
pub fn break_cow_in(
    vm_space: &mut KArc<VmSpace>,
    map: &mut VmaMap,
    fault_addr: u64,
    region: &VmaRegion,
) -> Result<(), MmError> {
    let owed = region.commit == Commit::Forked;
    if owed && map.charge_frames(1).is_err() {
        return Err(MmError::CommitRefused);
    }
    let broken = handle_cow_fault(vm_space, fault_addr, region.page_property());
    if owed && broken.is_err() {
        map.refund_frames(1);
    }
    broken
}

fn resolve_single_ref(
    vm_space: &mut KArc<VmSpace>,
    aligned_vaddr: VirtAddr,
) -> Result<(), MmError> {
    match resolve_cow_4kb(vm_space, aligned_vaddr) {
        Ok(true) => Ok(()),
        Ok(false) => Err(MmError::MappingFailed),
        Err(MapError::WouldBlock) => Err(MmError::Retry),
        Err(_) => Err(MmError::MappingFailed),
    }
}

fn resolve_multi_ref(
    vm_space: &mut KArc<VmSpace>,
    aligned_vaddr: VirtAddr,
    old_phys: PhysAddr,
    prop: PageProperty,
) -> Result<(), MmError> {
    let new_phys = alloc_kernel_page();
    if new_phys.is_null() {
        return Err(MmError::NoMemory);
    }

    let old_virt = old_phys.to_virt();
    let new_virt = new_phys.to_virt();

    if old_virt.is_null() || new_virt.is_null() {
        free_page_frame(new_phys);
        return Err(MmError::InvalidAddress);
    }

    copy_full_page(old_virt, new_virt);

    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(new_phys) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(new_phys);
            slopos_ostd::klog_info!("cow::resolve_multi_ref: claim failed: {:?}", e);
            return Err(MmError::MappingFailed);
        }
    };

    // A refusal returns the copy, whose drop frees it, and leaves the original
    // leaf in place, so the fault retries against an unmodified address space.
    let displaced = match replace_4kb_user(vm_space, aligned_vaddr, frame, prop) {
        Ok(displaced) => displaced,
        Err((_, err)) => {
            if err == MapError::WouldBlock {
                return Err(MmError::Retry);
            }
            slopos_ostd::klog_info!("cow::resolve_multi_ref: OSTD replace failed: {:?}", err);
            return Err(MmError::MappingFailed);
        }
    };

    tlb::flush_page(aligned_vaddr);
    // Possibly the old page's last reference; peers cached it until the flush above.
    drop(displaced);

    Ok(())
}

pub fn is_cow_fault(error_code: u64, vm_space: &KArc<VmSpace>, fault_addr: u64) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_present = (error_code & 0x01) != 0;

    if !is_write || !is_present {
        return false;
    }

    leaf_property_4kb(vm_space, VirtAddr::new(fault_addr)).is_some_and(|leaf| is_cow(&leaf))
}
