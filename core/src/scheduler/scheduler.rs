use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_lib::InterruptFrame;
use slopos_lib::IrqMutex;
use slopos_lib::preempt::PreemptGuard;
use spin::Once;

use slopos_lib::kdiag_timestamp;
use slopos_lib::klog_info;

use slopos_abi::arch::GDT_USER_DATA_SELECTOR;

use crate::platform;
use slopos_lib::wl_currency;

use super::per_cpu;
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE,
    TASK_PRIORITY_IDLE, TASK_STATE_BLOCKED, TASK_STATE_READY, Task, TaskContext, reap_zombies,
    task_get_info, task_is_blocked, task_is_invalid, task_is_ready, task_is_running,
    task_is_terminated, task_record_context_switch, task_record_yield, task_set_current,
    task_set_state,
};
use super::work_steal::try_work_steal;

const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;

static IDLE_WAKEUP_CB: Once<IrqMutex<Option<fn() -> c_int>>> = Once::new();

use core::sync::atomic::AtomicU8;
static SCHEDULER_ENABLED: AtomicU8 = AtomicU8::new(0);
static PREEMPTION_ENABLED: AtomicU8 = AtomicU8::new(SCHEDULER_PREEMPTION_DEFAULT);

#[inline]
fn is_scheduling_active() -> bool {
    SCHEDULER_ENABLED.load(Ordering::Acquire) != 0
        && PREEMPTION_ENABLED.load(Ordering::Acquire) != 0
}

use slopos_mm::paging::{paging_get_kernel_directory, paging_set_current_directory};
use slopos_mm::process_vm::{process_vm_get_page_dir, process_vm_sync_kernel_mappings};
use slopos_mm::user_copy;

use super::ffi_boundary::{context_switch, context_switch_user, kernel_stack_top};

fn current_task_process_id() -> u32 {
    let task = scheduler_get_current_task();
    if task.is_null() {
        return crate::task::INVALID_PROCESS_ID;
    }
    unsafe { (*task).process_id }
}

fn get_default_time_slice() -> u64 {
    SCHED_DEFAULT_TIME_SLICE as u64
}

fn reset_task_quantum(task: *mut Task) {
    if task.is_null() {
        return;
    }
    let slice = unsafe {
        if (*task).time_slice != 0 {
            (*task).time_slice
        } else {
            get_default_time_slice()
        }
    };
    unsafe {
        (*task).time_slice = slice;
        (*task).time_slice_remaining = slice;
    }
}

pub fn clear_scheduler_current_task() {
    let cpu_id = slopos_lib::get_current_cpu();
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(ptr::null_mut());
    });
    task_set_current(ptr::null_mut());
}

pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    if unsafe { (*task).time_slice_remaining } == 0 {
        reset_task_quantum(task);
    }

    let target_cpu = per_cpu::select_target_cpu(task);
    let current_cpu = slopos_lib::get_current_cpu();

    if target_cpu == current_cpu {
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));

        if result != Some(0) {
            return -1;
        }
        0
    } else {
        per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
        });

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        if slopos_lib::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        0
    }
}

pub fn unschedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    let last_cpu = unsafe { (*task).last_cpu as usize };
    per_cpu::with_cpu_scheduler(last_cpu, |sched| {
        sched.remove_task(task);
        if sched.current_task() == task {
            sched.set_current_task(ptr::null_mut());
        }
    });

    0
}

