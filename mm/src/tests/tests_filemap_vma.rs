//! A file-backed VMA's accounting.
//!
//! The pages belong to the filesystem's page set, so every teardown path must
//! tell the set how many page references it dropped and must not free a page
//! the set still owns. Neither is visible from a mapping's return value, so
//! these assert against the registry's own counters and the `MetaSlot`
//! refcount. The mapping is lazy, so the PTE only exists once a fault has run.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_abi::syscall::{MAP_PRIVATE, MAP_SHARED, PROT_READ, PROT_WRITE};
use slopos_ostd::mm::frame::{claim_owned_anon_page, reference_count_at, release_owned_anon_page};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::filemap_hook::{FileMapOps, filemap_swap_ops};
use crate::page_alloc::alloc_kernel_page;
use crate::process_vm::{process_vm_get_region, process_vm_mmap_file, process_vm_munmap};
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::vma_region::FileMapRef;
use slopos_abi::addr::PhysAddr;

const PAGES: u32 = 2;
const LENGTH: u64 = PAGES as u64 * 4096;

/// Stands in for the filesystem's page set: it counts what `mm` tells it.
struct CountingOps;

static RETAINED: AtomicU32 = AtomicU32::new(0);
static RELEASED: AtomicU32 = AtomicU32::new(0);
/// Retains that said the mapping can store into the pages.
static RETAINED_WRITABLE: AtomicU32 = AtomicU32::new(0);
static DRAINED: AtomicU32 = AtomicU32::new(0);
static FAULTED: AtomicU32 = AtomicU32::new(0);
/// The frames `fault_page` hands back, one per file page.
static FAULT_PAGES: [core::sync::atomic::AtomicU64; PAGES as usize] =
    [const { core::sync::atomic::AtomicU64::new(0) }; PAGES as usize];

static COUNTING_OPS: CountingOps = CountingOps;

impl FileMapOps for CountingOps {
    fn retain(
        &self,
        _map: FileMapRef,
        pages: u32,
        writable: bool,
        _holder: slopos_ostd::process::AccountId,
    ) -> bool {
        RETAINED.fetch_add(pages, Ordering::Relaxed);
        if writable {
            RETAINED_WRITABLE.fetch_add(pages, Ordering::Relaxed);
        }
        true
    }

    fn release(&self, _map: FileMapRef, pages: u32) {
        RELEASED.fetch_add(pages, Ordering::Relaxed);
    }

    fn drain(&self) {
        DRAINED.fetch_add(1, Ordering::Relaxed);
    }

    fn fault_page(&self, _map: FileMapRef, page_index: u64) -> Result<PhysAddr, i32> {
        FAULTED.fetch_add(1, Ordering::Relaxed);
        match page_index {
            0 => Ok(PhysAddr::new(FAULT_PAGES[0].load(Ordering::Relaxed))),
            1 => Ok(PhysAddr::new(FAULT_PAGES[1].load(Ordering::Relaxed))),
            _ => Err(slopos_abi::Errno::EINVAL.raw()),
        }
    }
}

/// Installs the counting registry and puts the real one back on drop, so a
/// failing assertion cannot leave the kernel's own page sets unreachable.
struct OpsSwap {
    previous: Option<&'static dyn FileMapOps>,
}

impl OpsSwap {
    fn install(pages: (PhysAddr, PhysAddr)) -> Self {
        RETAINED.store(0, Ordering::Relaxed);
        RELEASED.store(0, Ordering::Relaxed);
        RETAINED_WRITABLE.store(0, Ordering::Relaxed);
        DRAINED.store(0, Ordering::Relaxed);
        FAULTED.store(0, Ordering::Relaxed);
        FAULT_PAGES[0].store(pages.0.as_u64(), Ordering::Relaxed);
        FAULT_PAGES[1].store(pages.1.as_u64(), Ordering::Relaxed);
        Self {
            previous: filemap_swap_ops(Some(&COUNTING_OPS)),
        }
    }
}

impl Drop for OpsSwap {
    fn drop(&mut self) {
        filemap_swap_ops(self.previous);
    }
}

