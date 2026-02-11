use core::ffi::{c_int, c_void};
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_lib::InterruptFrame;
use slopos_lib::IrqMutex;
use slopos_lib::cpu;
use slopos_lib::preempt::PreemptGuard;
use spin::Once;

use slopos_lib::kdiag_timestamp;
use slopos_lib::klog_info;

use slopos_lib::arch::gdt::SegmentSelector;

use crate::platform;

use super::per_cpu;
use super::task::{
    INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT, TASK_FLAG_USER_MODE,
    TASK_PRIORITY_IDLE, Task, TaskContext, TaskStatus, reap_zombies, task_find_by_id,
    task_get_info, task_is_blocked, task_is_invalid, task_is_ready, task_is_running,
    task_is_terminated, task_record_context_switch, task_record_yield, task_set_current,
    task_set_state, task_set_state_with_reason,
};
use super::work_steal::try_work_steal;
use slopos_abi::task::{BlockReason, MAX_TASKS};

const SCHED_DEFAULT_TIME_SLICE: u32 = 10;
const SCHEDULER_PREEMPTION_DEFAULT: u8 = 1;
const USER_SPACE_TOP: u64 = 0xffff_8000_0000_0000;

static IDLE_WAKEUP_CB: Once<IrqMutex<Option<fn() -> c_int>>> = Once::new();

use core::sync::atomic::AtomicU8;
static SCHEDULER_ENABLED: AtomicU8 = AtomicU8::new(0);
static PREEMPTION_ENABLED: AtomicU8 = AtomicU8::new(SCHEDULER_PREEMPTION_DEFAULT);

#[derive(Copy, Clone)]
struct SleepEntry {
    task_id: u32,
    wake_tick: u64,
    active: bool,
}

impl SleepEntry {
    const fn empty() -> Self {
        Self {
            task_id: INVALID_TASK_ID,
            wake_tick: 0,
            active: false,
        }
    }
}

struct SleepQueue {
    entries: [SleepEntry; MAX_TASKS],
}

impl SleepQueue {
    const fn new() -> Self {
        Self {
            entries: [SleepEntry::empty(); MAX_TASKS],
        }
    }

    fn clear(&mut self) {
        self.entries = [SleepEntry::empty(); MAX_TASKS];
    }

    fn upsert(&mut self, task_id: u32, wake_tick: u64) -> bool {
        let mut free_idx = None;
        for (idx, entry) in self.entries.iter_mut().enumerate() {
            if entry.active && entry.task_id == task_id {
                entry.wake_tick = wake_tick;
                return true;
            }
            if !entry.active && free_idx.is_none() {
                free_idx = Some(idx);
            }
        }

        if let Some(idx) = free_idx {
            self.entries[idx] = SleepEntry {
                task_id,
                wake_tick,
                active: true,
            };
            true
        } else {
            false
        }
    }

    fn remove(&mut self, task_id: u32) {
        for entry in self.entries.iter_mut() {
            if entry.active && entry.task_id == task_id {
                *entry = SleepEntry::empty();
                break;
            }
        }
    }

    fn collect_due(&mut self, now_tick: u64, out: &mut [u32; MAX_TASKS]) -> usize {
        let mut count = 0usize;
        for entry in self.entries.iter_mut() {
            if !entry.active {
                continue;
            }
            if tick_reached(now_tick, entry.wake_tick) {
                if count < out.len() {
                    out[count] = entry.task_id;
                    count += 1;
                }
                *entry = SleepEntry::empty();
            }
        }
        count
    }
}

static SLEEP_QUEUE: IrqMutex<SleepQueue> = IrqMutex::new(SleepQueue::new());

#[inline]
fn tick_reached(now_tick: u64, deadline_tick: u64) -> bool {
    // Wraparound-safe compare: true when now >= deadline in unsigned tick space.
    now_tick.wrapping_sub(deadline_tick) < (1u64 << 63)
}

fn ms_to_sleep_ticks(ms: u32) -> u64 {
    let freq = platform::timer_frequency() as u64;
    if freq == 0 {
        return 1;
    }

    let ticks = (ms as u64).saturating_mul(freq).saturating_add(999) / 1000;
    ticks.max(1)
}

fn wake_sleeping_task(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }

    let task = task_find_by_id(task_id);
    if task.is_null() || task_is_invalid(task) || task_is_terminated(task) {
        return;
    }

    let is_sleep_blocked =
        task_is_blocked(task) && unsafe { (*task).block_reason == BlockReason::Sleep };
    if !is_sleep_blocked {
        return;
    }

    if task_set_state_with_reason(task_id, TaskStatus::Ready, BlockReason::None) != 0 {
        return;
    }

    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let _ = schedule_task(task);
}

