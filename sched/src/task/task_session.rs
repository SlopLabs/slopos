use core::sync::atomic::Ordering;

use slopos_abi::signal::{CLD_EXITED, CLD_KILLED, SIGCHLD, SigInfo};
use slopos_abi::syscall::TtyIndex;
use slopos_abi::task::TaskExitReason;
use slopos_ostd::task::{ProcessGroup, Session};
use slopos_ostd::{KArc, KWeak};

use super::task_ops::task_wake_all_waiters;
use super::task_table::{task_find_by_id, task_for_each_active, with_task_manager};
use super::{INVALID_TASK_ID, Task};

/// Resolve a live weak handle to the process group `pgid` names. `None` when no
/// live member carries a group object for `pgid`. Runs under the task-manager
/// lock, which is also what slot recycle drops the member handle under.
pub fn pgrp_handle_for_pgid(pgid: u32) -> Option<KWeak<ProcessGroup>> {
    if pgid == 0 {
        return None;
    }
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.pgid() == pgid {
                if let Some(pg) = task.process_group.load() {
                    return Some(KArc::downgrade(&pg));
                }
            }
        }
        None
    })
}

/// Resolve a live weak handle to the session `sid` names. `None` when no live
/// member carries a group object for `sid`.
pub fn session_handle_for_sid(sid: u32) -> Option<KWeak<Session>> {
    if sid == 0 {
        return None;
    }
    with_task_manager(|mgr| {
        for task in mgr.iter_tasks() {
            if task.sid() == sid {
                if let Some(pg) = task.process_group.load() {
                    return Some(KArc::downgrade(pg.session()));
                }
            }
        }
        None
    })
}

pub fn task_clear_controlling_tty_for_session(session_id: u32, tty: TtyIndex) -> usize {
    if session_id == 0 {
        return 0;
    }

    let mut cleared = 0usize;
    task_for_each_active(|task| {
        if task.sid() == session_id && task.clear_controlling_tty_if(tty) {
            cleared = cleared.saturating_add(1);
        }
    });
    cleared
}

pub(super) fn release_task_dependents(completed_task_id: u32) {
    let Some(task) = task_find_by_id(completed_task_id) else {
        return;
    };
    // The `waiters` SpinLock taken here and the waiter's `is_set` check inside
    // `wait_event` are the barrier pair for the Release `try_set` published
    // just before this call.
    task_wake_all_waiters(&task);
}

pub(super) fn notify_parent_of_child_exit(task: &Task) {
    let task_id = task.task_id;
    let tgid = task.tgid;
    let parent_task_id = task.parent_task_id();

    if parent_task_id == INVALID_TASK_ID || parent_task_id == task_id {
        return;
    }

    if tgid != task_id {
        return;
    }

    let _ = super::task_lifecycle::task_group_signal_info(
        parent_task_id,
        SIGCHLD,
        child_exit_info(task),
    );
    // Published unconditionally rather than only when a waiter exists: the
    // waiter registers before it scans, so a publish that races registration
    // costs a re-scan rather than a lost wakeup.
    slopos_ostd::sync::BUS.publish(slopos_ostd::task::ops::any_child_exit_event(parent_task_id));
}

/// The `SIGCHLD` record for `task`'s exit, as `waitpid` reports it; never
/// `CLD_DUMPED`, as there are no core dumps.
fn child_exit_info(task: &Task) -> SigInfo {
    let reason = TaskExitReason::from_u16(task.exit_reason.load(Ordering::Acquire));
    let signal = task.exit_signal();
    let (code, status) = if matches!(
        reason,
        TaskExitReason::Signalled | TaskExitReason::UserFault
    ) && signal != 0
    {
        (CLD_KILLED, signal as u32)
    } else {
        (CLD_EXITED, task.exit_code.load(Ordering::Acquire) & 0xff)
    };
    SigInfo::sent(code, task.task_id, 0, status as u64)
}