/// Two owned frames, claimed the way the page set claims its own.
fn claim_pages() -> Option<(PhysAddr, PhysAddr)> {
    let first = alloc_kernel_page();
    let second = alloc_kernel_page();
    if first.is_null() || second.is_null() {
        return None;
    }
    if !claim_owned_anon_page(first) || !claim_owned_anon_page(second) {
        return None;
    }
    Some((first, second))
}

fn drop_pages(pages: (PhysAddr, PhysAddr)) {
    release_owned_anon_page(pages.0);
    release_owned_anon_page(pages.1);
}

const MAP: FileMapRef = FileMapRef {
    slot: 3,
    generation: 9,
};

/// A lazy file mapping installs no PTE until a fault, then aliases the set's
/// page; `munmap` drops its references and leaves the pages with the set.
pub fn test_file_vma_faults_in_and_unmap_releases_without_freeing() -> TestResult {
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let _swap = OpsSwap::install(pages);
    let Some(vm) = ProcessVmGuard::new() else {
        drop_pages(pages);
        return fail!("create VM");
    };

    let va = process_vm_mmap_file(
        vm.process,
        0,
        LENGTH,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        MAP,
        0,
        false,
    );
    if va == 0 {
        drop_pages(pages);
        return fail!("the shared file mapping was refused");
    }

    let retained = RETAINED.load(Ordering::Relaxed);
    let before_fault = vm.virt_to_phys(va);
    let region = process_vm_get_region(vm.process, va);

    let faulted = vm.handle_file_fault(va, 0);
    let mapped = vm.virt_to_phys(va);
    let refs_mapped = reference_count_at(pages.0);

    let rc = process_vm_munmap(vm.process, va, LENGTH);
    let released = RELEASED.load(Ordering::Relaxed);
    let refs_unmapped = reference_count_at(pages.0);
    let still_mapped = vm.virt_to_phys(va);
    drop_pages(pages);

    assert_test!(
        retained == PAGES,
        "the mapping retained {} page refs, expected {}",
        retained,
        PAGES
    );
    assert_test!(
        before_fault.is_null(),
        "a lazy file mapping installed a PTE at map time ({:#x})",
        before_fault.as_u64()
    );
    assert_test!(
        region.is_some_and(|r| r.filemap_ref() == Some(MAP)),
        "the VMA does not name the page set it was mapped from"
    );
    assert_test!(faulted.is_ok(), "the file fault failed: {:?}", faulted);
    assert_test!(
        mapped.as_u64() == pages.0.as_u64(),
        "the faulted page maps {:#x}, expected {:#x}",
        mapped.as_u64(),
        pages.0.as_u64()
    );
    assert_test!(
        refs_mapped == 2,
        "a mapped page should hold the set's ref and the PTE's, holds {}",
        refs_mapped
    );
    assert_test!(rc == 0, "munmap of the file mapping failed: {}", rc);
    assert_test!(
        released == PAGES + 1,
        "munmap released {} page refs, expected {}",
        released,
        PAGES + 1
    );
    assert_test!(
        refs_unmapped == 1,
        "munmap left {} refs; the page set must keep holding the page",
        refs_unmapped
    );
    assert_test!(
        still_mapped.is_null(),
        "the range is still mapped after munmap"
    );
    pass!()
}

/// Address-space teardown is the path a process exit takes, and it runs under
/// a preempt guard: it must still tell the page set, and still not free.
pub fn test_file_vma_teardown_releases() -> TestResult {
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let _swap = OpsSwap::install(pages);
    let Some(vm) = ProcessVmGuard::new() else {
        drop_pages(pages);
        return fail!("create VM");
    };

    let va = process_vm_mmap_file(
        vm.process,
        0,
        LENGTH,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        MAP,
        0,
        false,
    );
    if va == 0 {
        drop_pages(pages);
        return fail!("the shared file mapping was refused");
    }
    let _ = vm.handle_file_fault(va, 0);

    drop(vm);

    let released = RELEASED.load(Ordering::Relaxed);
    let refs_after = reference_count_at(pages.0);
    drop_pages(pages);

    assert_test!(
        released == PAGES + 1,
        "teardown released {} page refs, expected {}",
        released,
        PAGES + 1
    );
    assert_test!(
        refs_after == 1,
        "teardown left {} refs on a page the set still owns, expected 1",
        refs_after
    );
    pass!()
}