/// Unified task execution for all CPUs.
/// Handles page directory setup, TSS RSP0, context validation, and actual switch.
fn execute_task(cpu_id: usize, from_task: *mut Task, to_task: *mut Task) {
    if to_task.is_null() {
        return;
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, to_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(to_task);
        sched.increment_switches();
    });
    task_set_current(to_task);

    let _balance = wl_currency::check_balance();

    unsafe {
        let is_user_mode = (*to_task).flags & TASK_FLAG_USER_MODE != 0;

        let kernel_rsp = if is_user_mode && (*to_task).kernel_stack_top != 0 {
            (*to_task).kernel_stack_top
        } else {
            kernel_stack_top() as u64
        };
        platform::gdt_set_kernel_rsp0(kernel_rsp);

        if (*to_task).process_id != INVALID_TASK_ID {
            if is_user_mode {
                process_vm_sync_kernel_mappings((*to_task).process_id);
            }
            let page_dir = process_vm_get_page_dir((*to_task).process_id);
            if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
                (*to_task).context.cr3 = (*page_dir).pml4_phys.as_u64();
                paging_set_current_directory(page_dir);
            }
        } else {
            let kernel_dir = paging_get_kernel_directory();
            paging_set_current_directory(kernel_dir);
            let kd_phys = (*kernel_dir).pml4_phys.as_u64();
            if kd_phys != 0 {
                (*to_task).context.cr3 = kd_phys;
            }
        }

        let old_ctx_ptr = if !from_task.is_null() && (*from_task).context_from_user == 0 {
            &raw mut (*from_task).context
        } else {
            ptr::null_mut()
        };

        // Use CS RPL bits to determine dispatch mode, not TASK_FLAG_USER_MODE.
        // A user-mode task mid-syscall has CS=0x8 (kernel) in its saved context,
        // so dispatching via context_switch (retq) is correct. Only dispatch via
        // context_switch_user (iretq) when the saved CS is actually ring 3.
        if (*to_task).context.cs & 3 == 3 {
            validate_user_context(&(*to_task).context, to_task);
            context_switch_user(old_ctx_ptr, &(*to_task).context);
        } else {
            context_switch(old_ctx_ptr, &(*to_task).context);
        }
    }
}

/// Validate that a user-mode task's context has sane values before iretq.
/// Catches context corruption early with a clear panic message rather than
/// a mysterious Invalid Opcode at a garbage address.
#[inline]
fn validate_user_context(ctx: &TaskContext, task: *const Task) {
    let cs = ctx.cs;
    let ss = ctx.ss;
    let rip = ctx.rip;
    let rsp = ctx.rsp;

    // CS and SS must have user RPL (ring 3, bits 0:1 == 3)
    let cs_ok = (cs & 3) == 3;
    let ss_ok = (ss & 3) == 3;
    // RIP must be in user VA range (below kernel half)
    let rip_ok = rip < 0xffff800000000000;
    // RSP must be in user VA range
    let rsp_ok = rsp < 0xffff800000000000;

    if cs_ok && ss_ok && rip_ok && rsp_ok {
        return;
    }

    let task_id = if task.is_null() {
        INVALID_TASK_ID
    } else {
        unsafe { (*task).task_id }
    };
    let cfu = if task.is_null() {
        0
    } else {
        unsafe { (*task).context_from_user }
    };

    let cr3 = ctx.cr3;
    panic!(
        "validate_user_context: corrupt context for task {} (cfu={}): \
         cs=0x{:x} ss=0x{:x} rip=0x{:x} rsp=0x{:x} cr3=0x{:x}",
        task_id, cfu, cs, ss, rip, rsp, cr3
    );
}

pub fn schedule() {
    let cpu_id = slopos_lib::get_current_cpu();
    let preempt_guard = PreemptGuard::new();

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0 {
        drop(preempt_guard);
        return;
    }

    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());

    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_schedule_calls();
    });

    if idle_task.is_null() {
        drop(preempt_guard);
        return;
    }

    if current == idle_task {
        drop(preempt_guard);
        return;
    }

    if !current.is_null() {
        unsafe {
            if task_is_running(current) {
                if task_set_state((*current).task_id, TASK_STATE_READY) == 0 {
                    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                        sched.enqueue_local(current);
                    });
                }
            }
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, idle_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
    });
    task_set_current(idle_task);

    unsafe {
        let kernel_dir = paging_get_kernel_directory();
        paging_set_current_directory(kernel_dir);

        let kdir_phys = (*kernel_dir).pml4_phys.as_u64();
        if !(*kernel_dir).pml4_phys.is_null() {
            (*idle_task).context.cr3 = kdir_phys;
        }

        // Always save kernel context for ALL tasks, including user-mode
        // tasks mid-syscall. Clear context_from_user so the next resume
        // uses context_switch (retq) rather than context_switch_user (iretq).
        let current_ctx = if !current.is_null() {
            if (*current).flags & TASK_FLAG_USER_MODE != 0 {
                (*current).context_from_user = 0;
            }
            &raw mut (*current).context
        } else {
            ptr::null_mut()
        };

        let idle_ctx = &raw const (*idle_task).context;
        context_switch(current_ctx, idle_ctx);
    }
    drop(preempt_guard);
}

pub fn r#yield() {
    let cpu_id = slopos_lib::get_current_cpu();
    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_yields();
    });
    task_record_yield(current);
    schedule();
}

pub fn yield_() {
    r#yield();
}

