use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use super::tests::resolve_pid;
use slopos_ostd::klog_info;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::cow::is_cow_fault;
use crate::error::MmError;
use crate::hhdm::PhysAddrHhdm;

use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{process_vm_clone_cow, process_vm_with_vm_space};
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::user_mappings::ostd_map_4kb_user_fresh;
use slopos_abi::addr::VirtAddr;
use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_ostd::mm::frame::{AnonymousMeta, Frame, Paddr, reference_count_at};
use slopos_ostd::test_support::page_io;

pub fn test_cow_read_not_cow_fault() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = vm.map_test_page(0x2000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(0x2000);

    let error_code_read: u64 = 0x05;
    let cow_fault =
        process_vm_with_vm_space(vm.process, |vs| is_cow_fault(error_code_read, vs, 0x2000))
            .unwrap_or(false);
    assert_test!(!cow_fault, "is_cow_fault returned true for read access");

    pass!()
}

pub fn test_cow_not_present_not_cow() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x5000_0000;
    let error_code: u64 = 0x02;

    let cow_fault =
        process_vm_with_vm_space(vm.process, |vs| is_cow_fault(error_code, vs, unmapped_addr))
            .unwrap_or(false);
    assert_test!(
        !cow_fault,
        "is_cow_fault returned true for not-present page"
    );

    pass!()
}

pub fn test_cow_dispatch_absent_for_a_reaped_process() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("could not create a process VM");
    };
    let stale = vm.process;
    drop(vm);

    let result = crate::process_vm::process_vm_with_vm_space(stale, |_| ());
    if result.is_some() {
        return fail!("process_vm_with_vm_space resolved a reaped process");
    }
    pass!()
}

pub fn test_cow_handle_not_cow_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(_phys) = vm.map_test_page(0x3000, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    match vm.handle_cow_fault(0x3000) {
        Err(MmError::NotCowPage) => pass!(),
        Ok(_) => fail!("handle_cow_fault succeeded on non-COW page"),
        Err(e) => fail!("wrong error for non-COW page: {:?}", e),
    }
}

pub fn test_cow_single_ref_upgrade() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let Some(phys) = vm.map_test_page(0x4000, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(0x4000);

    let ref_before = reference_count_at(Paddr::new(phys.as_u64()));
    assert_test!(ref_before == 1, "initial META_SLOTS refcount should be 1");

    if let Err(e) = vm.handle_cow_fault(0x4000) {
        return fail!("single-ref COW failed: {:?}", e);
    }

    let phys_after = vm.virt_to_phys(0x4000);
    if phys_after != phys {
        klog_info!(
            "COW_TEST: Single-ref COW copied page unnecessarily! {:#x} -> {:#x}",
            phys.as_u64(),
            phys_after.as_u64()
        );
    }

    assert_test!(!vm.is_cow(0x4000), "page still marked COW after resolution");

    pass!()
}

pub fn test_cow_multi_ref_copy() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let test_addr: u64 = 0x5000;
    let Some(phys) = vm.map_test_page(test_addr, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        page_io::fill_indexed(ptr, PAGE_SIZE_4KB as usize, |i| (i & 0xFF) as u8);
    }

    vm.mark_cow(test_addr);

    // Equivalent to two extra processes mapping the same paddr.
    let extra1 = Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64()))
        .expect("from_in_use 1 for COW multi-ref test");
    let extra2 = Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64()))
        .expect("from_in_use 2 for COW multi-ref test");

    let ref_before = reference_count_at(Paddr::new(phys.as_u64()));
    if ref_before < 3 {
        klog_info!("COW_TEST: Expected refcount >=3, got {}", ref_before);
    }

    if let Err(e) = vm.handle_cow_fault(test_addr) {
        drop(extra1);
        drop(extra2);
        return fail!("multi-ref COW failed: {:?}", e);
    }

    let phys_after = vm.virt_to_phys(test_addr);
    assert_test!(phys_after != phys, "multi-ref COW didn't copy page");

    if let Some(virt) = phys_after.to_virt_checked() {
        let ptr = virt.as_ptr::<u8>();
        if let Some(i) = page_io::verify_indexed(ptr, PAGE_SIZE_4KB as usize, |i| (i & 0xFF) as u8)
        {
            let val = page_io::read_byte(ptr, i);
            let expected = (i & 0xFF) as u8;
            return fail!(
                "data not copied correctly at offset {}: expected {:#x}, got {:#x}",
                i,
                expected,
                val
            );
        }
    }

    let ref_after = reference_count_at(Paddr::new(phys.as_u64()));
    assert_test!(ref_after < ref_before, "old page refcount didn't decrement");

    drop(extra1);
    drop(extra2);

    pass!()
}

