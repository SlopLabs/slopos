//! The task side of the OOM killer: who is init, who is already dying, and
//! how a victim dies. Which process is the victim is `slopos_mm::oom`'s call.

use core::ops::ControlFlow;

use slopos_abi::signal::SIGKILL;
use slopos_abi::task::INVALID_TASK_ID;
use slopos_mm::oom::{Killed, OomOps, Standing};
use slopos_ostd::process::Process;
use slopos_sched::task::{
    TaskRef, task_find_by_id, task_for_each_enumerable, task_group_signal,
    task_try_for_each_enumerable,
};

pub struct TaskOomOps;

pub static OOM_OPS: TaskOomOps = TaskOomOps;

fn in_process(task: &TaskRef, process: &Process) -> bool {
    task.process_handle_raw() == process.handle_raw()
}

/// The pid `getpid` reports for `task`.
fn group_id(task: &TaskRef) -> u32 {
    if task.tgid == INVALID_TASK_ID {
        task.task_id
    } else {
        task.tgid
    }
}

/// `process`'s standing when `init` is init's task id: exempt if init runs
/// in it, dying once every task it has left is killed.
pub fn standing_of(process: &Process, init: u32) -> Standing {
    if init != INVALID_TASK_ID && task_find_by_id(init).is_some_and(|t| in_process(&t, process)) {
        return Standing::Exempt;
    }
    let mut live = false;
    task_try_for_each_enumerable(|task| {
        if in_process(task, process) && !task.is_killed() {
            live = true;
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    });
    if live {
        Standing::Killable
    } else {
        Standing::Dying
    }
}

impl OomOps for TaskOomOps {
    fn standing(&self, process: &Process) -> Standing {
        standing_of(process, crate::exec::init_task_id())
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        let mut killed: Option<Killed> = None;
        task_for_each_enumerable(|task| {
            if !in_process(task, process) || task.is_killed() {
                return;
            }
            if killed.is_none() {
                killed = Some(Killed {
                    pid: group_id(task),
                    name: task.name,
                });
            }
            // Every thread group sharing the address space, as `SIGKILL` to
            // each would: a group's first kill marks all its members, so the
            // walk signals each group once.
            task_group_signal(task.task_id, SIGKILL);
        });
        killed
    }
}
