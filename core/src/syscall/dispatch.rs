use slopos_lib::klog_info;

use crate::sched::save_task_context_from_interrupt_frame;
use crate::sched::scheduler_get_current_task;
use crate::syscall::handlers::syscall_lookup;

use crate::scheduler::task_struct::Task;
use slopos_abi::task::{TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE};
use slopos_lib::InterruptFrame;

struct NoPreemptGuard {
    task: *mut Task,
}

impl NoPreemptGuard {
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

    let sysno = unsafe { (*frame).rax };

    let task = scheduler_get_current_task() as *mut Task;
    if task.is_null() {
        return;
    }
    unsafe {
        if ((*task).flags & TASK_FLAG_USER_MODE) == 0 {
            return;
        }
    }

    // CRITICAL: Set NO_PREEMPT *before* saving user context.
    //
    // save_task_context_from_interrupt_frame sets context_from_user=1,
    // which tells the scheduler it can resume this task directly from
    // task.context via IRETQ (skipping kernel context save).  Without
    // NO_PREEMPT held first, a timer interrupt between the context save
    // and the handler completion could trigger a context switch that
    // resumes from stale task.context values (e.g. rax still holding
    // the raw syscall number instead of the handler's return code).
    //
    // With NO_PREEMPT set, the scheduler sees in_syscall_block_path=true
    // and falls back to saving/restoring kernel context, which correctly
    // resumes execution within syscall_handle rather than jumping back
    // to userspace with stale register values.
    let _no_preempt = NoPreemptGuard::new(task);

    // Save user context snapshot.  frame.rax still holds the original
    // syscall number, giving the context a correct pre-syscall snapshot
    // (used for signal delivery, core dumps, ptrace).
    save_task_context_from_interrupt_frame(task, frame, true);

    // Clobber frame.rax with a safe negative sentinel.  If the handler
    // panics or misses a return path, userland gets -EINVAL rather than
    // the raw syscall number interpreted as a character.
    unsafe {
        (*frame).rax = slopos_abi::syscall::ERRNO_EINVAL as u64;
    }

    let pid = unsafe { (*task).process_id };
    let _provider_guard = slopos_mm::user_copy::set_syscall_process_id(pid);

    let entry = syscall_lookup(sysno);
    if entry.is_null() {
        klog_info!("SYSCALL: Unknown syscall {} -> ENOSYS", sysno);
        unsafe {
            (*frame).rax = slopos_abi::syscall::ENOSYS_RETURN;
        }
    } else {
        let handler = unsafe { (*entry).handler };
        if let Some(func) = handler {
            func(task, frame);
            crate::syscall::signal::deliver_pending_signal(task, frame);
        }
    }

    // Sync all frame registers that may have been modified back to the
    // saved user context.  This MUST happen while NO_PREEMPT is still
    // held (before NoPreemptGuard drops).
    //
    // After the guard drops there is a window before the assembly `cli`
    // where a timer interrupt can trigger schedule_from_trap_exit().
    // The scheduler sees context_from_user=1 and NO_PREEMPT=0, so it
    // may resume this task from task.context via context_switch_user/IRETQ.
    // Without this sync, stale pre-handler values would leak to userland.
    //
    // Registers potentially modified by:
    //   - Syscall handler: rax (return value)
    //   - Signal delivery: rip, rsp, rdi, rsi, rdx (redirected to
    //     signal trampoline)
    unsafe {
        (*task).context.rax = (*frame).rax;
        (*task).context.rip = (*frame).rip;
        (*task).context.rsp = (*frame).rsp;
        (*task).context.rdi = (*frame).rdi;
        (*task).context.rsi = (*frame).rsi;
        (*task).context.rdx = (*frame).rdx;
    }
}
