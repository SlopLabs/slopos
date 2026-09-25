//! Holding a second `KArc<VmSpace>` reproduces the contention
//! single-threaded — no SMP, no timing — except where the thing under test is
//! a wait, which needs a task to do it and a clock to outlast.

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use slopos_abi::addr::PhysAddr;
use slopos_abi::task::TaskPriority;
use slopos_kernel_services::clock::uptime_ms;
use slopos_ostd::mm::frame::{AnonymousMeta, Frame, Paddr};

use crate::error::MmError;
use crate::page_fault::{
    FaultOutcome, FileIo, RETRY_WARN_MS, RetryEpisode, note_retry, populate_user_range_for_read,
    try_resolve_user_fault,
};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};
use crate::process_vm::{
    pack_process_vm_handle, process_vm_alloc, process_vm_get_vm_space, process_vm_handle,
};
use crate::tests::test_fixtures::ProcessVmGuard;

const WRITE_USER_ABSENT: u64 = 0x06;

fn lazy_anon_page(vm: &ProcessVmGuard) -> Option<u64> {
    let addr = process_vm_alloc(vm.process, PAGE_SIZE_4KB, PageFlags::WRITABLE.bits() as u32);
    if addr == 0 { None } else { Some(addr) }
}

fn cow_page(vm: &ProcessVmGuard) -> Option<PhysAddr> {
    let phys = vm.map_test_page(0x5000, PageFlags::USER_RO.bits())?;
    vm.mark_cow(0x5000);
    Some(phys)
}

pub fn test_demand_fault_retries_while_a_reader_holds_the_space() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };

    let result = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    drop(reader);

    match result {
        Err(MmError::Retry) => pass!(),
        other => fail!("expected Retry, got {:?}", other),
    }
}

pub fn test_demand_fault_retry_maps_nothing() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };

    let result = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    let mapped = vm.virt_to_phys(addr);
    drop(reader);

    assert_test!(result == Err(MmError::Retry), "expected Retry");
    assert_test!(mapped.is_null(), "a retried demand fault left a mapping");
    pass!()
}

pub fn test_demand_fault_resolves_after_the_reader_drops() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let blocked = vm.handle_demand_fault(addr, WRITE_USER_ABSENT);
    drop(reader);
    assert_test!(blocked == Err(MmError::Retry), "expected Retry while held");

    if let Err(e) = vm.handle_demand_fault(addr, WRITE_USER_ABSENT) {
        return fail!("demand fault failed after the reader dropped: {:?}", e);
    }
    assert_test!(
        !vm.virt_to_phys(addr).is_null(),
        "demand fault reported success without a mapping"
    );
    pass!()
}

pub fn test_cow_fault_retries_while_a_reader_holds_the_space() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(phys) = cow_page(&vm) else {
        return fail!("map and mark a COW page");
    };
    // A second reference to the paddr is what selects the copying arm.
    let shared = match Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64())) {
        Ok(frame) => frame,
        Err(e) => return fail!("take a second reference to the COW frame: {:?}", e),
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        drop(shared);
        return fail!("clone the address space");
    };
    let result = vm.handle_cow_fault(0x5000);
    drop(reader);
    drop(shared);

    match result {
        Err(MmError::Retry) => pass!(),
        other => fail!("expected Retry, got {:?}", other),
    }
}

pub fn test_cow_retry_leaves_the_page_mapped_and_cow() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(phys) = cow_page(&vm) else {
        return fail!("map and mark a COW page");
    };
    let shared = match Frame::<AnonymousMeta>::from_in_use(Paddr::new(phys.as_u64())) {
        Ok(frame) => frame,
        Err(e) => return fail!("take a second reference to the COW frame: {:?}", e),
    };

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        drop(shared);
        return fail!("clone the address space");
    };
    let result = vm.handle_cow_fault(0x5000);
    let after = vm.virt_to_phys(0x5000);
    let still_cow = vm.is_cow(0x5000);
    drop(reader);
    drop(shared);

    assert_test!(result == Err(MmError::Retry), "expected Retry");
    assert_test!(after == phys, "a retried COW fault replaced the leaf");
    assert_test!(still_cow, "a retried COW fault cleared the COW bit");
    pass!()
}

