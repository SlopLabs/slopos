//! The task side of the OOM killer: who is init, who is already dying, who is
//! shielded from the writer, and how a victim dies. Which process is the
//! victim is `slopos_mm::oom`'s call.
//!
//! Every walk here is heapless: the killer runs when the heap is dry, and a
//! walk that could not pay for its snapshot would see no task at all.

use core::ops::ControlFlow;

use slopos_abi::signal::SigInfo;
use slopos_abi::task::INVALID_TASK_ID;
use slopos_mm::oom::{Killed, OomOps, Standing};
use slopos_ostd::process::Process;
use slopos_sched::scheduler::{current_task_flags, current_task_id};
use slopos_sched::task::{
    TaskRef, task_find_by_id, task_for_each_enumerable_heapless, task_sigkill_member,
    task_try_for_each_enumerable_heapless,
};

use crate::syscall::signal::signal_dominates;

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

/// `process`'s standing with the writer `writer_id` holding `writer_flags`
/// when `init` is init's task id: exempt if init runs in it, dying once every
/// task it has left is killed, shielded — never taken for this writer — if a
/// live one holds privileged flags the writer lacks, the relation `kill`
/// refuses on. Nothing is shielded from init's own write: init cannot be
/// impersonated, and a write it cannot finish takes the machine down.
pub fn standing_of(process: &Process, init: u32, writer_id: u32, writer_flags: u16) -> Standing {
    let has_init = init != INVALID_TASK_ID;
    if has_init && task_find_by_id(init).is_some_and(|t| in_process(&t, process)) {
        return Standing::Exempt;
    }
    let writer_is_init = has_init && writer_id == init;
    let mut live = false;
    let mut shielded = false;
    task_try_for_each_enumerable_heapless(|task| {
        if in_process(task, process) && !task.is_killed() {
            live = true;
            shielded |= !writer_is_init && !signal_dominates(writer_flags, task.flags);
            if shielded {
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    });
    match (live, shielded) {
        (false, _) => Standing::Dying,
        (true, true) => Standing::Shielded,
        (true, false) => Standing::Killable,
    }
}

impl OomOps for TaskOomOps {
    fn standing(&self, process: &Process) -> Standing {
        standing_of(
            process,
            crate::exec::init_task_id(),
            current_task_id(),
            current_task_flags(),
        )
    }

    fn kill(&self, process: &Process) -> Option<Killed> {
        let mut killed: Option<Killed> = None;
        // Thread by thread, not a group signal per thread group: the group
        // walk snapshots the registry on the heap.
        task_for_each_enumerable_heapless(|task| {
            if !in_process(task, process) || task.is_killed() {
                return;
            }
            if killed.is_none() {
                killed = Some(Killed {
                    pid: group_id(task),
                    name: task.name.get(),
                });
            }
            task_sigkill_member(task, SigInfo::KERNEL);
        });
        killed
    }
}
