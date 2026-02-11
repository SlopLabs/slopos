use core::ptr;

use crate::error::MmError;
use crate::paging_defs::PageFlags;
use slopos_abi::addr::{PhysAddr, VirtAddr};

use crate::hhdm::PhysAddrHhdm;
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frame, free_page_frame, page_frame_get_ref};
use crate::paging::{ProcessPageDir, map_page_4kb_in_dir, paging_is_cow, virt_to_phys_in_dir};
use crate::paging_defs::PAGE_SIZE_4KB;
use crate::tlb;

pub fn handle_cow_fault(page_dir: *mut ProcessPageDir, fault_addr: u64) -> Result<(), MmError> {
    if page_dir.is_null() {
        return Err(MmError::NullPageDir);
    }

    let vaddr = VirtAddr::new(fault_addr);
    let aligned_vaddr = VirtAddr::new(fault_addr & !(PAGE_SIZE_4KB - 1));

    if !paging_is_cow(page_dir, vaddr) {
        return Err(MmError::NotCowPage);
    }

    let old_phys = virt_to_phys_in_dir(page_dir, aligned_vaddr);
    if old_phys.is_null() {
        return Err(MmError::InvalidAddress);
    }

    let ref_count = page_frame_get_ref(old_phys);

    if ref_count <= 1 {
        return resolve_single_ref(page_dir, aligned_vaddr);
    }

    resolve_multi_ref(page_dir, aligned_vaddr, old_phys)
}

fn resolve_single_ref(
    page_dir: *mut ProcessPageDir,
    aligned_vaddr: VirtAddr,
) -> Result<(), MmError> {
    let old_phys = virt_to_phys_in_dir(page_dir, aligned_vaddr);

    let new_flags = PageFlags::USER_RW;

    if map_page_4kb_in_dir(page_dir, aligned_vaddr, old_phys, new_flags.bits()) != 0 {
        return Err(MmError::MappingFailed);
    }

    tlb::flush_page(aligned_vaddr);
    Ok(())
}

fn resolve_multi_ref(
    page_dir: *mut ProcessPageDir,
    aligned_vaddr: VirtAddr,
    old_phys: PhysAddr,
) -> Result<(), MmError> {
    let new_phys = alloc_page_frame(ALLOC_FLAG_ZERO);
    if new_phys.is_null() {
        return Err(MmError::NoMemory);
    }

    let old_virt = old_phys.to_virt();
    let new_virt = new_phys.to_virt();

    if old_virt.is_null() || new_virt.is_null() {
        free_page_frame(new_phys);
        return Err(MmError::InvalidAddress);
    }

    unsafe {
        ptr::copy_nonoverlapping(
            old_virt.as_ptr::<u8>(),
            new_virt.as_mut_ptr::<u8>(),
            PAGE_SIZE_4KB as usize,
        );
    }

    let new_flags = PageFlags::USER_RW;

    if map_page_4kb_in_dir(page_dir, aligned_vaddr, new_phys, new_flags.bits()) != 0 {
        free_page_frame(new_phys);
        return Err(MmError::MappingFailed);
    }

    tlb::flush_page(aligned_vaddr);

    free_page_frame(old_phys);

    Ok(())
}

pub fn is_cow_fault(error_code: u64, page_dir: *mut ProcessPageDir, fault_addr: u64) -> bool {
    let is_write = (error_code & 0x02) != 0;
    let is_present = (error_code & 0x01) != 0;

    if !is_write || !is_present {
        return false;
    }

    paging_is_cow(page_dir, VirtAddr::new(fault_addr))
}
