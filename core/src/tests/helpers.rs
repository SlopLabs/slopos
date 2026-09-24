//! Shared fixtures for test-hooks-gated test modules in this crate.

use core::ffi::c_void;

/// No-op task body; `extern "C"` to match the scheduler's `TaskEntry` alias.
pub extern "C" fn dummy_task_entry(_arg: *mut c_void) {}

/// Set or clear the current task's kill flag, answering whether it took.
/// [`SIGNAL_KILLED`](slopos_abi::signal::SIGNAL_KILLED) is outside
/// `SIGNAL_MASK`, so the raw field is the only way back out of the state.
pub fn mark_current_killed(on: bool) -> bool {
    use core::sync::atomic::Ordering;
    let Some(current) = slopos_sched::task_struct::Current::get() else {
        return false;
    };
    let task = current.task();
    if on {
        task.signal_pending
            .fetch_or(slopos_abi::signal::SIGNAL_KILLED, Ordering::AcqRel);
    } else {
        task.signal_pending
            .fetch_and(!slopos_abi::signal::SIGNAL_KILLED, Ordering::AcqRel);
    }
    task.is_killed() == on
}
