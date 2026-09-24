//! Thin shim over the task-exit path. `slopos_ostd::task::switch` is the sole
//! context-switch implementation.

/// Task exit path for `super::kthread`. OSTD's `task_entry_trampoline`
/// reaches the same impl through the registered `TaskExitHook`, so this
/// needs no C linkage.
pub fn scheduler_task_exit() -> ! {
    super::scheduler::scheduler_task_exit_impl()
}