pub fn test_cow_page_boundary() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let page_start: u64 = 0x6000;
    let Some(_phys) = vm.map_test_page(page_start, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };

    vm.mark_cow(page_start);

    let fault_addr = page_start + PAGE_SIZE_4KB - 1;
    if let Err(e) = vm.handle_cow_fault(fault_addr) {
        return fail!("boundary COW failed: {:?}", e);
    }

    pass!()
}

pub fn test_cow_clone_modify_both() -> TestResult {
    use crate::process_vm::process_vm_alloc;

    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    let test_addr = process_vm_alloc(
        parent.process,
        PAGE_SIZE_4KB,
        PageFlags::WRITABLE.bits() as u32,
    );
    assert_test!(test_addr != 0, "process_vm_alloc failed");

    let Some(phys) = parent.map_test_page(test_addr, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    if let Some(virt) = phys.to_virt_checked() {
        let ptr = virt.as_mut_ptr::<u8>();
        page_io::fill_pattern(ptr, 0xAA, PAGE_SIZE_4KB as usize);
    }

    let Some(child) = parent.clone_cow() else {
        return fail!("COW clone failed");
    };

    if parent.is_cow(test_addr) {
        if let Err(e) = parent.handle_cow_fault(test_addr) {
            return fail!("parent COW resolution failed: {:?}", e);
        }
    }

    let parent_phys = parent.virt_to_phys(test_addr);
    if let Some(virt) = parent_phys.to_virt_checked() {
        page_io::write_byte(virt.as_mut_ptr::<u8>(), 0, 0xBB);
    }

    if child.is_cow(test_addr) {
        if let Err(e) = child.handle_cow_fault(test_addr) {
            return fail!("child COW resolution failed: {:?}", e);
        }
    }

    let child_phys = child.virt_to_phys(test_addr);
    if let Some(virt) = child_phys.to_virt_checked() {
        page_io::write_byte(virt.as_mut_ptr::<u8>(), 0, 0xCC);
    }

    if let (Some(pv), Some(cv)) = (parent_phys.to_virt_checked(), child_phys.to_virt_checked()) {
        let parent_val = page_io::read_byte(pv.as_ptr::<u8>(), 0);
        let child_val = page_io::read_byte(cv.as_ptr::<u8>(), 0);

        assert_test!(
            parent_val != child_val,
            "parent and child share same data after COW"
        );
        assert_test!(parent_val == 0xBB, "parent data corrupted");
        assert_test!(child_val == 0xCC, "child data corrupted");
    }

    pass!()
}

static RACED_PARENT: AtomicU32 = AtomicU32::new(INVALID_PROCESS_ID);
static RACED_ADDR: AtomicU64 = AtomicU64::new(0);

fn unmap_the_raced_page() {
    if let Some(parent) =
        slopos_ostd::process::ProcessId::resolve(RACED_PARENT.load(Ordering::Relaxed))
    {
        let _ = crate::process_vm::process_vm_munmap(
            parent,
            RACED_ADDR.load(Ordering::Relaxed),
            PAGE_SIZE_4KB,
        );
    }
}

/// A sibling thread unmapping the page between the fork's snapshot and the
/// child's mapping must not free the frame the child is about to map.
pub fn test_cow_clone_survives_a_sibling_unmap() -> TestResult {
    use crate::process_vm::process_vm_alloc;

    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };
    let addr = process_vm_alloc(
        parent.process,
        PAGE_SIZE_4KB,
        PageFlags::WRITABLE.bits() as u32,
    );
    assert_test!(addr != 0, "process_vm_alloc failed");
    let Some(phys) = parent.map_test_page(addr, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };
    if let Some(virt) = phys.to_virt_checked() {
        page_io::fill_pattern(virt.as_mut_ptr::<u8>(), 0xA5, PAGE_SIZE_4KB as usize);
    }

    RACED_PARENT.store(parent.pid(), Ordering::Relaxed);
    RACED_ADDR.store(addr, Ordering::Relaxed);
    crate::process_vm::set_clone_window_hook(Some((parent.pid(), unmap_the_raced_page)));
    let child = parent.clone_cow();
    crate::process_vm::set_clone_window_hook(None);

    let Some(child) = child else {
        return fail!("the fork failed across a sibling's munmap");
    };
    assert_test!(
        parent.virt_to_phys(addr).is_null(),
        "the parent still maps the page it unmapped"
    );
    let byte = child
        .virt_to_phys(addr)
        .to_virt_checked()
        .map(|virt| page_io::read_byte(virt.as_ptr::<u8>(), 0));
    assert_test!(
        byte == Some(0xA5),
        "the child does not map the parent's page"
    );
    pass!()
}

