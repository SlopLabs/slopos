//! A task's identity, so a selection names a task rather than a row of a
//! table that re-sorts on every refresh.

use slopos_abi::syscall::types::UserTaskEntry;

/// Ids recycle; the creation time, which the kernel never rewrites, keeps the
/// pair distinct across a recycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskKey {
    pub pid: u32,
    pub started_ms: u64,
}

impl TaskKey {
    pub fn of(task: &UserTaskEntry) -> Self {
        Self {
            pid: task.task_id,
            started_ms: task.creation_time_ms,
        }
    }

    pub fn matches(&self, task: &UserTaskEntry) -> bool {
        *self == Self::of(task)
    }
}

/// `order` holds indices into `tasks`, in display order.
pub fn row_of(key: TaskKey, tasks: &[UserTaskEntry], order: &[usize]) -> Option<usize> {
    order
        .iter()
        .position(|&idx| tasks.get(idx).is_some_and(|t| key.matches(t)))
}

pub fn key_at_row(tasks: &[UserTaskEntry], order: &[usize], row: usize) -> Option<TaskKey> {
    let idx = *order.get(row)?;
    tasks.get(idx).map(TaskKey::of)
}

pub fn index_of(key: TaskKey, tasks: &[UserTaskEntry]) -> Option<usize> {
    tasks.iter().position(|t| key.matches(t))
}
