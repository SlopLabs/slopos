//! The kernel heap's own backing pages, charged to the root.
//!
//! Tier-1 object charges bound how many objects a principal may hold, never
//! how many pages the shared slab needed, so slab backing is charged to the
//! root rather than to the allocating principal.

use core::ffi::c_void;
use core::ptr::NonNull;

use slopos_abi::quota::ResourceKind;
use slopos_ostd::KVec;
use slopos_ostd::cpu::preempt::PreemptGuard;
use slopos_ostd::process::quota::{root, stats};
use slopos_testing::TestResult;
use slopos_testing::{assert_test, pass};

use crate::paging_defs::PAGE_SIZE_4KB;
use crate::slab::MAX_SLAB_CLASS_BYTES;
use crate::slab::large::{CACHE_PAGES, region_pages};
use crate::slab::page::charged_heap_pages;
use crate::slab::{kfree, kmalloc};

/// The request that takes a region of `pages`, its header included.
const fn bytes_of(pages: u32) -> usize {
    pages as usize * PAGE_SIZE_4KB as usize - 64
}

fn root_kernelmeta() -> u32 {
    stats(root(), ResourceKind::KernelMeta).map_or(0, |s| s.used)
}

/// The heap's charged page count never exceeds what the buddy handed out.
pub fn test_quota_heap_backing_reconciles_with_the_buddy() -> TestResult {
    let charged = charged_heap_pages();
    let allocated = crate::page_alloc::get_page_allocator_stats().allocated;
    assert_test!(
        charged > 0,
        "no heap backing is charged, yet the kernel has a heap"
    );
    assert_test!(
        charged <= allocated,
        "the heap claims {} pages but the buddy has only {} allocated",
        charged,
        allocated
    );
    assert_test!(
        root_kernelmeta() >= charged,
        "the root's kernelmeta row ({}) is below the heap backing it includes ({})",
        root_kernelmeta(),
        charged
    );
    pass!()
}

/// Preemption off, so the three readings are of this CPU's allocation alone.
pub fn test_quota_heap_large_alloc_is_charged() -> TestResult {
    const BYTES: usize = 4 * (MAX_SLAB_CLASS_BYTES + 1);

    let (ptr, before, with, after) = {
        let _preempt = PreemptGuard::new();
        let before = charged_heap_pages();
        let ptr = kmalloc(BYTES);
        let with = charged_heap_pages();
        kfree(ptr);
        (ptr, before, with, charged_heap_pages())
    };

    assert_test!(!ptr.is_null(), "large allocation failed");
    assert_test!(
        with >= before,
        "a large allocation lowered the heap's charge ({} -> {})",
        before,
        with
    );
    // A freed region may wait in the tier for the next request of its size.
    assert_test!(
        after <= with,
        "freeing a large allocation raised the heap's charge ({} -> {})",
        with,
        after
    );
    pass!()
}

/// A burst of frees leaves the tier holding no more than its cache: the
/// rest, and every region too large to keep, is the buddy's again and off
/// the root's charge.
pub fn test_quota_heap_large_frees_go_back_to_the_buddy() -> TestResult {
    const TOTAL_PAGES: u32 = 2 * CACHE_PAGES;

    for pages in [16u32, 32] {
        let regions = (TOTAL_PAGES / pages) as usize;
        let mut held: KVec<*mut c_void> = KVec::with_capacity(regions).expect("alloc");
        for _ in 0..regions {
            let p = kmalloc(bytes_of(pages));
            if p.is_null() {
                break;
            }
            held.push(p).expect("pre-reserved");
        }
        let peak = charged_heap_pages();
        for p in held.iter() {
            kfree(*p);
        }
        let back = peak.saturating_sub(charged_heap_pages());

        assert_test!(
            held.len() == regions,
            "only {} of {} regions of {} pages were allocated",
            held.len(),
            regions,
            pages
        );
        // Half of what must come back, for what other CPUs allocate meanwhile.
        assert_test!(
            back >= (TOTAL_PAGES - CACHE_PAGES) / 2,
            "freeing {} regions of {} pages gave the buddy {} pages back",
            regions,
            pages,
            back
        );
    }
    pass!()
}

/// A request is served a region of its own size, whatever larger one was
/// freed before it: what it left unused would be held for as long as the
/// allocation lives, as a task's one-page record held thirteen.
pub fn test_quota_heap_large_request_takes_no_larger_region() -> TestResult {
    const HELD: usize = 2 * CACHE_PAGES as usize;

    for pages in [2, 13, 16, 40] {
        kfree(kmalloc(bytes_of(pages)));
    }
    let mut held: KVec<*mut c_void> = KVec::with_capacity(HELD).expect("alloc");
    let mut larger = 0usize;
    for _ in 0..HELD {
        let p = kmalloc(MAX_SLAB_CLASS_BYTES + 1);
        let Some(region) = NonNull::new(p.cast::<u8>()) else {
            break;
        };
        if region_pages(region) != Some(1) {
            larger += 1;
        }
        held.push(p).expect("pre-reserved");
    }
    for p in held.iter() {
        kfree(*p);
    }

    assert_test!(
        held.len() == HELD,
        "only {} of {} one-page regions were allocated",
        held.len(),
        HELD
    );
    assert_test!(
        larger == 0,
        "{} of {} one-page requests were served a larger region",
        larger,
        HELD
    );
    pass!()
}

/// Slab refills move the root's row, and the ledger stays consistent.
pub fn test_quota_heap_slab_refill_moves_the_root_row() -> TestResult {
    let before = charged_heap_pages();

    // Enough 512-byte objects to exhaust several pages' worth of one class.
    let mut held: KVec<*mut core::ffi::c_void> = KVec::with_capacity(256).expect("alloc");
    for _ in 0..256 {
        let p = kmalloc(512);
        if p.is_null() {
            break;
        }
        held.push(p).expect("pre-reserved");
    }
    let peak = charged_heap_pages();
    for p in held.iter() {
        kfree(*p);
    }

    assert_test!(
        peak >= before,
        "256 slab allocations lowered the heap's charge ({} -> {})",
        before,
        peak
    );

    let mut faults = 0usize;
    slopos_ostd::process::quota::ledger_audit(|_| faults += 1);
    assert_test!(
        faults == 0,
        "the ledger is inconsistent after {} slab refills",
        peak.saturating_sub(before)
    );
    pass!()
}

slopos_testing::stest!(
    name = test_quota_heap_backing_reconciles_with_the_buddy,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_large_alloc_is_charged,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_large_frees_go_back_to_the_buddy,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_large_request_takes_no_larger_region,
    suite = quota_heap
);
slopos_testing::stest!(
    name = test_quota_heap_slab_refill_moves_the_root_row,
    suite = quota_heap
);