pub fn test_cow_multiple_clones() -> TestResult {
    let Some(parent) = ProcessVmGuard::new() else {
        return fail!("create parent VM");
    };

    let mut children: [u32; 4] = [INVALID_PROCESS_ID; 4];
    let mut child_count = 0usize;

    for i in 0..4 {
        let child_pid = process_vm_clone_cow(parent.process);
        if child_pid == INVALID_PROCESS_ID {
            klog_info!("COW_TEST: Clone {} failed", i);
            break;
        }
        children[i] = child_pid;
        child_count += 1;
    }

    assert_test!(child_count >= 2, "couldn't create enough clones");

    for i in 0..child_count {
        crate::process_vm::destroy_process_vm(resolve_pid(children[i]));
    }

    pass!()
}

pub fn test_cow_no_collateral_damage() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr1: u64 = 0x7000;
    let addr2: u64 = 0x8000;

    let map_addr1 = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user_fresh(vs, VirtAddr::new(addr1), PageFlags::USER_RO.bits())
    });
    let Some(Ok(phys1)) = map_addr1 else {
        return fail!("map page 1");
    };
    let map_addr2 = process_vm_with_vm_space(vm.process, |vs| {
        ostd_map_4kb_user_fresh(vs, VirtAddr::new(addr2), PageFlags::USER_RO.bits())
    });
    let Some(Ok(phys2)) = map_addr2 else {
        return fail!("map page 2");
    };

    if let Some(v1) = phys1.to_virt_checked() {
        page_io::write_bytes(v1.as_mut_ptr::<u8>(), 0x11, PAGE_SIZE_4KB as usize);
    }
    if let Some(v2) = phys2.to_virt_checked() {
        page_io::write_bytes(v2.as_mut_ptr::<u8>(), 0x22, PAGE_SIZE_4KB as usize);
    }

    vm.mark_cow(addr1);
    vm.mark_cow(addr2);

    if let Err(e) = vm.handle_cow_fault(addr1) {
        return fail!("first page COW failed: {:?}", e);
    }

    let phys2_after = vm.virt_to_phys(addr2);
    assert_test!(phys2_after == phys2, "second page physical address changed");

    if let Some(v2) = phys2_after.to_virt_checked() {
        let val = page_io::read_byte(v2.as_ptr::<u8>(), 0);
        assert_test!(val == 0x22, "second page data corrupted");
    }

    pass!()
}