fn wake_due_sleepers(now_tick: u64) {
    let mut due = [INVALID_TASK_ID; MAX_TASKS];
    let due_count = {
        let mut queue = SLEEP_QUEUE.lock();
        queue.collect_due(now_tick, &mut due)
    };

    for task_id in due.iter().take(due_count) {
        wake_sleeping_task(*task_id);
    }
}

pub fn cancel_sleep(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    SLEEP_QUEUE.lock().remove(task_id);
}

pub fn sleep_current_task_ms(ms: u32) -> c_int {
    if ms == 0 {
        return 0;
    }

    if !is_scheduling_active() {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let current = scheduler_get_current_task();
    if current.is_null() {
        return -1;
    }
    if per_cpu::is_idle_task(current) {
        platform::timer_poll_delay_ms(ms);
        return 0;
    }

    let task_id = unsafe { (*current).task_id };
    if task_id == INVALID_TASK_ID {
        return -1;
    }

    let now_tick = platform::timer_ticks();
    let wake_tick = now_tick.wrapping_add(ms_to_sleep_ticks(ms));
    if !SLEEP_QUEUE.lock().upsert(task_id, wake_tick) {
        return -1;
    }

    if task_set_state_with_reason(task_id, TaskStatus::Blocked, BlockReason::Sleep) != 0 {
        cancel_sleep(task_id);
        return -1;
    }

    unschedule_task(current);
    schedule();
    0
}

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

#[inline]
fn scheduler_tasks_for_cpu(cpu_id: usize) -> (*mut Task, *mut Task) {
    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    let idle =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
    (current, idle)
}

#[inline]
fn scheduler_ready_count(cpu_id: usize) -> u32 {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count()).unwrap_or(0)
}

#[inline]
fn set_scheduler_current_task(cpu_id: usize, task: *mut Task) {
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(task);
    });
    task_set_current(task);
}

fn requeue_running_task(cpu_id: usize, current: *mut Task) {
    if current.is_null() {
        return;
    }

    unsafe {
        if task_is_running(current) && task_set_state((*current).task_id, TaskStatus::Ready) == 0 {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.enqueue_local(current);
            });
        }
    }
}

fn switch_to_kernel_address_space(task: *mut Task) {
    unsafe {
        let kernel_dir = paging_get_kernel_directory();
        paging_set_current_directory(kernel_dir);
        if !(*kernel_dir).pml4_phys.is_null() && !task.is_null() {
            (*task).context.cr3 = (*kernel_dir).pml4_phys.as_u64();
        }
    }
}

fn switch_from_current_to_idle(cpu_id: usize, current: *mut Task, idle_task: *mut Task) {
    let timestamp = kdiag_timestamp();
    task_record_context_switch(current, idle_task, timestamp);

    set_scheduler_current_task(cpu_id, idle_task);
    switch_to_kernel_address_space(idle_task);

    unsafe {
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
}

#[inline]
fn task_has_no_preempt_flag(task: *mut Task) -> bool {
    !task.is_null() && (unsafe { (*task).flags } & TASK_FLAG_NO_PREEMPT != 0)
}

#[inline]
fn consume_time_slice(current: *mut Task) -> bool {
    unsafe {
        if (*current).time_slice_remaining > 0 {
            (*current).time_slice_remaining -= 1;
        }
        (*current).time_slice_remaining > 0
    }
}

#[inline]
fn mark_preempt_if_ready(cpu_id: usize) {
    if scheduler_ready_count(cpu_id) > 0 {
        PreemptGuard::set_reschedule_pending();
    }
}

pub fn clear_scheduler_current_task() {
    let cpu_id = slopos_lib::get_current_cpu();
    set_scheduler_current_task(cpu_id, ptr::null_mut());
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

    let Some(target_cpu) = per_cpu::select_target_cpu(task) else {
        return -1;
    };
    let current_cpu = slopos_lib::get_current_cpu();

    if target_cpu == current_cpu {
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));

        if result != Some(0) {
            return -1;
        }
        0
    } else {
        let push_result = per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
            0
        });
        if push_result != Some(0) {
            return -1;
        }

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

    let cpu_count = slopos_lib::get_cpu_count();
    for cpu_id in 0..cpu_count {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.remove_task(task);
        });
    }

    0
}

