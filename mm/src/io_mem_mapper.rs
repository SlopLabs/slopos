//! Bridges `slopos_ostd::mm::io_mem::IoMemMapper` to the kernel-virt
//! MMIO window allocator and the kernel-half mapping path.
//!
//! Lives in `slopos-mm` so `slopos-ostd` keeps no dependency on it; the
//! boot path registers it via `slopos_ostd::mm::register_io_mem_mapper`
//! before the init phases run.
//!
//! Cache policies index the firmware-default PAT ([`crate::pat`]): write
//! combining and write-through both take PA1 (WC, and WT against the
//! unmodified default).

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::{PhysAddr, VirtAddr};
use slopos_ostd::alignment::align_up_u64;
use slopos_ostd::mm::io_mem::{IoMemCachePolicy, IoMemError, IoMemMapper};
use slopos_ostd::mm::page_property::{CachePolicy, PageProperty};

use crate::kernel_mappings::kernel_map_io_4kb;
use crate::memory_layout_defs::{MMIO_VIRT_BASE, MMIO_VIRT_SIZE};
use crate::paging_defs::PAGE_SIZE_4KB;

static MMIO_NEXT_VIRT: AtomicU64 = AtomicU64::new(MMIO_VIRT_BASE);

fn alloc_virt(size: u64) -> Option<u64> {
    let aligned_size = align_up_u64(size, PAGE_SIZE_4KB);
    let mut current = MMIO_NEXT_VIRT.load(Ordering::Relaxed);
    loop {
        let new_next = current.checked_add(aligned_size)?;
        if new_next > MMIO_VIRT_BASE + MMIO_VIRT_SIZE {
            return None;
        }
        match MMIO_NEXT_VIRT.compare_exchange_weak(
            current,
            new_next,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

fn property_for(policy: IoMemCachePolicy) -> PageProperty {
    let cache_policy = match policy {
        IoMemCachePolicy::Uncacheable => CachePolicy::Uncacheable,
        IoMemCachePolicy::WriteCombining | IoMemCachePolicy::WriteThrough => {
            CachePolicy::WriteCombining
        }
        IoMemCachePolicy::WriteBack => CachePolicy::WriteBack,
    };
    PageProperty {
        cache_policy,
        ..PageProperty::KERNEL_RW
    }
}

pub struct KernelIoMemMapper;

pub static KERNEL_IO_MEM_MAPPER: KernelIoMemMapper = KernelIoMemMapper;

/// Doubly-indirect handle (`&'static &'static dyn IoMemMapper`) the OSTD
/// `register_io_mem_mapper` hook consumes; `pub` for the inline
/// registration in `boot::early_init::kernel_main_impl`.
pub static KERNEL_IO_MEM_MAPPER_DYN: &dyn IoMemMapper = &KERNEL_IO_MEM_MAPPER;

impl IoMemMapper for KernelIoMemMapper {
    fn map(
        &self,
        phys: PhysAddr,
        size: usize,
        policy: IoMemCachePolicy,
    ) -> Result<u64, IoMemError> {
        if phys.is_null() || size == 0 {
            return Err(IoMemError::MappingFailed);
        }
        let aligned_phys = phys.as_u64() & !(PAGE_SIZE_4KB - 1);
        let offset_in_page = phys.as_u64() - aligned_phys;
        let total = align_up_u64(offset_in_page + size as u64, PAGE_SIZE_4KB);
        let num_pages = total / PAGE_SIZE_4KB;
        let virt_base = alloc_virt(total).ok_or(IoMemError::MappingFailed)?;
        let prop = property_for(policy);
        for i in 0..num_pages {
            let page_phys = PhysAddr::new(aligned_phys + i * PAGE_SIZE_4KB);
            let page_virt = VirtAddr::new(virt_base + i * PAGE_SIZE_4KB);
            if kernel_map_io_4kb(page_virt, page_phys, prop).is_err() {
                return Err(IoMemError::MappingFailed);
            }
        }
        Ok(virt_base + offset_in_page)
    }

    fn unmap(&self, _virt: u64, _size: usize) {
        // TODO(tech-debt): unmap leaks the window — `MMIO_NEXT_VIRT` is
        // bump-only; needs a kernel-virt allocator with a recycle path.
    }
}