pub fn test_cow_handle_invalid_address() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped: u64 = 0xDEAD_0000;
    match vm.handle_cow_fault(unmapped) {
        Err(MmError::NotCowPage) | Err(MmError::InvalidAddress) => pass!(),
        Ok(_) => fail!("COW succeeded on unmapped address"),
        Err(e) => {
            klog_info!(
                "COW_TEST: Got error {:?} for unmapped address (acceptable)",
                e
            );
            pass!()
        }
    }
}

/// A sibling resolves the COW page while this CPU's fault is in flight, so
/// the handler arrives at a leaf that already permits the write. That is what
/// `threads_share_one_stream` used to die of after the fork before it.
pub fn test_cow_write_fault_on_an_already_resolved_page_is_not_fatal() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr: u64 = 0x9000;
    let Some(phys) = vm.map_test_page(addr, PageFlags::USER_RO.bits()) else {
        return fail!("map test page");
    };
    vm.mark_cow(addr);

    // The sibling's resolution: single-ref, so the leaf is upgraded in place.
    if let Err(e) = vm.handle_cow_fault(addr) {
        return fail!("the peer's COW resolution failed: {:?}", e);
    }
    assert_test!(!vm.is_cow(addr), "the page is still COW after resolution");

    let Some(handle) = crate::process_vm::process_vm_handle(vm.process) else {
        return fail!("no VM handle");
    };
    let packed = crate::process_vm::pack_process_vm_handle(handle);

    // 0x07: user write against a page the error code reports as present.
    let outcome = crate::page_fault::try_resolve_user_fault(addr, 0x07, packed, 1);
    assert_test!(
        outcome == crate::page_fault::FaultOutcome::Resolved,
        "a write fault on a leaf that already permits the write was not resolved"
    );
    assert_test!(
        vm.virt_to_phys(addr) == phys,
        "the late fault moved the page"
    );

    pass!()
}

/// The other side of that arm: a leaf that does not permit the write is a
/// protection violation, and staleness never makes one spurious.
pub fn test_write_fault_on_a_read_only_page_stays_fatal() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr: u64 = 0xA000;
    if vm.map_test_page(addr, PageFlags::USER_RO.bits()).is_none() {
        return fail!("map test page");
    }

    let Some(handle) = crate::process_vm::process_vm_handle(vm.process) else {
        return fail!("no VM handle");
    };
    let packed = crate::process_vm::pack_process_vm_handle(handle);

    let outcome = crate::page_fault::try_resolve_user_fault(addr, 0x07, packed, 1);
    assert_test!(
        outcome
            == crate::page_fault::FaultOutcome::Fatal(slopos_abi::task::TaskFaultReason::UserPage),
        "a write to a read-only page was not fatal"
    );

    pass!()
}

slopos_testing::stest!(name = test_cow_read_not_cow_fault, suite = cow_edge);
slopos_testing::stest!(
    name = test_cow_clone_survives_a_sibling_unmap,
    suite = cow_edge
);
slopos_testing::stest!(name = test_cow_not_present_not_cow, suite = cow_edge);
slopos_testing::stest!(
    name = test_cow_dispatch_absent_for_a_reaped_process,
    suite = cow_edge
);
slopos_testing::stest!(name = test_cow_handle_not_cow_page, suite = cow_edge);
slopos_testing::stest!(name = test_cow_single_ref_upgrade, suite = cow_edge);
slopos_testing::stest!(name = test_cow_multi_ref_copy, suite = cow_edge);
slopos_testing::stest!(name = test_cow_page_boundary, suite = cow_edge);
slopos_testing::stest!(name = test_cow_clone_modify_both, suite = cow_edge);
slopos_testing::stest!(name = test_cow_multiple_clones, suite = cow_edge);
slopos_testing::stest!(name = test_cow_no_collateral_damage, suite = cow_edge);
slopos_testing::stest!(name = test_cow_handle_invalid_address, suite = cow_edge);
slopos_testing::stest!(
    name = test_cow_write_fault_on_an_already_resolved_page_is_not_fatal,
    suite = cow_edge
);
slopos_testing::stest!(
    name = test_write_fault_on_a_read_only_page_stays_fatal,
    suite = cow_edge
);
