use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::demand::{can_satisfy_fault, is_demand_fault};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{process_vm_alloc, process_vm_get_region};
use crate::tests::test_fixtures::ProcessVmGuard;
use crate::vma_region::{Protection, RegionBacking, RegionPurpose, VmaRegion};

fn anon_region(prot: Protection) -> VmaRegion {
    VmaRegion::new(prot, RegionBacking::Anonymous, true, RegionPurpose::General)
}

fn kernel_region(prot: Protection) -> VmaRegion {
    let mut region = anon_region(prot);
    region.user = false;
    region
}

pub fn test_demand_fault_present_page() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.process, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let Some(_phys) = vm.map_test_page(addr, PageFlags::USER_RW.bits()) else {
        return fail!("map test page");
    };

    let error_code_present: u64 = 0x01;
    assert_test!(
        !is_demand_fault(error_code_present, vm.process, addr),
        "is_demand_fault returned true for present page"
    );

    pass!()
}

pub fn test_demand_fault_no_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let unmapped_addr: u64 = 0x7FFF_0000_0000;
    let error_code: u64 = 0x06;

    assert_test!(
        !is_demand_fault(error_code, vm.process, unmapped_addr),
        "is_demand_fault returned true for unmapped address"
    );

    pass!()
}

pub fn test_demand_fault_lazy_anon_vma() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = process_vm_alloc(vm.process, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    assert_test!(addr != 0, "process_vm_alloc failed");

    let region = process_vm_get_region(vm.process, addr);
    assert_test!(region.is_some(), "no VMA found for allocated address");
    let region = region.unwrap();
    assert_test!(region.is_demand_paged(), "allocated VMA is not LAZY");
    assert_test!(region.is_anonymous(), "allocated VMA is not Anonymous");

    let error_code: u64 = 0x04;
    assert_test!(
        is_demand_fault(error_code, vm.process, addr),
        "is_demand_fault returned false for valid LAZY VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_write_ro() -> TestResult {
    let ro = anon_region(Protection::RO);
    let error_code_write: u64 = 0x06;

    assert_test!(
        !can_satisfy_fault(error_code_write, &ro),
        "can_satisfy_fault allowed write to read-only VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_user_kernel() -> TestResult {
    let kernel = kernel_region(Protection::RW);
    let error_code_user: u64 = 0x04;

    assert_test!(
        !can_satisfy_fault(error_code_user, &kernel),
        "can_satisfy_fault allowed user access to kernel VMA"
    );

    pass!()
}

pub fn test_demand_permission_deny_exec() -> TestResult {
    let data = anon_region(Protection::RW);
    let error_code_ifetch: u64 = 0x14;

    assert_test!(
        !can_satisfy_fault(error_code_ifetch, &data),
        "can_satisfy_fault allowed exec on non-exec VMA"
    );

    pass!()
}

pub fn test_demand_permission_allow_read() -> TestResult {
    let readable = anon_region(Protection::RO);
    let error_code_read: u64 = 0x04;

    assert_test!(
        can_satisfy_fault(error_code_read, &readable),
        "can_satisfy_fault denied valid read"
    );

    pass!()
}

pub fn test_demand_permission_allow_write() -> TestResult {
    let writable = anon_region(Protection::RW);
    let error_code_write: u64 = 0x06;

    assert_test!(
        can_satisfy_fault(error_code_write, &writable),
        "can_satisfy_fault denied valid write"
    );

    pass!()
}

pub fn test_demand_dispatch_absent_for_a_reaped_process() -> TestResult {
    // `handle_demand_fault` is reached only through `process_vm_with_vm_space`,
    // so this drives the `None` arm `try_resolve_user_fault` depends on.
    let Some(vm) = crate::tests::test_fixtures::ProcessVmGuard::new() else {
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

/// A `PROT_NONE` mapping is the guard page slibc puts below every thread
/// stack, and the POSIX floor leans on it: std tells a stack overflow from an
/// ordinary `SIGSEGV` only if touching the guard kills the task. Driven
/// through `try_resolve_user_fault` rather than `can_satisfy_fault` because
/// the whole hazard was that every *other* arm of the fault path says yes.
pub fn test_demand_read_of_a_prot_none_mapping_is_fatal() -> TestResult {
    use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_NONE};

    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };

    let addr = crate::process_vm::process_vm_mmap(
        vm.process,
        0,
        PAGE_SIZE_4KB,
        PROT_NONE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    );
    assert_test!(addr != 0, "PROT_NONE mmap failed");

    let Some(handle) = crate::process_vm::process_vm_handle(vm.process) else {
        return fail!("no VM handle");
    };
    let packed = crate::process_vm::pack_process_vm_handle(handle);

    // 0x04: user read against an absent page — the one shape whose access
    // bits name nothing `can_satisfy_fault` used to look at.
    let outcome = crate::page_fault::try_resolve_user_fault(addr, 0x04, packed, 1);
    assert_test!(
        outcome
            == crate::page_fault::FaultOutcome::Fatal(slopos_abi::task::TaskFaultReason::UserPage),
        "a read of a PROT_NONE page was not fatal"
    );
    assert_test!(
        vm.virt_to_phys(addr).is_null(),
        "a PROT_NONE page was backed by a frame"
    );

    pass!()
}

slopos_testing::stest!(name = test_demand_fault_present_page, suite = demand);
slopos_testing::stest!(name = test_demand_fault_no_vma, suite = demand);
slopos_testing::stest!(name = test_demand_fault_lazy_anon_vma, suite = demand);
slopos_testing::stest!(name = test_demand_permission_deny_write_ro, suite = demand);
slopos_testing::stest!(
    name = test_demand_permission_deny_user_kernel,
    suite = demand
);
slopos_testing::stest!(name = test_demand_permission_deny_exec, suite = demand);
slopos_testing::stest!(name = test_demand_permission_allow_read, suite = demand);
slopos_testing::stest!(name = test_demand_permission_allow_write, suite = demand);
slopos_testing::stest!(
    name = test_demand_dispatch_absent_for_a_reaped_process,
    suite = demand
);
slopos_testing::stest!(
    name = test_demand_read_of_a_prot_none_mapping_is_fatal,
    suite = demand
);