pub fn test_user_fault_dispatch_reports_retry_not_fatal() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(handle) = process_vm_handle(vm.process) else {
        return fail!("resolve the process-VM handle");
    };
    let packed = pack_process_vm_handle(handle);

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let outcome = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    let mapped = vm.virt_to_phys(addr);
    drop(reader);

    assert_test!(
        outcome == FaultOutcome::Retry,
        "a contended address space must not be a fatal user fault"
    );
    assert_test!(mapped.is_null(), "the retried fault left a mapping");
    pass!()
}

pub fn test_user_fault_dispatch_resolves_after_the_reader_drops() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(handle) = process_vm_handle(vm.process) else {
        return fail!("resolve the process-VM handle");
    };
    let packed = pack_process_vm_handle(handle);

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let blocked = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    drop(reader);
    assert_test!(blocked == FaultOutcome::Retry, "expected Retry while held");

    let outcome = try_resolve_user_fault(addr, WRITE_USER_ABSENT, packed, vm.pid());
    assert_test!(
        outcome == FaultOutcome::Resolved,
        "the fault did not resolve once the reader dropped"
    );
    pass!()
}

/// `map_user_range` is the one remaining eager multi-page mapper, and an `Err`
/// from it must leave nothing mapped, or the rollback is a leak.
pub fn test_map_user_range_leaves_nothing_mapped_on_would_block() -> TestResult {
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let start = crate::memory_layout_defs::PROCESS_MMAP_START_VA;
    let end = start + 2 * PAGE_SIZE_4KB;
    let flags = crate::paging_defs::PageFlags::USER_RW.bits();

    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };
    let mapped = crate::process_vm::process_vm_map_range_for_test(vm.process, start, end, flags);
    let first = vm.virt_to_phys(start);
    let second = vm.virt_to_phys(start + PAGE_SIZE_4KB);
    drop(reader);

    assert_test!(
        mapped.is_err(),
        "the range mapped while a reader held the address space"
    );
    assert_test!(
        first.is_null() && second.is_null(),
        "a failed multi-page map left part of the range mapped"
    );
    pass!()
}

const POPULATE_PENDING: u8 = 0;
const POPULATE_STARTED: u8 = 1;
const POPULATE_DONE: u8 = 2;
const POPULATE_REFUSED: u8 = 3;

static POPULATE_TARGET: AtomicU64 = AtomicU64::new(0);
static POPULATE_HANDLE: AtomicU64 = AtomicU64::new(0);
static POPULATE_OUTCOME: AtomicU8 = AtomicU8::new(POPULATE_PENDING);

/// A kernel thread, since only a task can nap between attempts.
fn populate_from_a_task() {
    POPULATE_OUTCOME.store(POPULATE_STARTED, Ordering::Release);
    let populated = populate_user_range_for_read(
        POPULATE_HANDLE.load(Ordering::Acquire),
        POPULATE_TARGET.load(Ordering::Acquire),
        PAGE_SIZE_4KB,
        slopos_arch::pcr::current_task_id(),
        FileIo::Read,
    );
    let outcome = if populated {
        POPULATE_DONE
    } else {
        POPULATE_REFUSED
    };
    POPULATE_OUTCOME.store(outcome, Ordering::Release);
}

