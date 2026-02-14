use slopos_lib::klog_info;

use crate::sched::save_task_context_from_interrupt_frame;
use crate::sched::scheduler_get_current_task;
use crate::syscall::handlers::syscall_lookup;

use crate::scheduler::task_struct::Task;
use slopos_abi::task::{TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE};
use slopos_lib::InterruptFrame;

/// RAII guard that clears NO_PREEMPT on the task when dropped.
/// Ensures the flag cannot leak even if the syscall handler panics.
struct NoPreemptGuard {
    task: *mut Task,
}

impl NoPreemptGuard {
    /// Set NO_PREEMPT on the task and return a guard that clears it on drop.
    fn new(task: *mut Task) -> Self {
        unsafe { (*task).flags |= TASK_FLAG_NO_PREEMPT };
        Self { task }
    }
}

impl Drop for NoPreemptGuard {
    fn drop(&mut self) {
        if !self.task.is_null() {
            unsafe { (*self.task).flags &= !TASK_FLAG_NO_PREEMPT };
        }
    }
}

pub fn syscall_handle(frame: *mut InterruptFrame) {
    if frame.is_null() {
        return;
    }

    let task = scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        return;
    }
    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }
    }

    save_task_context_from_interrupt_frame(task, frame, true);

    // RAII guards ensure cleanup on all exit paths including panics.
    let _no_preempt = NoPreemptGuard::new(task);
    let pid = unsafe { (*task).process_id };
    let _provider_guard = slopos_mm::user_copy::set_syscall_process_id(pid);

    let sysno = unsafe { (*frame).rax };
    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
        unsafe { (*frame).rax = slopos_abi::syscall::ENOSYS_RETURN };
        return;
    }

    let handler = unsafe { (*entry).handler };
    if let Some(func) = handler {
        func(task, frame);
        crate::syscall::signal::deliver_pending_signal(task, frame);
    }
}
