//! The task side of the OOM killer: init is never a victim, a process whose
//! tasks are all killed is already dying, and a kill is the kill flag.

use core::ffi::c_char;
use core::ptr;

use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_USER_MODE};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_mm::oom::{OomOps, Standing};
use slopos_sched::task::{task_create, task_find_by_id, task_terminate};
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
