use core::marker::PhantomData;

use slopos_ostd::klog_debug;
use slopos_ostd::sync::kernel_io_task::{
    KernelIoTaskIds, arm_kernel_io_hold, disarm_kernel_io_hold, kernel_io_hold_armed,
    kernel_io_hold_covers, kernel_io_task_ids, refresh_kernel_io_hold,
};
use slopos_ostd::task::SchedPlacement;

use super::{KernelIoFreeze, task_find_by_id};
use crate::per_cpu::ApPauseToken;
use crate::task_struct::Task;

#[inline]
pub(crate) fn kernel_io_hold_claim(task: &Task, from: SchedPlacement) -> bool {
    kernel_io_hold_covers(task.task_id)
        && task.sched_placement_compare_exchange(from, SchedPlacement::Held)
}

pub fn kernel_io_dispatchable_count() -> usize {
    // The whole registry, not the arm-time snapshot, or a later registration reads
    // as quiesced. Read off-lock: the stop and task registries share a lock level.
    let armed = kernel_io_hold_armed();
    let mut count = 0usize;
    for id in kernel_io_task_ids().iter() {
        let Some(task) = task_find_by_id(id) else {
            continue;
        };
        // `Waking` reserves and links nothing; a covered publisher is claimed at the gate.
        let owned = match task.sched_placement() {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating => true,
            SchedPlacement::Waking => !armed || !kernel_io_hold_covers(id),
            _ => false,
        };
        if owned {
            count += 1;
        }
    }
    count
}

#[must_use = "dropping the token publishes the held threads again"]
pub struct KernelIoHold {
    /// !Send: the release must run on the CPU that armed the hold.
    _not_send: PhantomData<*mut ()>,
}

/// An AP that dequeued before the pause hands the task back on resume; one sweep is not enough.
const HOLD_SETTLE_SPINS: u32 = 4_096;

pub fn hold_kernel_io_all(_freeze: &KernelIoFreeze, paused: &ApPauseToken) -> KernelIoHold {
    // Armed before the sweep, or a publisher slips in between the two.
    arm_kernel_io_hold();
    let mut swept = 0usize;
    let mut settled = false;
    for _ in 0..HOLD_SETTLE_SPINS {
        refresh_kernel_io_hold();
        swept += crate::per_cpu::hold_kernel_io_off_all_runqueues(paused);
        if kernel_io_dispatchable_count() == 0 {
            settled = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !settled {
        slopos_ostd::klog_info!(
            "SCHED: KERNEL_IO_HOLD_UNSETTLED left={} spins={}",
            kernel_io_dispatchable_count(),
            HOLD_SETTLE_SPINS
        );
    }
    if swept != 0 {
        klog_debug!(
            "SCHED: kernel-io hold took {} thread(s) off a run queue",
            swept
        );
    }
    KernelIoHold {
        _not_send: PhantomData,
    }
}

pub fn republish_held_kernel_io(held: &KernelIoTaskIds) {
    for id in held.iter() {
        let Some(task) = task_find_by_id(id) else {
            continue;
        };
        let body: &Task = &task;
        if body.sched_placement() != SchedPlacement::Held {
            continue;
        }
        if !body.is_ready() {
            let _ =
                body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::None);
            continue;
        }
        if !body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::Waking) {
            continue;
        }
        if !crate::scheduler::schedule_task(&task) {
            klog_debug!(
                "SCHED: kernel-io hold could not republish task {}",
                body.task_id
            );
        }
    }
}

impl Drop for KernelIoHold {
    fn drop(&mut self) {
        // Disarmed first, or the republish below is claimed straight back.
        if let Some(held) = disarm_kernel_io_hold() {
            republish_held_kernel_io(&held);
        }
    }
}
