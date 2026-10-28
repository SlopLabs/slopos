use super::task_table::task_find_by_id;
use super::{BlockReason, Task, TaskStatus};

/// `false` if the task is gone, not yet published, or refuses the transition.
pub fn task_set_state(task_id: u32, new_status: TaskStatus) -> bool {
    let Some(task_ref) = task_find_by_id(task_id) else {
        return false;
    };
    task_ref.status() != TaskStatus::Invalid && task_ref.try_transition_to(new_status)
}

fn apply_state_transition(task_ref: &Task, new_status: TaskStatus, reason: BlockReason) -> bool {
    match new_status {
        TaskStatus::Ready => task_ref.mark_ready(),
        TaskStatus::Running => task_ref.mark_running(),
        TaskStatus::Blocked => task_ref.block(reason),
        TaskStatus::Terminated => task_ref.terminate(),
        TaskStatus::Zombie => task_ref.mark_zombie(),
        TaskStatus::Stopped => task_ref.mark_stopped(),
        TaskStatus::Invalid => false,
    }
}

pub fn task_set_state_with_reason(
    task_id: u32,
    new_status: TaskStatus,
    reason: BlockReason,
) -> bool {
    let Some(task_ref) = task_find_by_id(task_id) else {
        return false;
    };
    task_ref.status() != TaskStatus::Invalid
        && apply_state_transition(&task_ref, new_status, reason)
}

/// Atomically transition a task the caller already holds from `expected` to
/// `target`, reporting whether this caller won. A caller holding the task takes
/// this over the id-keyed [`task_try_transition_from`] and pays neither the
/// cli-spinlock nor the registry scan.
pub fn task_transition_from(task: &Task, expected: TaskStatus, target: TaskStatus) -> bool {
    task.status() != TaskStatus::Invalid && task.try_transition_from(expected, target)
}

/// Atomically transition from `expected` to `target`. `false` if the task is
/// gone, the state does not match `expected`, or the transition is invalid.
pub fn task_try_transition_from(task_id: u32, expected: TaskStatus, target: TaskStatus) -> bool {
    task_find_by_id(task_id).is_some_and(|task| task_transition_from(&task, expected, target))
}

/// Atomically transition from `expected` to `new_status`, setting block reason.
pub fn task_set_state_from_with_reason(
    task_id: u32,
    expected: TaskStatus,
    new_status: TaskStatus,
    reason: BlockReason,
) -> bool {
    let Some(task_ref) = task_find_by_id(task_id) else {
        return false;
    };
    if task_ref.status() == TaskStatus::Invalid {
        return false;
    }
    match new_status {
        TaskStatus::Blocked => task_ref.block_from(expected, reason),
        _ => task_ref.try_transition_from(expected, new_status),
    }
}