pub fn block_current_task() {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return;
    }
    if task_is_blocked(current) {
        return;
    }
    if task_set_state(unsafe { (*current).task_id }, TASK_STATE_BLOCKED) != 0 {
        return;
    }
    unschedule_task(current);
    schedule();
}

pub fn task_wait_for(task_id: u32) -> c_int {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return -1;
    }
    if task_id == INVALID_TASK_ID || unsafe { (*current).task_id } == task_id {
        return -1;
    }

    let mut target: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut target) != 0 || target.is_null() {
        unsafe {
            (*current)
                .waiting_on
                .store(INVALID_TASK_ID, Ordering::Release)
        };
        return 0;
    }
    unsafe { (*current).waiting_on.store(task_id, Ordering::Release) };
    block_current_task();
    unsafe {
        (*current)
            .waiting_on
            .store(INVALID_TASK_ID, Ordering::Release)
    };
    0
}

pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    // Only unblock if actually blocked - if task is already ready/running,
    // that's success (idempotent unblock for SMP safety)
    if !task_is_blocked(task) {
        return 0;
    }

    if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
        // CAS failed - another CPU already changed the state, which is fine
        // under SMP. Only fail if task is in a bad state.
        if task_is_terminated(task) || task_is_invalid(task) {
            return -1;
        }
        // Task is ready or running - that's success for unblock
        return 0;
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    schedule_task(task)
}