pub fn test_blocking_populate_outlasts_a_long_reader() -> TestResult {
    const HOLD_MS: u64 = 2000;
    let Some(vm) = ProcessVmGuard::new() else {
        return fail!("create VM");
    };
    let Some(addr) = lazy_anon_page(&vm) else {
        return fail!("process_vm_alloc failed");
    };
    let Some(handle) = process_vm_handle(vm.process) else {
        return fail!("no handle for the process");
    };
    let Some(reader) = process_vm_get_vm_space(vm.process) else {
        return fail!("clone the address space");
    };

    POPULATE_TARGET.store(addr, Ordering::Release);
    POPULATE_HANDLE.store(pack_process_vm_handle(handle), Ordering::Release);
    POPULATE_OUTCOME.store(POPULATE_PENDING, Ordering::Release);
    if slopos_ostd::task::spawn("populate-nap", populate_from_a_task, TaskPriority::Normal).is_err()
    {
        drop(reader);
        return fail!("could not spawn the populating thread");
    }
    let started_by = uptime_ms() + 5_000;
    while POPULATE_OUTCOME.load(Ordering::Acquire) == POPULATE_PENDING && uptime_ms() < started_by {
        core::hint::spin_loop();
    }
    let released_at = uptime_ms() + HOLD_MS;
    while uptime_ms() < released_at {
        core::hint::spin_loop();
    }
    let early = POPULATE_OUTCOME.load(Ordering::Acquire);
    drop(reader);
    let deadline = uptime_ms() + 10_000;
    while POPULATE_OUTCOME.load(Ordering::Acquire) == POPULATE_STARTED && uptime_ms() < deadline {
        core::hint::spin_loop();
    }

    assert_test!(
        early != POPULATE_PENDING,
        "the populating thread never started"
    );
    assert_test!(
        early == POPULATE_STARTED,
        "the populate settled while the reader still held the space"
    );
    assert_test!(
        POPULATE_OUTCOME.load(Ordering::Acquire) == POPULATE_DONE,
        "the populate gave the page up rather than wait out the reader"
    );
    assert_test!(
        !vm.virt_to_phys(addr).is_null(),
        "the populate reported success without a mapping"
    );
    pass!()
}

pub fn test_retry_episode_warns_once_after_the_budget() -> TestResult {
    let mut ep = RetryEpisode::IDLE;
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, 0),
        "the first retry of an episode warned"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS - 1),
        "warned before the budget elapsed"
    );
    assert_test!(
        note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS),
        "did not warn once the budget elapsed"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS + 1),
        "warned twice in one episode"
    );
    pass!()
}

/// A restart is only observable by warning again, hence the second call in
/// each arm.
pub fn test_retry_episode_keys_on_the_task_not_the_address() -> TestResult {
    let mut ep = RetryEpisode::IDLE;
    assert_test!(
        !note_retry(&mut ep, 7, 0x1000, 0),
        "the first retry of an episode warned"
    );
    assert_test!(
        note_retry(&mut ep, 7, 0x1000, RETRY_WARN_MS),
        "did not warn once the budget elapsed"
    );

    assert_test!(
        !note_retry(&mut ep, 7, 0x2000, 200),
        "a new fault address warned on its first retry"
    );
    assert_test!(
        !note_retry(&mut ep, 7, 0x2000, 200 + RETRY_WARN_MS),
        "a new fault address restarted the episode: one task warned twice"
    );

    assert_test!(
        !note_retry(&mut ep, 8, 0x2000, 300),
        "a new task warned on its first retry"
    );
    assert_test!(
        !note_retry(&mut ep, 8, 0x2000, 300 + RETRY_WARN_MS - 1),
        "the new task's budget was measured from the previous episode"
    );
    assert_test!(
        note_retry(&mut ep, 8, 0x2000, 300 + RETRY_WARN_MS),
        "a new task did not restart the episode: it never warned"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_demand_fault_retries_while_a_reader_holds_the_space,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_demand_fault_retry_maps_nothing,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_demand_fault_resolves_after_the_reader_drops,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_blocking_populate_outlasts_a_long_reader,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_cow_fault_retries_while_a_reader_holds_the_space,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_cow_retry_leaves_the_page_mapped_and_cow,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_user_fault_dispatch_reports_retry_not_fatal,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_user_fault_dispatch_resolves_after_the_reader_drops,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_map_user_range_leaves_nothing_mapped_on_would_block,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_retry_episode_warns_once_after_the_budget,
    suite = vm_contention
);
slopos_testing::stest!(
    name = test_retry_episode_keys_on_the_task_not_the_address,
    suite = vm_contention
);
