use slopos_abi::syscall::types::UserTaskEntry;

use super::monitor::Process;

/// Ids recycle; the creation time, which the kernel never rewrites, keeps the
/// pair distinct across a recycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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
}

pub fn row_of(key: TaskKey, processes: &[Process], order: &[usize]) -> Option<usize> {
    order
        .iter()
        .position(|&idx| processes.get(idx).is_some_and(|p| p.key == key))
}

pub fn key_at_row(processes: &[Process], order: &[usize], row: usize) -> Option<TaskKey> {
    let idx = *order.get(row)?;
    processes.get(idx).map(|p| p.key)
}

pub fn index_of(key: TaskKey, processes: &[Process]) -> Option<usize> {
    processes.iter().position(|p| p.key == key)
}
