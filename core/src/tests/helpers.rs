//! Shared fixtures for test-hooks-gated test modules in this crate.

use core::ffi::c_void;

/// No-op task body; `extern "C"` to match the scheduler's `TaskEntry` alias.
pub extern "C" fn dummy_task_entry(_arg: *mut c_void) {}

pub use slopos_sched::test_fixture::mark_current_killed;

/// Mark task `id` for death and wake it, as a kill does.
pub fn kill_task(id: u32) -> bool {
    slopos_sched::task::task_find_by_id(id)
        .is_some_and(|task| slopos_sched::task::task_kill_and_wake(&*task))
}
