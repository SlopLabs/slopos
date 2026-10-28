use core::ptr::NonNull;

use crate::hhdm::PhysAddrHhdm;
use crate::paging_defs::PAGE_SIZE_4KB;
use slopos_abi::addr::PhysAddr;
use slopos_ostd::mm::page_property::PageProperty;
use slopos_ostd::mm::page_table::PAGE_TABLE_ENTRIES;
use slopos_ostd::mm::page_table::PteFlags;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn is_present(&self) -> bool {
        self.0 & PteFlags::PRESENT.bits() != 0
    }

    #[inline]
    pub const fn is_huge(&self) -> bool {
        self.0 & PteFlags::HUGE.bits() != 0
    }

    #[inline]
    pub const fn address(&self) -> PhysAddr {
        PhysAddr(self.0 & PteFlags::ADDRESS_MASK)
    }

    /// The leaf's permissions and cache policy.
    #[inline]
    pub fn property(&self) -> PageProperty {
        PageProperty::from_leaf_flags(PteFlags::from_bits_truncate(self.0))
    }
}

impl core::fmt::Debug for PageTableEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PTE({:#x})", self.0)
    }
}

// The accessors pun a frame as `*mut u64`, so a `PageTableEntry` must stay
// exactly a `u64` in size and alignment.
const _: () = assert!(
    core::mem::size_of::<PageTableEntry>() == core::mem::size_of::<u64>()
        && core::mem::align_of::<PageTableEntry>() == core::mem::align_of::<u64>(),
    "a page-table entry is exactly a u64"
);
const _: () = assert!(
    PAGE_TABLE_ENTRIES * core::mem::size_of::<PageTableEntry>() == PAGE_SIZE_4KB as usize,
    "a page-table frame is one 4 KiB page of entries"
);

// Every kernel-half write goes through `slopos_ostd::mm::vm_space::CursorMut`
// under the `KERNEL_VM_SPACE` lock. Access is per-entry and atomic, never a
// reference over the frame: the hardware walker stamps Accessed and Dirty
// concurrently.

/// The HHDM view of the page-table frame at `phys`, as an entry array.
#[inline]
fn table_base_at(phys: PhysAddr) -> NonNull<u64> {
    debug_assert!(!phys.is_null(), "page-table frame address must be non-null");
    debug_assert!(
        phys.as_u64() % PAGE_SIZE_4KB == 0,
        "page-table frame address must be page-aligned"
    );
    NonNull::new(phys.to_virt().as_mut_ptr::<u64>())
        .expect("page-table frame address must be non-null")
}

#[inline]
pub(crate) fn entry_at(phys: PhysAddr, index: usize) -> PageTableEntry {
    debug_assert!(index < PAGE_TABLE_ENTRIES);
    slopos_ostd::util::ptr_buf::with_atomic_u64_in_page(table_base_at(phys), index, |slot| {
        PageTableEntry::from_raw(slot.load(core::sync::atomic::Ordering::Relaxed))
    })
}