/// Attempt to wake a task that was waiting on `completed_id`.
/// Returns true if THIS caller won the wake race and should handle the task.
/// Returns false if another caller already woke it or task wasn't waiting on this ID.
///
/// This is the key primitive for lock-free task termination - uses CAS to ensure
/// exactly one waker succeeds per waiting task.
pub fn try_wake_from_task_wait(task: *mut Task, completed_id: u32) -> bool {
    if task.is_null() || completed_id == INVALID_TASK_ID {
        return false;
    }

    // CAS: Atomically clear waiting_on only if it matches completed_id
    // Only ONE caller can succeed this CAS - the "winner"
    let result = unsafe {
        (*task).waiting_on.compare_exchange(
            completed_id,      // expected: waiting on the completed task
            INVALID_TASK_ID,   // desired: no longer waiting
            Ordering::AcqRel,  // success: acquire prior writes, release our write
            Ordering::Acquire, // failure: just acquire to see current value
        )
    };

    match result {
        Ok(_) => {
            // We won the race! Now transition state and enqueue
            // CAS: BLOCKED -> READY (single-winner state transition)
            if task_set_state(unsafe { (*task).task_id }, TASK_STATE_READY) != 0 {
                // State changed unexpectedly - task may be terminated or already ready
                // Check if it's a real failure
                if task_is_terminated(task) || task_is_invalid(task) {
                    klog_info!(
                        "try_wake_from_task_wait: task {} state transition failed (terminated/invalid)",
                        unsafe { (*task).task_id }
                    );
                    return false;
                }
                // Task is already ready/running - that's fine, we still "won" the CAS
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            // Enqueue the task
            if schedule_task(task) != 0 {
                klog_info!(
                    "try_wake_from_task_wait: failed to schedule task {}",
                    unsafe { (*task).task_id }
                );
            }
            true
        }
        Err(_current) => {
            // Lost race OR task is waiting on different ID
            // Either way, not our responsibility to wake
            false
        }
    }
}

/// Unified task exit for all CPUs.
/// Terminates the current task and switches to idle via schedule().
pub fn scheduler_task_exit_impl() -> ! {
    let current = scheduler_get_current_task();
    let cpu_id = slopos_lib::get_current_cpu();

    if current.is_null() {
        klog_info!("scheduler_task_exit: No current task on CPU {}", cpu_id);
        // No current task - just schedule, which will switch to idle
        schedule();
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, ptr::null_mut(), timestamp);

    if crate::task::task_terminate(u32::MAX) != 0 {
        klog_info!("scheduler_task_exit: Failed to terminate current task");
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(ptr::null_mut());
    });
    task_set_current(ptr::null_mut());

    // All CPUs use the unified schedule() path which switches to idle
    schedule();

    klog_info!(
        "scheduler_task_exit: Schedule returned unexpectedly on CPU {}",
        cpu_id
    );
    loop {
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

pub(super) fn unified_idle_loop(_: *mut c_void) {
    loop {
        let cb = IDLE_WAKEUP_CB.get().and_then(|m| *m.lock());
        if let Some(callback) = cb {
            if callback() != 0 {
                r#yield();
                continue;
            }
        }
        let cpu_id = slopos_lib::get_current_cpu();
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

pub fn scheduler_register_idle_wakeup_callback(callback: Option<fn() -> c_int>) {
    IDLE_WAKEUP_CB.call_once(|| IrqMutex::new(None));
    if let Some(mutex) = IDLE_WAKEUP_CB.get() {
        *mutex.lock() = callback;
    }
}

fn deferred_reschedule_callback() {
    if !PreemptGuard::is_active() && is_scheduling_active() {
        schedule();
    }
}

pub fn init_scheduler() -> c_int {
    SCHEDULER_ENABLED.store(0, Ordering::Release);
    PREEMPTION_ENABLED.store(SCHEDULER_PREEMPTION_DEFAULT, Ordering::Release);

    user_copy::register_current_task_provider(current_task_process_id);

    per_cpu::init_all_percpu_schedulers();

    slopos_lib::preempt::register_reschedule_callback(deferred_reschedule_callback);

    slopos_lib::panic_recovery::register_panic_cleanup(sched_panic_cleanup);

    0
}

fn sched_panic_cleanup() {
    unsafe {
        scheduler_force_unlock();
        crate::task::task_manager_force_unlock();
    }
}

pub fn create_idle_task() -> c_int {
    create_idle_task_for_cpu(0)
}

pub fn create_idle_task_for_cpu(cpu_id: usize) -> c_int {
    let idle_task_id = unsafe {
        crate::task::task_create(
            b"idle\0".as_ptr() as *const i8,
            core::mem::transmute(unified_idle_loop as *const ()),
            ptr::null_mut(),
            TASK_PRIORITY_IDLE,
            TASK_FLAG_KERNEL_MODE,
        )
    };
    if idle_task_id == INVALID_TASK_ID {
        return -1;
    }
    let mut idle_task: *mut Task = ptr::null_mut();
    if task_get_info(idle_task_id, &mut idle_task) != 0 {
        return -1;
    }

    unsafe {
        (*idle_task).cpu_affinity = 1 << cpu_id;
        (*idle_task).last_cpu = cpu_id as u8;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    0
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    // Guard against double-enable on BSP
    if cpu_id == 0 {
        let already_enabled = SCHEDULER_ENABLED.load(Ordering::Acquire) != 0;
        if already_enabled {
            loop {
                unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
            }
        }
    }
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    if cpu_id == 0 {
        if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0 {
            SCHEDULER_ENABLED.store(1, Ordering::Release);
        }
        scheduler_set_preemption_enabled(SCHEDULER_PREEMPTION_DEFAULT as c_int);
    }

    slopos_lib::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task()).unwrap_or(ptr::null_mut());

    if idle_task.is_null() {
        klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
        loop {
            unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
        }
    }

    unsafe {
        let return_ctx = per_cpu::get_ap_return_context(cpu_id);
        if !return_ctx.is_null() {
            crate::ffi_boundary::init_kernel_context(return_ctx);
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
    });
    task_set_current(idle_task);

    // APs: switch from Limine boot stack (HHDM-transient) to idle's kernel stack.
    // BSP: stays on .bss boot stack which is stable kernel virtual memory.
    // We cannot switch BSP's stack here because local variables would become invalid.
    if cpu_id != 0 {
        unsafe {
            let new_rsp = (*idle_task).kernel_stack_top;
            if new_rsp != 0 {
                core::arch::asm!(
                    "mov rsp, {0}",
                    "mov rbp, rsp",
                    in(reg) new_rsp,
                    options(nostack)
                );
            }
        }
    }

    scheduler_loop(cpu_id, idle_task)
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        // 1. Drain remote inbox (continuous for all CPUs)
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        // 2. Check for pause (test reinitialization)
        if per_cpu::are_aps_paused() {
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        // 3. Dequeue highest priority task
        let next_task =
            per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority())
                .unwrap_or(ptr::null_mut());

        // 4. Execute task if available
        if !next_task.is_null() {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.set_executing_task(true);
            });

            // Re-check pause after marking executing
            if per_cpu::are_aps_paused() {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.enqueue_local(next_task);
                    sched.set_executing_task(false);
                });
                core::hint::spin_loop();
                continue;
            }

            if task_is_terminated(next_task) || !task_is_ready(next_task) {
                per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                    sched.set_executing_task(false);
                });
                continue;
            }

            execute_task(cpu_id, idle_task, next_task);

            // Post-switch: restore idle as current and re-queue task if runnable
            let timestamp = kdiag_timestamp();
            task_record_context_switch(next_task, idle_task, timestamp);

            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.set_current_task(idle_task);
            });
            task_set_current(idle_task);

            unsafe {
                let kernel_dir = paging_get_kernel_directory();
                paging_set_current_directory(kernel_dir);
                if !(*kernel_dir).pml4_phys.is_null() {
                    (*idle_task).context.cr3 = (*kernel_dir).pml4_phys.as_u64();
                }

                if !task_is_terminated(next_task) && task_is_running(next_task) {
                    if task_set_state((*next_task).task_id, TASK_STATE_READY) == 0 {
                        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                            sched.enqueue_local(next_task);
                        });
                    }
                }
            }

            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.set_executing_task(false);
            });
            continue;
        }

        // 5. Try work stealing if no local tasks
        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        // 6. Reap zombie tasks (deferred cleanup for terminated tasks)
        // Only BSP handles zombie reaping to avoid contention
        if cpu_id == 0 {
            reap_zombies();
        }

        // 7. Idle - increment stats and halt
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });

        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}