/// A read-only shared file mapping must not arm the writeback, and an ordinary
/// `munmap` must complete what the release queued rather than leave it to a
/// flusher that may not exist.
pub fn test_file_vma_readonly_is_not_armed_and_munmap_drains() -> TestResult {
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let _swap = OpsSwap::install(pages);
    let Some(vm) = ProcessVmGuard::new() else {
        drop_pages(pages);
        return fail!("create VM");
    };

    let va = process_vm_mmap_file(vm.process, 0, LENGTH, PROT_READ, MAP_SHARED, MAP, 0, false);
    if va == 0 {
        drop_pages(pages);
        return fail!("the read-only file mapping was refused");
    }
    let armed = RETAINED_WRITABLE.load(Ordering::Relaxed);
    let retained = RETAINED.load(Ordering::Relaxed);

    let rc = process_vm_munmap(vm.process, va, LENGTH);
    let drained = DRAINED.load(Ordering::Relaxed);
    drop_pages(pages);

    assert_test!(
        rc == 0,
        "munmap of the read-only file mapping failed: {}",
        rc
    );
    assert_test!(
        retained == PAGES,
        "the mapping retained {} page refs, expected {}",
        retained,
        PAGES
    );
    assert_test!(
        armed == 0,
        "a PROT_READ mapping armed the writeback for {} page(s)",
        armed
    );
    assert_test!(
        drained >= 1,
        "munmap left the queued writeback for someone else ({} drains)",
        drained
    );
    pass!()
}

/// A `MAP_PRIVATE` fault copies the set's page rather than aliasing it, so a
/// store cannot reach the file.
pub fn test_private_file_vma_faults_into_its_own_page() -> TestResult {
    let Some(pages) = claim_pages() else {
        return fail!("claim the backing pages");
    };
    let _swap = OpsSwap::install(pages);
    let Some(vm) = ProcessVmGuard::new() else {
        drop_pages(pages);
        return fail!("create VM");
    };

    let va = process_vm_mmap_file(
        vm.process,
        0,
        LENGTH,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE,
        MAP,
        0,
        true,
    );
    if va == 0 {
        drop_pages(pages);
        return fail!("the private file mapping was refused");
    }

    let faulted = vm.handle_file_fault(va, 0);
    let mapped = vm.virt_to_phys(va);
    let set_refs = reference_count_at(pages.0);
    let armed = RETAINED_WRITABLE.load(Ordering::Relaxed);
    let rc = process_vm_munmap(vm.process, va, LENGTH);
    drop_pages(pages);

    assert_test!(
        faulted.is_ok(),
        "the private file fault failed: {:?}",
        faulted
    );
    assert_test!(
        !mapped.is_null() && mapped.as_u64() != pages.0.as_u64(),
        "a private mapping aliased the page set's frame {:#x}",
        mapped.as_u64()
    );
    assert_test!(
        set_refs == 1,
        "the set's page holds {} refs after a private fault, expected 1",
        set_refs
    );
    assert_test!(
        armed == 0,
        "a private mapping armed the writeback for {} page(s)",
        armed
    );
    assert_test!(rc == 0, "munmap of the private file mapping failed: {}", rc);
    pass!()
}

slopos_testing::stest!(
    name = test_file_vma_faults_in_and_unmap_releases_without_freeing,
    suite = filemap_vma
);
slopos_testing::stest!(name = test_file_vma_teardown_releases, suite = filemap_vma);
slopos_testing::stest!(
    name = test_file_vma_readonly_is_not_armed_and_munmap_drains,
    suite = filemap_vma
);
slopos_testing::stest!(
    name = test_private_file_vma_faults_into_its_own_page,
    suite = filemap_vma
);