/// Unified task execution for all CPUs.
/// Handles page directory setup, TSS RSP0, context validation, and actual switch.
fn execute_task(cpu_id: usize, from_task: *mut Task, to_task: *mut Task) {
    if to_task.is_null() {
        return;
    }

    unsafe {
        let rip = (*to_task).context.rip;
        if rip < 0x1000 {
            klog_info!(
                "SCHED: refusing to dispatch task {} with invalid rip=0x{:x}",
                (*to_task).task_id,
                rip
            );
            let _ = crate::task::task_terminate((*to_task).task_id);
            return;
        }
    }

    let timestamp = kdiag_timestamp();
    task_record_context_switch(from_task, to_task, timestamp);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(to_task);
        sched.increment_switches();
    });
    task_set_current(to_task);

    unsafe {
        let is_user_mode = (*to_task).flags & TASK_FLAG_USER_MODE != 0;

        if is_user_mode {
            slopos_lib::cpu::msr::write_msr(slopos_lib::cpu::msr::Msr::FS_BASE, (*to_task).fs_base);
        } else {
            slopos_lib::cpu::msr::write_msr(slopos_lib::cpu::msr::Msr::FS_BASE, 0);
        }

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

fn run_ready_task_from_idle(cpu_id: usize, idle_task: *mut Task) -> bool {
    let next_task = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority())
        .unwrap_or(ptr::null_mut());

    if next_task.is_null() {
        return false;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(true);
    });

    if per_cpu::should_pause_scheduler_loop(cpu_id) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            let _ = sched.enqueue_local(next_task);
            sched.set_executing_task(false);
        });
        core::hint::spin_loop();
        return false;
    }

    if task_is_terminated(next_task) || !task_is_ready(next_task) {
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.set_executing_task(false);
        });
        return false;
    }

    execute_task(cpu_id, idle_task, next_task);

    let timestamp = kdiag_timestamp();
    task_record_context_switch(next_task, idle_task, timestamp);

    set_scheduler_current_task(cpu_id, idle_task);

    switch_to_kernel_address_space(idle_task);

    unsafe {
        if !task_is_terminated(next_task)
            && task_is_running(next_task)
            && task_set_state((*next_task).task_id, TaskStatus::Ready) == 0
        {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                let _ = sched.enqueue_local(next_task);
            });
        }
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(false);
    });

    true
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
    let rip_ok = rip < USER_SPACE_TOP;
    // RSP must be in user VA range
    let rsp_ok = rsp < USER_SPACE_TOP;

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
    let irq_flags = cpu::save_flags_cli();

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0 {
        cpu::restore_flags(irq_flags);
        return;
    }

    let (current, idle_task) = scheduler_tasks_for_cpu(cpu_id);

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.increment_schedule_calls();
    });

    if idle_task.is_null() {
        cpu::restore_flags(irq_flags);
        return;
    }

    if current == idle_task {
        let _ = run_ready_task_from_idle(cpu_id, idle_task);
        cpu::restore_flags(irq_flags);
        return;
    }

    requeue_running_task(cpu_id, current);
    switch_from_current_to_idle(cpu_id, current, idle_task);
    cpu::restore_flags(irq_flags);
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
    if task_set_state(unsafe { (*current).task_id }, TaskStatus::Blocked) != 0 {
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

    if task_set_state(unsafe { (*task).task_id }, TaskStatus::Ready) != 0 {
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
            if task_set_state(unsafe { (*task).task_id }, TaskStatus::Ready) != 0 {
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
    SLEEP_QUEUE.lock().clear();

    slopos_lib::preempt::register_reschedule_callback(deferred_reschedule_callback);

    slopos_lib::panic_recovery::register_panic_cleanup(sched_panic_cleanup);

    0
}

fn sched_panic_cleanup() {
    // SAFETY: Called from the panic recovery path after longjmp. The lock
    // may have been held when the panic occurred and the guard was lost.
    // We poison-unlock to mark the data as potentially inconsistent; the
    // scheduler reinit path checks is_poisoned() before accepting operations.
    unsafe {
        scheduler_force_unlock();
        crate::task::task_manager_poison_unlock();
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
        (*idle_task).cpu_affinity = per_cpu::affinity_mask_for_cpu(cpu_id);
        (*idle_task).last_cpu = cpu_id as u8;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.set_idle_task(idle_task);
    });

    0
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleStackResolveError {
    MissingIdleTask,
    MissingKernelStack,
}

pub(crate) fn resolve_idle_stack_for_cpu(
    cpu_id: usize,
) -> Result<(*mut Task, u64), IdleStackResolveError> {
    let idle_task =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.idle_task()).unwrap_or(ptr::null_mut());
    if idle_task.is_null() {
        return Err(IdleStackResolveError::MissingIdleTask);
    }

    let stack_top = unsafe { (*idle_task).kernel_stack_top };
    if stack_top == 0 {
        return Err(IdleStackResolveError::MissingKernelStack);
    }

    Ok((idle_task, stack_top))
}

#[inline(never)]
unsafe fn enter_scheduler_on_idle_stack(cpu_id: usize, idle_task: *mut Task, stack_top: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "mov rsp, rdx",
            "mov rbp, rsp",
            "call {target}",
            "ud2",
            target = sym scheduler_loop_entry,
            in("rdi") cpu_id,
            in("rsi") idle_task,
            in("rdx") stack_top,
            options(noreturn)
        );
    }
}