pub fn stop_scheduler() {
    SCHEDULER_ENABLED.store(0, Ordering::Release);
}

pub fn scheduler_shutdown() {
    SCHEDULER_ENABLED.store(0, Ordering::Release);
    per_cpu::clear_all_cpu_queues();
}

pub fn get_scheduler_stats(
    context_switches: *mut u64,
    yields: *mut u64,
    ready_tasks: *mut u32,
    schedule_calls: *mut u32,
) {
    if !context_switches.is_null() {
        unsafe { *context_switches = per_cpu::get_total_switches() };
    }
    if !yields.is_null() {
        unsafe { *yields = per_cpu::get_total_yields() };
    }
    if !schedule_calls.is_null() {
        unsafe { *schedule_calls = per_cpu::get_total_schedule_calls() };
    }
    if !ready_tasks.is_null() {
        unsafe { *ready_tasks = per_cpu::get_total_ready_tasks() };
    }
}

pub fn scheduler_is_enabled() -> c_int {
    SCHEDULER_ENABLED.load(Ordering::Acquire) as c_int
}

pub fn scheduler_get_current_task() -> *mut Task {
    let cpu_id = slopos_lib::get_current_cpu();
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task()).unwrap_or(ptr::null_mut())
}

pub fn scheduler_set_preemption_enabled(enabled: c_int) {
    let val = if enabled != 0 { 1u8 } else { 0u8 };
    PREEMPTION_ENABLED.store(val, Ordering::Release);
    if val == 0 {
        PreemptGuard::clear_reschedule_pending();
    }
    if val != 0 {
        platform::timer_enable_irq();
    } else {
        platform::timer_disable_irq();
    }
}

pub fn scheduler_is_preemption_enabled() -> c_int {
    PREEMPTION_ENABLED.load(Ordering::Acquire) as c_int
}

pub fn scheduler_timer_tick() {
    if PreemptGuard::is_active() {
        PreemptGuard::set_reschedule_pending();
        return;
    }

    let cpu_id = slopos_lib::get_current_cpu();

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
        sched.increment_ticks();
    });

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0
        || PREEMPTION_ENABLED.load(Ordering::Acquire) == 0
    {
        return;
    }

    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    if current.is_null() {
        return;
    }

    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
    if current == idle_task {
        let ready_count =
            per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
        if ready_count > 0 {
            PreemptGuard::set_reschedule_pending();
        }
        return;
    }

    if unsafe { (*current).flags } & TASK_FLAG_NO_PREEMPT != 0 {
        return;
    }

    unsafe {
        if (*current).time_slice_remaining > 0 {
            (*current).time_slice_remaining -= 1;
        }
        if (*current).time_slice_remaining > 0 {
            return;
        }
    }

    let ready_count =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0);
    if ready_count == 0 {
        reset_task_quantum(current);
        return;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_preemptions();
    });
    PreemptGuard::set_reschedule_pending();
}

pub fn scheduler_request_reschedule_from_interrupt() {
    if is_scheduling_active() && !PreemptGuard::is_active() {
        PreemptGuard::set_reschedule_pending();
    }
}

