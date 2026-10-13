//! `slopos-pidfd` — a process-exit fd (`FileKind::Pidfd`).
//!
//! `pidfd_open(pid)` returns an fd that becomes `POLLIN`-ready when the target
//! task exits, so a waiter can reach a child through `poll(2)` / SlopRing
//! `OP_POLL_ADD` instead of busy-polling `waitpid` with `WNOHANG`.

#![no_std]
#![forbid(unsafe_code)]

pub mod file_ops;

pub use file_ops::PIDFD_FILE_OPS;

use slopos_abi::Errno;
use slopos_abi::task::INVALID_TASK_ID;
use slopos_fs::FdFlags;
use slopos_fs::fileio::FdTable;

/// `pidfd_open(2)`: any live process, as Linux allows, since all a pidfd
/// tells its holder is when the process exits, which `process_list` already
/// tells everyone. Returns the new fd or a negated errno:
/// - `-ESRCH` if no such task exists (e.g. already reaped),
/// - `-EINVAL` if it is a thread other than its group's leader.
pub fn pidfd_open(table: FdTable, target_task_id: u32) -> i32 {
    let Some(task) = slopos_sched::task::task_find_by_id(target_task_id) else {
        return Errno::ESRCH.raw();
    };
    if task.tgid != INVALID_TASK_ID && task.tgid != target_task_id {
        return Errno::EINVAL.raw();
    }
    pidfd_install(table, target_task_id)
}

/// A close-on-exec pidfd for `pid`, as Linux makes every pidfd, live or not:
/// task ids are never reused, so one that has gone names nothing else and its
/// pidfd is simply readable at once.
pub fn pidfd_install(table: FdTable, pid: u32) -> i32 {
    // No per-open kernel state, hence no backing to drop.
    slopos_fs::fileio_open_fd_with_ops(
        table,
        &file_ops::PIDFD_FILE_OPS,
        pid as usize,
        None,
        FdFlags {
            cloexec: true,
            close_on_fork: false,
        },
    )
}