extern "C" fn scheduler_loop_entry(cpu_id: usize, idle_task: *mut Task) -> ! {
    scheduler_loop(cpu_id, idle_task)
}

pub fn enter_scheduler(cpu_id: usize) -> ! {
    let already_enabled =
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.is_enabled()).unwrap_or(false);
    if already_enabled {
        loop {
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.enable();
    });

    // Entering any scheduler loop marks global scheduling as active.
    // Do not touch timer IRQ routing here; boot may not have configured IOAPIC yet.
    SCHEDULER_ENABLED.store(1, Ordering::Release);

    slopos_lib::mark_cpu_online(cpu_id);
    klog_info!("SCHED: CPU {} scheduler online", cpu_id);

    let (idle_task, idle_stack_top) = match resolve_idle_stack_for_cpu(cpu_id) {
        Ok(values) => values,
        Err(IdleStackResolveError::MissingIdleTask) => {
            klog_info!("SCHED: CPU {} has no idle task, halting", cpu_id);
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
        Err(IdleStackResolveError::MissingKernelStack) => {
            klog_info!(
                "SCHED: CPU {} idle task has no kernel stack, halting",
                cpu_id
            );
            loop {
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
    };

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

    unsafe { enter_scheduler_on_idle_stack(cpu_id, idle_task, idle_stack_top) }
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        // 1. Drain remote inbox (continuous for all CPUs)
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        // 2. Check for pause (test reinitialization)
        if per_cpu::should_pause_scheduler_loop(cpu_id) {
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        // 3/4. Dequeue and execute one ready task if available.
        if run_ready_task_from_idle(cpu_id, idle_task) {
            continue;
        }

        // 5. Try work stealing if no local tasks
        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        // 6. Reap zombie tasks (deferred cleanup for terminated tasks)
        reap_zombies();

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
    SLEEP_QUEUE.lock().clear();
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
    let cpu_id = slopos_lib::get_current_cpu();
    let (current, idle_task) = scheduler_tasks_for_cpu(cpu_id);

    let preempt_active = PreemptGuard::is_active();
    let running_idle = !current.is_null() && current == idle_task;

    if preempt_active && !running_idle {
        PreemptGuard::set_reschedule_pending();
        return;
    }

    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
        sched.increment_ticks();
    });

    wake_due_sleepers(platform::timer_ticks());

    if SCHEDULER_ENABLED.load(Ordering::Acquire) == 0
        || PREEMPTION_ENABLED.load(Ordering::Acquire) == 0
    {
        return;
    }

    if current.is_null() {
        return;
    }

    if current == idle_task {
        mark_preempt_if_ready(cpu_id);
        return;
    }

    if task_has_no_preempt_flag(current) {
        return;
    }

    if consume_time_slice(current) {
        return;
    }

    if scheduler_ready_count(cpu_id) == 0 {
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
        ctx.ds = SegmentSelector::USER_DATA.bits() as u64;
        ctx.es = SegmentSelector::USER_DATA.bits() as u64;
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

pub fn boot_step_task_manager_init() -> i32 {
    crate::task::init_task_manager()
}

pub fn boot_step_scheduler_init() -> i32 {
    init_scheduler()
}

pub fn boot_step_idle_task() -> i32 {
    create_idle_task()
}

pub fn init_scheduler_for_ap(cpu_id: usize) {
    per_cpu::init_percpu_scheduler(cpu_id);

    if create_idle_task_for_cpu(cpu_id) != 0 {
        klog_info!(
            "SCHED: Warning - failed to create idle task for CPU {}",
            cpu_id
        );
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
    use slopos_lib::arch::idt::RESCHEDULE_IPI_VECTOR;

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
