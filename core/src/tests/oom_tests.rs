//! The task side of the OOM killer: init is never a victim, a process whose
//! tasks are all killed is already dying, and a kill is the kill flag.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::syscall::{MAP_ANONYMOUS, MAP_PRIVATE, PROT_READ, PROT_WRITE};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::oom::{
    Killed, OomOps, OomTrigger, Standing, oom_decide_for_test, oom_forget_victim_for_test,
};
use slopos_mm::page_fault::{FaultOutcome, try_resolve_user_fault};
use slopos_mm::paging_defs::PAGE_SIZE_4KB;
use slopos_mm::process_vm::{pack_process_vm_handle, process_vm_handle, process_vm_mmap};
use slopos_ostd::process::{Process, ProcessId};
use slopos_sched::task::{
    fail_task_snapshots_for_test, task_create, task_find_by_id, task_terminate,
};
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;
use slopos_testing::{assert_test, fail, pass};

use crate::oom::{OOM_OPS, standing_of};

fn create_user_task() -> u32 {
    task_create(
        b"OomVictim\0".as_ptr() as *const c_char,
        slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64),
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    )
}

pub fn test_oom_spares_init_and_passes_over_the_dying() -> TestResult {
    let _scope = KernelTestScope::new();

    let task_id = create_user_task();
    if task_id == INVALID_TASK_ID {
        return fail!("could not create a user task");
    }
    let Some(task) = task_find_by_id(task_id) else {
        task_terminate(task_id);
        return fail!("the new task is not findable");
    };
    let Some(process) = task.process() else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task has no process");
    };

    let plain = standing_of(&process, INVALID_TASK_ID);
    let as_init = standing_of(&process, task_id);
    let killed = OOM_OPS.kill(&process);
    let marked = task.is_killed();
    let after_kill = standing_of(&process, INVALID_TASK_ID);
    let second = OOM_OPS.kill(&process);

    drop(task);
    task_terminate(task_id);

    assert_test!(
        plain == Standing::Killable,
        "a live process stood {:?}",
        plain
    );
    assert_test!(as_init == Standing::Exempt, "init stood {:?}", as_init);
    assert_test!(
        killed.as_ref().is_some_and(|k| k.pid == task_id) && marked,
        "the kill reached pid {:?}, and marked the task: {}",
        killed.as_ref().map(|k| k.pid),
        marked
    );
    assert_test!(
        after_kill == Standing::Dying,
        "a process whose tasks are all killed stood {:?}",
        after_kill
    );
    assert_test!(
        second.is_none(),
        "a second kill of a dying process claimed another victim"
    );
    pass!()
}

slopos_testing::stest!(
    name = test_oom_spares_init_and_passes_over_the_dying,
    suite = oom_killer
);

/// The real task side, confined to one process so nothing else bound at the
/// time can be chosen.
struct Only(u32);

impl OomOps for Only {
    fn standing(&self, process: &Process) -> Standing {
        if process.id() == self.0 {
            standing_of(process, INVALID_TASK_ID)
        } else {
            Standing::Exempt
        }
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        (process.id() == self.0)
            .then(|| OOM_OPS.kill(process))
            .flatten()
    }
}

/// Map `pages` fresh pages into `process` and write every one.
fn touch_fresh_pages(process: ProcessId, task_id: u32, pages: u64) -> bool {
    let addr = process_vm_mmap(
        process,
        0,
        pages * PAGE_SIZE_4KB,
        PROT_READ | PROT_WRITE,
        MAP_ANONYMOUS | MAP_PRIVATE,
        -1,
        0,
    );
    let Some(handle) = process_vm_handle(process) else {
        return false;
    };
    let packed = pack_process_vm_handle(handle);
    addr != 0
        && (0..pages).all(|page| {
            // 0x06: a user write to an absent page.
            try_resolve_user_fault(addr + page * PAGE_SIZE_4KB, 0x06, packed, task_id)
                == FaultOutcome::Resolved
        })
}

/// The killer runs when the heap is dry, so neither its choice nor its kill
/// may rest on a task-registry snapshot the heap has to pay for.
pub fn test_oom_kills_the_hog_without_a_task_snapshot() -> TestResult {
    let _scope = KernelTestScope::new();

    let task_id = create_user_task();
    if task_id == INVALID_TASK_ID {
        return fail!("could not create a user task");
    }
    let Some(task) = task_find_by_id(task_id) else {
        task_terminate(task_id);
        return fail!("the new task is not findable");
    };
    let Some(process) = task.process() else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task has no process");
    };
    let Some(designator) = ProcessId::of(&process) else {
        drop(task);
        task_terminate(task_id);
        return fail!("the user task's process is not live");
    };
    let hog_vm = process_vm_handle(designator);
    let touched = touch_fresh_pages(designator, task_id, 8);

    oom_forget_victim_for_test();
    fail_task_snapshots_for_test(true);
    let awaited = oom_decide_for_test(&Only(process.id()), OomTrigger::Frames);
    fail_task_snapshots_for_test(false);
    oom_forget_victim_for_test();
    let marked = task.is_killed();

    drop(task);
    task_terminate(task_id);

    assert_test!(
        touched && hog_vm.is_some(),
        "could not make the process resident"
    );
    assert_test!(
        awaited.is_some() && awaited == hog_vm,
        "the killer chose no victim with the hog resident"
    );
    assert_test!(marked, "the victim was chosen but its task never killed");
    pass!()
}

slopos_testing::stest!(
    name = test_oom_kills_the_hog_without_a_task_snapshot,
    suite = oom_killer
);