/// Save user context from an interrupt frame when timer preempts user-mode code.
///
/// This must be called from the timer IRQ handler BEFORE scheduler_timer_tick()
/// when the interrupted code was running in user mode (CS ring 3).
///
/// Without this, timer preemptions of user tasks would lose their context:
/// - The syscall path saves user context via save_user_context() in dispatch.rs
/// - Timer preemptions must also save user context from the interrupt frame
/// - Otherwise, context_switch_user would save kernel-mode registers instead
pub fn save_preempt_context(frame: *mut InterruptFrame) {
    if frame.is_null() {
        return;
    }

    let task = scheduler_get_current_task();
    if task.is_null() {
        return;
    }

    // Only save for user-mode tasks
    let is_user_mode = unsafe { (*task).flags & TASK_FLAG_USER_MODE != 0 };
    if !is_user_mode {
        return;
    }

    // Verify the interrupt came from user mode (ring 3)
    let cs = unsafe { (*frame).cs };

    if (cs & 3) != 3 {
        return;
    }

    // Save user registers from the interrupt frame to the task's context
    unsafe {
        let ctx: &mut TaskContext = &mut (*task).context;
        ctx.rax = (*frame).rax;
        ctx.rbx = (*frame).rbx;
        ctx.rcx = (*frame).rcx;
        ctx.rdx = (*frame).rdx;
        ctx.rsi = (*frame).rsi;
        ctx.rdi = (*frame).rdi;
        ctx.rbp = (*frame).rbp;
        ctx.r8 = (*frame).r8;
        ctx.r9 = (*frame).r9;
        ctx.r10 = (*frame).r10;
        ctx.r11 = (*frame).r11;
        ctx.r12 = (*frame).r12;
        ctx.r13 = (*frame).r13;
        ctx.r14 = (*frame).r14;
        ctx.r15 = (*frame).r15;
        ctx.rip = (*frame).rip;
        ctx.rsp = (*frame).rsp;
        ctx.rflags = (*frame).rflags;
        ctx.cs = (*frame).cs;
        ctx.ss = (*frame).ss;
        ctx.ds = GDT_USER_DATA_SELECTOR as u64;
        ctx.es = GDT_USER_DATA_SELECTOR as u64;
        ctx.fs = 0;
        ctx.gs = 0;

        // Mark that we have fresh user context saved
        (*task).context_from_user = 1;
    }
}

pub fn scheduler_handle_post_irq() {
    if PreemptGuard::is_active() {
        return;
    }

    if !PreemptGuard::is_reschedule_pending() {
        return;
    }

    if is_scheduling_active() {
        PreemptGuard::clear_reschedule_pending();
        schedule();
    }
}

pub fn boot_step_task_manager_init() -> c_int {
    crate::task::init_task_manager()
}

pub fn boot_step_scheduler_init() -> c_int {
    init_scheduler()
}

pub fn boot_step_idle_task() -> c_int {
    create_idle_task()
}

pub fn init_scheduler_for_ap(cpu_id: usize) {
    per_cpu::init_percpu_scheduler(cpu_id);

    // Create idle task for this AP. At this point BSP has already initialized
    // the task manager and heap, so it's safe to allocate.
    if cpu_id != 0 {
        let idle = per_cpu::create_ap_idle_task(cpu_id);
        if idle.is_null() {
            klog_info!(
                "SCHED: Warning - failed to create idle task for CPU {}",
                cpu_id
            );
        }
    }
}

pub fn get_percpu_scheduler_stats(
    cpu_id: usize,
    switches: *mut u64,
    preemptions: *mut u64,
    ready_tasks: *mut u32,
) {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        if !switches.is_null() {
            unsafe { *switches = sched.total_switches.load(Ordering::Relaxed) };
        }
        if !preemptions.is_null() {
            unsafe { *preemptions = sched.total_preemptions.load(Ordering::Relaxed) };
        }
        if !ready_tasks.is_null() {
            unsafe { *ready_tasks = sched.total_ready_count() };
        }
    });
}

pub fn get_total_ready_tasks_all_cpus() -> u32 {
    per_cpu::get_total_ready_tasks()
}

pub fn send_reschedule_ipi(target_cpu: usize) {
    use slopos_abi::arch::x86_64::idt::RESCHEDULE_IPI_VECTOR;

    let current_cpu = slopos_lib::get_current_cpu();
    if target_cpu == current_cpu {
        return;
    }

    if let Some(apic_id) = slopos_lib::apic_id_from_cpu_index(target_cpu) {
        slopos_lib::send_ipi_to_cpu(apic_id, RESCHEDULE_IPI_VECTOR);
    }
}

pub unsafe fn scheduler_force_unlock() {
    // No global scheduler mutex to unlock - per-CPU schedulers use lockless atomics
}
