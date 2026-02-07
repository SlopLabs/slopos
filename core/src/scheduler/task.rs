use core::ffi::{c_char, c_int, c_void};
use core::mem;
use core::ptr;
use core::sync::atomic::Ordering;

use slopos_lib::IrqMutex;

use slopos_lib::cpu;
use slopos_lib::kdiag_timestamp;
use slopos_lib::string::cstr_to_str;
use slopos_lib::{klog_debug, klog_info};

// =============================================================================
// Zombie List for Deferred Task Reclamation
// =============================================================================

/// List of terminated tasks waiting to be freed when refcount hits zero.
/// Protected by IrqMutex for interrupt safety.
static ZOMBIE_LIST: slopos_lib::IrqMutex<ZombieList> = slopos_lib::IrqMutex::new(ZombieList::new());

struct ZombieList {
    tasks: [Option<*mut Task>; MAX_TASKS],
    count: usize,
}

// SAFETY: ZombieList contains raw pointers to Task slots which are static.
// All access is serialized through the IrqMutex.
unsafe impl Send for ZombieList {}
unsafe impl Sync for ZombieList {}

impl ZombieList {
    const fn new() -> Self {
        Self {
            tasks: [None; MAX_TASKS],
            count: 0,
        }
    }

    fn push(&mut self, task: *mut Task) {
        if self.count < MAX_TASKS {
            self.tasks[self.count] = Some(task);
            self.count += 1;
        }
    }
}

/// Add a terminated task to the zombie list for deferred cleanup.
/// The task will be freed when its reference count reaches zero.
fn defer_task_cleanup(task: *mut Task) {
    if task.is_null() {
        return;
    }
    ZOMBIE_LIST.lock().push(task);
}

/// Reap zombie tasks that are ready to be freed.
/// Should be called periodically (e.g., from scheduler idle path).
pub fn reap_zombies() {
    let mut list = ZOMBIE_LIST.lock();

    let mut write_idx = 0usize;
    for read_idx in 0..list.count {
        if let Some(task) = list.tasks[read_idx] {
            // Check if task is ready to be freed (refcount == 0)
            let ref_count = unsafe { (*task).ref_count() };
            if ref_count == 0 {
                // Safe to free now
                unsafe {
                    klog_debug!("reap_zombies: Freeing zombie task {}", (*task).task_id);

                    let kstack = (*task).kernel_stack_base;
                    let ustack = (*task).stack_base;

                    // Free kernel stack
                    if kstack != 0 {
                        kfree(kstack as *mut c_void);
                        (*task).kernel_stack_base = 0;
                    }

                    // Free user stack only if it differs from kernel stack
                    if (*task).process_id == INVALID_PROCESS_ID && ustack != 0 && ustack != kstack {
                        kfree(ustack as *mut c_void);
                        (*task).stack_base = 0;
                    }

                    // Mark slot as invalid for reuse
                    *task = Task::invalid();
                }
                // Don't keep this one in the list
                list.tasks[read_idx] = None;
            } else {
                // Still has references - keep in list
                if write_idx != read_idx {
                    list.tasks[write_idx] = list.tasks[read_idx];
                    list.tasks[read_idx] = None;
                }
                write_idx += 1;
            }
        }
    }
    list.count = write_idx;
}

use super::scheduler;

pub use slopos_abi::task::{
    BlockReason, FpuState, INVALID_PROCESS_ID, INVALID_TASK_ID, IdtEntry, MAX_TASKS,
    TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_KERNEL_MODE, TASK_FLAG_NO_PREEMPT,
    TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TASK_KERNEL_STACK_SIZE, TASK_NAME_MAX_LEN,
    TASK_PRIORITY_HIGH, TASK_PRIORITY_IDLE, TASK_PRIORITY_LOW, TASK_PRIORITY_NORMAL,
    TASK_STACK_SIZE, Task, TaskContext, TaskExitReason, TaskExitRecord, TaskFaultReason,
    TaskStatus,
};

use slopos_mm::mm_constants::PROCESS_CODE_START_VA;

use spin::Once;

pub type TaskIterateCb = Option<fn(*mut Task, *mut c_void)>;
pub type TaskEntry = fn(*mut c_void);

static VIDEO_CLEANUP_HOOK: Once<fn(u32)> = Once::new();

pub fn register_video_cleanup_hook(hook: fn(u32)) {
    VIDEO_CLEANUP_HOOK.call_once(|| hook);
}

fn video_task_cleanup(task_id: u32) {
    if let Some(hook) = VIDEO_CLEANUP_HOOK.get() {
        hook(task_id);
    }
}

struct TaskManagerInner {
    tasks: [Task; MAX_TASKS],
    num_tasks: u32,
    next_task_id: u32,
    total_context_switches: u64,
    total_yields: u64,
    tasks_created: u32,
    tasks_terminated: u32,
    exit_records: [TaskExitRecord; MAX_TASKS],
    initialized: bool,
}

// SAFETY: TaskManagerInner contains Task structs with raw pointers.
// All access is serialized through the mutex.
unsafe impl Send for TaskManagerInner {}

impl TaskManagerInner {
    const fn new() -> Self {
        Self {
            tasks: [const { Task::invalid() }; MAX_TASKS],
            num_tasks: 0,
            next_task_id: 1,
            total_context_switches: 0,
            total_yields: 0,
            tasks_created: 0,
            tasks_terminated: 0,
            exit_records: [TaskExitRecord::empty(); MAX_TASKS],
            initialized: false,
        }
    }
}

static TASK_MANAGER: IrqMutex<TaskManagerInner> = IrqMutex::new(TaskManagerInner::new());

use slopos_fs::fileio::{
    fileio_clone_table_for_process, fileio_create_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_mm::kernel_heap::{kfree, kmalloc};
use slopos_mm::process_vm::{
    create_process_vm, destroy_process_vm, process_vm_clone_cow, process_vm_get_page_dir,
    process_vm_get_stack_top,
};
use slopos_mm::shared_memory::shm_cleanup_task;

#[inline]
fn with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> R {
    let mut guard = TASK_MANAGER.lock();
    f(&mut guard)
}

#[inline]
fn try_with_task_manager<R>(f: impl FnOnce(&mut TaskManagerInner) -> R) -> Option<R> {
    let mut guard = TASK_MANAGER.lock();
    if guard.initialized {
        Some(f(&mut guard))
    } else {
        None
    }
}

pub fn task_find_by_id(task_id: u32) -> *mut Task {
    // INVALID_TASK_ID is used for uninitialized/invalid task slots - never return those
    if task_id == INVALID_TASK_ID {
        return ptr::null_mut();
    }

    with_task_manager(|mgr| {
        for task in mgr.tasks.iter_mut() {
            if task.task_id == task_id {
                return task as *mut Task;
            }
        }
        ptr::null_mut()
    })
}

fn release_task_dependents(completed_task_id: u32) {
    // Collect task pointers first, then try to wake outside the lock
    // This prevents holding the task manager lock during wake operations
    let candidates: [Option<*mut Task>; MAX_TASKS] = with_task_manager(|mgr| {
        let mut result = [None; MAX_TASKS];
        let mut idx = 0;
        for dependent in mgr.tasks.iter_mut() {
            // Skip non-blocked tasks
            if !task_is_blocked(dependent) {
                continue;
            }
            // Check if waiting on the completed task (atomic load)
            if dependent.waiting_on.load(Ordering::Acquire) != completed_task_id {
                continue;
            }
            // Candidate for wakeup - collect pointer
            result[idx] = Some(dependent as *mut Task);
            idx += 1;
        }
        result
    });

    // Now attempt to wake each candidate using the single-winner protocol
    // CAS ensures exactly one waker wins per task
    for candidate_opt in candidates.iter() {
        if let Some(task_ptr) = candidate_opt {
            let task = *task_ptr;
            let task_id = unsafe { (*task).task_id };

            if scheduler::try_wake_from_task_wait(task, completed_task_id) {
                klog_info!(
                    "release_task_dependents: Woke task {} (was waiting on {})",
                    task_id,
                    completed_task_id
                );
            }
            // If try_wake returns false, another waker won or task changed state
            // Either way, nothing more for us to do
        }
    }
}

fn user_entry_is_allowed(addr: u64) -> bool {
    // User tasks must start from filesystem-loaded ELF code in PROCESS_CODE_START_VA.
    const PROCESS_CODE_END: u64 = 0x0000_0000_0050_0000; // 1MB range
    addr >= PROCESS_CODE_START_VA && addr < PROCESS_CODE_END
}

fn task_slot_index_inner(mgr: &TaskManagerInner, task: *const Task) -> Option<usize> {
    if task.is_null() {
        return None;
    }
    let start = mgr.tasks.as_ptr() as usize;
    let idx = (task as usize - start) / mem::size_of::<Task>();
    if idx < MAX_TASKS { Some(idx) } else { None }
}

fn record_task_exit(
    task: *const Task,
    exit_reason: TaskExitReason,
    fault_reason: TaskFaultReason,
    exit_code: u32,
) {
    with_task_manager(|mgr| {
        if let Some(idx) = task_slot_index_inner(mgr, task) {
            mgr.exit_records[idx] = TaskExitRecord {
                task_id: unsafe { (*task).task_id },
                exit_reason,
                fault_reason,
                exit_code,
            };
        }
    });
}

fn init_task_context(task: &mut Task) {
    task.context = TaskContext::default();
    task.fpu_state = FpuState::new();
    task.context.rsi = task.entry_arg as u64;
    task.context.rdi = task.entry_point;
    task.context.rsp = task.stack_pointer;
    task.context.rflags = 0x202;

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        task.context.rip = task_entry_wrapper as *const () as usize as u64;
    } else {
        task.context.rip = task.entry_point;
    }

    if task.flags & TASK_FLAG_KERNEL_MODE != 0 {
        task.context.cs = 0x08;
        task.context.ds = 0x10;
        task.context.es = 0x10;
        task.context.fs = 0;
        task.context.gs = 0;
        task.context.ss = 0x10;
    } else {
        task.context.cs = 0x23;
        task.context.ds = 0x1B;
        task.context.es = 0x1B;
        task.context.fs = 0x1B;
        task.context.gs = 0x1B;
        task.context.ss = 0x1B;
        task.context.rdi = task.entry_arg as u64;
        task.context.rsi = 0;
    }

    task.context.cr3 = 0;
}

unsafe fn copy_name(dest: &mut [u8; TASK_NAME_MAX_LEN], src: *const c_char) {
    if src.is_null() {
        dest[0] = 0;
        return;
    }
    let mut i = 0;
    while i < TASK_NAME_MAX_LEN - 1 {
        let ch = unsafe { *src.add(i) };
        if ch == 0 {
            break;
        }
        dest[i] = ch as u8;
        i += 1;
    }
    dest[i] = 0;
    while i + 1 < TASK_NAME_MAX_LEN {
        i += 1;
        dest[i] = 0;
    }
}
pub fn init_task_manager() -> c_int {
    with_task_manager(|mgr| {
        mgr.total_context_switches = 0;
        mgr.total_yields = 0;
        mgr.tasks_created = 0;
        mgr.tasks_terminated = 0;

        let mut preserved_count = 0u32;
        let mut max_task_id = 0u32;
        for task in mgr.tasks.iter_mut() {
            let task_ptr = task as *mut Task;
            if crate::per_cpu::is_idle_task(task_ptr) {
                preserved_count += 1;
                if task.task_id > max_task_id {
                    max_task_id = task.task_id;
                }
                klog_debug!(
                    "init_task_manager: preserving idle task {} ('{}')",
                    task.task_id,
                    unsafe { cstr_to_str(task.name.as_ptr() as *const c_char) }
                );
                continue;
            }
            *task = Task::invalid();
        }
        for rec in mgr.exit_records.iter_mut() {
            *rec = TaskExitRecord::empty();
        }
        mgr.num_tasks = preserved_count;
        mgr.next_task_id = max_task_id + 1;
        mgr.initialized = true;
    });
    0
}
pub fn task_create(
    name: *const c_char,
    entry_point: TaskEntry,
    arg: *mut c_void,
    priority: u8,
    mut flags: u16,
) -> u32 {
    if entry_point as usize == 0 {
        klog_info!("task_create: Invalid entry point");
        return INVALID_TASK_ID;
    }

    if flags & TASK_FLAG_KERNEL_MODE == 0 && flags & TASK_FLAG_USER_MODE == 0 {
        flags |= TASK_FLAG_USER_MODE;
    }

    if flags & TASK_FLAG_KERNEL_MODE != 0 && flags & TASK_FLAG_USER_MODE != 0 {
        klog_info!("task_create: Conflicting mode flags");
        return INVALID_TASK_ID;
    }

    let (task, task_id) = with_task_manager(|mgr| {
        if mgr.num_tasks >= MAX_TASKS as u32 {
            klog_info!("task_create: Maximum tasks reached");
            return (ptr::null_mut(), INVALID_TASK_ID);
        }

        let task = {
            let mut found = ptr::null_mut();
            for t in mgr.tasks.iter_mut() {
                if t.status() == TaskStatus::Invalid {
                    found = t as *mut Task;
                    break;
                }
            }
            found
        };
        if task.is_null() {
            klog_info!("task_create: No free task slots");
            return (ptr::null_mut(), INVALID_TASK_ID);
        }

        if let Some(idx) = task_slot_index_inner(mgr, task) {
            mgr.exit_records[idx] = TaskExitRecord::empty();
        }

        let task_id = mgr.next_task_id;
        mgr.next_task_id = task_id.wrapping_add(1);

        (task, task_id)
    });

    if task.is_null() {
        return INVALID_TASK_ID;
    }

    let mut process_id = INVALID_PROCESS_ID;
    let stack_base;
    let kernel_stack_base;
    let kernel_stack_size;

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        let stack = kmalloc(TASK_STACK_SIZE as usize);
        if stack.is_null() {
            klog_info!("task_create: Failed to allocate kernel stack");
            return INVALID_TASK_ID;
        }
        stack_base = stack as u64;
        kernel_stack_base = stack_base;
        kernel_stack_size = TASK_STACK_SIZE;
    } else {
        process_id = create_process_vm();
        if process_id == INVALID_PROCESS_ID {
            klog_info!("task_create: Failed to create process VM");
            return INVALID_TASK_ID;
        }

        let stack_top = process_vm_get_stack_top(process_id);
        if stack_top == 0 {
            klog_info!("task_create: Failed to get process stack");
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }
        stack_base = stack_top - TASK_STACK_SIZE;

        let kstack = kmalloc(TASK_KERNEL_STACK_SIZE as usize);
        if kstack.is_null() {
            klog_info!("task_create: Failed to allocate kernel RSP0 stack");
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }

        kernel_stack_base = kstack as u64;
        kernel_stack_size = TASK_KERNEL_STACK_SIZE;

        if fileio_create_table_for_process(process_id) != 0 {
            kfree(kstack);
            destroy_process_vm(process_id);
            return INVALID_TASK_ID;
        }
    }

    let task_ref = unsafe { &mut *task };
    task_ref.task_id = task_id;
    unsafe { copy_name(&mut task_ref.name, name) };
    task_ref.set_status(TaskStatus::Ready);
    task_ref.priority = priority;
    task_ref.flags = flags;
    task_ref.process_id = process_id;
    task_ref.stack_base = stack_base;
    task_ref.stack_size = TASK_STACK_SIZE;
    task_ref.stack_pointer = stack_base + TASK_STACK_SIZE - 8;
    if flags & TASK_FLAG_USER_MODE != 0 && !user_entry_is_allowed(entry_point as u64) {
        klog_info!("task_create: user entry outside user_text window");
        if process_id != INVALID_PROCESS_ID {
            fileio_destroy_table_for_process(process_id);
            destroy_process_vm(process_id);
            if kernel_stack_base != 0 {
                kfree(kernel_stack_base as *mut c_void);
            }
        } else if kernel_stack_base != 0 {
            kfree(kernel_stack_base as *mut c_void);
        }
        *task_ref = Task::invalid();
        return INVALID_TASK_ID;
    }

    task_ref.kernel_stack_base = kernel_stack_base;
    task_ref.kernel_stack_top = kernel_stack_base + kernel_stack_size;
    task_ref.kernel_stack_size = kernel_stack_size;
    task_ref.entry_point = entry_point as usize as u64;
    task_ref.entry_arg = arg;
    task_ref.time_slice = 10;
    task_ref.time_slice_remaining = task_ref.time_slice;
    task_ref.total_runtime = 0;
    task_ref.creation_time = kdiag_timestamp();
    task_ref.yield_count = 0;
    task_ref.last_run_timestamp = 0;
    task_ref
        .waiting_on
        .store(INVALID_TASK_ID, Ordering::Release);
    task_ref.user_started = 0;
    task_ref.context_from_user = 0;
    task_ref.exit_reason = TaskExitReason::None;
    task_ref.fault_reason = TaskFaultReason::None;
    task_ref.exit_code = 0;
    task_ref.fate_token = 0;
    task_ref.fate_value = 0;
    task_ref.fate_pending = 0;
    task_ref.next_ready = ptr::null_mut();
    task_ref
        .next_inbox
        .store(ptr::null_mut(), Ordering::Release);
    task_ref.refcnt.store(0, Ordering::Release);

    init_task_context(task_ref);

    if flags & TASK_FLAG_KERNEL_MODE != 0 {
        task_ref.context.cr3 = cpu::read_cr3() & !0xFFF;
    } else {
        let page_dir = process_vm_get_page_dir(process_id);
        if !page_dir.is_null() {
            task_ref.context.cr3 = unsafe { (*page_dir).pml4_phys.as_u64() };
        }
    }

    with_task_manager(|mgr| {
        mgr.num_tasks = mgr.num_tasks.saturating_add(1);
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });

    klog_debug!(
        "Created task '{}' with ID {}",
        unsafe { cstr_to_str(task_ref.name.as_ptr() as *const c_char) },
        task_id
    );

    task_id
}
pub fn task_terminate(task_id: u32) -> c_int {
    let mut resolved_id = task_id;
    let task_ptr: *mut Task;

    if task_id == u32::MAX {
        task_ptr = scheduler::scheduler_get_current_task();
        if task_ptr.is_null() {
            klog_info!("task_terminate: No current task to terminate");
            return -1;
        }
        resolved_id = unsafe { (*task_ptr).task_id };
    } else {
        task_ptr = task_find_by_id(task_id);
    }

    if task_ptr.is_null() || unsafe { (*task_ptr).status() } == TaskStatus::Invalid {
        klog_info!("task_terminate: Task not found");
        return -1;
    }

    klog_info!(
        "Terminating task '{}' (ID {})",
        unsafe { cstr_to_str((*task_ptr).name.as_ptr() as *const c_char) },
        resolved_id
    );

    let is_current = task_ptr == scheduler::scheduler_get_current_task();

    let now = kdiag_timestamp();
    unsafe {
        if (*task_ptr).last_run_timestamp != 0 && now >= (*task_ptr).last_run_timestamp {
            (*task_ptr).total_runtime += now - (*task_ptr).last_run_timestamp;
        }
        (*task_ptr).last_run_timestamp = 0;
        if (*task_ptr).exit_reason == TaskExitReason::None {
            (*task_ptr).exit_reason = TaskExitReason::Kernel;
        }
        record_task_exit(
            task_ptr,
            (*task_ptr).exit_reason,
            (*task_ptr).fault_reason,
            (*task_ptr).exit_code,
        );
        (*task_ptr).set_status(TaskStatus::Terminated);
        scheduler::cancel_sleep(resolved_id);
        (*task_ptr).fate_token = 0;
        (*task_ptr).fate_value = 0;
        (*task_ptr).fate_pending = 0;
    }

    // Clear our own waiting_on (we're done waiting on anything)
    unsafe {
        (*task_ptr)
            .waiting_on
            .store(INVALID_TASK_ID, Ordering::Release);
    }

    scheduler::unschedule_task(task_ptr);

    // Memory barrier: ensure TERMINATED state is visible before we wake dependents
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    // NO MORE pause_all_aps()!
    // The atomic CAS protocol in release_task_dependents handles races safely.
    release_task_dependents(resolved_id);

    if !is_current {
        unsafe {
            if (*task_ptr).process_id != INVALID_PROCESS_ID {
                // Clean up process-specific resources immediately
                // (These don't depend on task memory being valid)
                fileio_destroy_table_for_process((*task_ptr).process_id);
                video_task_cleanup(resolved_id);
                // Clean up shared memory buffers owned by this task
                // Must happen before destroy_process_vm to properly unmap pages
                shm_cleanup_task(resolved_id);
                destroy_process_vm((*task_ptr).process_id);
            }

            // Defer memory cleanup until refcount is zero
            // This allows other CPUs to safely finish using task pointers
            // (e.g., in remote inbox or other queues)
            if (*task_ptr).ref_count() > 0 {
                defer_task_cleanup(task_ptr);
            } else {
                // No references - safe to free immediately
                if (*task_ptr).kernel_stack_base != 0 {
                    kfree((*task_ptr).kernel_stack_base as *mut c_void);
                }
                // For kernel tasks, stack_base == kernel_stack_base (same allocation).
                // Only free stack_base separately if it differs from kernel_stack_base.
                if (*task_ptr).process_id == INVALID_PROCESS_ID
                    && (*task_ptr).stack_base != 0
                    && (*task_ptr).stack_base != (*task_ptr).kernel_stack_base
                {
                    kfree((*task_ptr).stack_base as *mut c_void);
                }
                *task_ptr = Task::invalid();
            }
        }
    }

    with_task_manager(|mgr| {
        if !is_current && mgr.num_tasks > 0 {
            mgr.num_tasks -= 1;
        }
        mgr.tasks_terminated = mgr.tasks_terminated.saturating_add(1);
    });

    0
}

pub fn task_shutdown_all() -> c_int {
    let was_paused = crate::per_cpu::pause_all_aps();

    let mut result = 0;
    let current = scheduler::scheduler_get_current_task();

    let tasks_to_terminate: [Option<u32>; MAX_TASKS] = with_task_manager(|mgr| {
        let mut ids = [None; MAX_TASKS];
        for (i, task) in mgr.tasks.iter().enumerate() {
            if task.status() == TaskStatus::Invalid {
                continue;
            }
            let task_ptr = &mgr.tasks[i] as *const Task as *mut Task;
            if task_ptr == current {
                continue;
            }
            if crate::per_cpu::is_idle_task(task_ptr) {
                continue;
            }
            if task.task_id == INVALID_TASK_ID {
                continue;
            }
            ids[i] = Some(task.task_id);
        }
        ids
    });

    for id_opt in tasks_to_terminate.iter() {
        if let Some(task_id) = id_opt {
            if task_terminate(*task_id) != 0 {
                result = -1;
            }
        }
    }

    crate::per_cpu::clear_all_cpu_queues();

    with_task_manager(|mgr| {
        let mut preserved = 0u32;
        for task in mgr.tasks.iter() {
            let s = task.status();
            if s != TaskStatus::Invalid && s != TaskStatus::Terminated {
                preserved += 1;
            }
        }
        mgr.num_tasks = preserved;
    });

    crate::per_cpu::resume_all_aps_if_not_nested(was_paused);
    result
}

pub fn task_get_info(task_id: u32, task_info: *mut *mut Task) -> c_int {
    if task_info.is_null() {
        return -1;
    }
    let task = task_find_by_id(task_id);
    unsafe {
        if task.is_null() || (*task).status() == TaskStatus::Invalid {
            *task_info = ptr::null_mut();
            return -1;
        }
        *task_info = task;
    }
    0
}

pub fn task_get_exit_record(task_id: u32, record_out: *mut TaskExitRecord) -> c_int {
    if record_out.is_null() {
        return -1;
    }
    with_task_manager(|mgr| {
        for rec in mgr.exit_records.iter() {
            if rec.task_id == task_id {
                unsafe { *record_out = *rec };
                return 0;
            }
        }
        -1
    })
}

pub fn task_set_state(task_id: u32, new_status: TaskStatus) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }

    let task_ref = unsafe { &*task };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    if task_ref.try_transition_to(new_status) {
        0
    } else {
        -1
    }
}

pub fn task_set_state_with_reason(
    task_id: u32,
    new_status: TaskStatus,
    reason: BlockReason,
) -> c_int {
    let task = task_find_by_id(task_id);
    if task.is_null() {
        return -1;
    }

    let task_ref = unsafe { &mut *task };
    if task_ref.status() == TaskStatus::Invalid {
        return -1;
    }

    match new_status {
        TaskStatus::Ready => {
            if task_ref.mark_ready() {
                0
            } else {
                -1
            }
        }
        TaskStatus::Running => {
            if task_ref.mark_running() {
                0
            } else {
                -1
            }
        }
        TaskStatus::Blocked => {
            if task_ref.block(reason) {
                0
            } else {
                -1
            }
        }
        TaskStatus::Terminated => {
            if task_ref.terminate() {
                0
            } else {
                -1
            }
        }
        TaskStatus::Invalid => -1,
    }
}
pub fn get_task_stats(total_tasks: *mut u32, active_tasks: *mut u32, context_switches: *mut u64) {
    with_task_manager(|mgr| {
        if !total_tasks.is_null() {
            unsafe { *total_tasks = mgr.tasks_created };
        }
        if !active_tasks.is_null() {
            unsafe { *active_tasks = mgr.num_tasks };
        }
        if !context_switches.is_null() {
            unsafe { *context_switches = mgr.total_context_switches };
        }
    });
}

pub fn task_record_context_switch(from: *mut Task, to: *mut Task, timestamp: u64) {
    if !from.is_null() {
        unsafe {
            if (*from).last_run_timestamp != 0 && timestamp >= (*from).last_run_timestamp {
                (*from).total_runtime += timestamp - (*from).last_run_timestamp;
            }
            (*from).last_run_timestamp = 0;
        }
    }

    if !to.is_null() {
        unsafe { (*to).last_run_timestamp = timestamp };
    }

    if !to.is_null() && to != from {
        with_task_manager(|mgr| {
            mgr.total_context_switches += 1;
        });
    }
}

pub fn task_record_yield(task: *mut Task) {
    with_task_manager(|mgr| {
        mgr.total_yields += 1;
    });
    if !task.is_null() {
        unsafe { (*task).yield_count = (*task).yield_count.saturating_add(1) };
    }
}

pub fn task_get_total_yields() -> u64 {
    try_with_task_manager(|mgr| mgr.total_yields).unwrap_or(0)
}

pub fn task_state_to_string(status: TaskStatus) -> *const c_char {
    match status {
        TaskStatus::Invalid => b"invalid\0".as_ptr() as *const c_char,
        TaskStatus::Ready => b"ready\0".as_ptr() as *const c_char,
        TaskStatus::Running => b"running\0".as_ptr() as *const c_char,
        TaskStatus::Blocked => b"blocked\0".as_ptr() as *const c_char,
        TaskStatus::Terminated => b"terminated\0".as_ptr() as *const c_char,
    }
}

pub fn task_iterate_active(callback: TaskIterateCb, context: *mut c_void) {
    if callback.is_none() {
        return;
    }
    let cb = callback.unwrap();

    let task_ptrs: [Option<*mut Task>; MAX_TASKS] = with_task_manager(|mgr| {
        let mut ptrs = [None; MAX_TASKS];
        for (i, task) in mgr.tasks.iter_mut().enumerate() {
            if task.status() != TaskStatus::Invalid && task.task_id != INVALID_TASK_ID {
                ptrs[i] = Some(task as *mut Task);
            }
        }
        ptrs
    });

    for ptr_opt in task_ptrs.iter() {
        if let Some(task) = ptr_opt {
            cb(*task, context);
        }
    }
}
pub fn task_get_current_id() -> u32 {
    let current = scheduler::scheduler_get_current_task();
    if current.is_null() {
        0
    } else {
        unsafe { (*current).task_id }
    }
}
pub fn task_get_current() -> *mut Task {
    scheduler::scheduler_get_current_task()
}
pub fn task_set_current(task: *mut Task) {
    if task.is_null() {
        return;
    }
    unsafe {
        let current_status = (*task).status();
        if current_status != TaskStatus::Ready && current_status != TaskStatus::Running {
            klog_info!(
                "task_set_current: unexpected state {} for task {} ('{}')",
                current_status.as_u8() as u32,
                (*task).task_id,
                cstr_to_str((*task).name.as_ptr() as *const c_char)
            );
        }
        (*task).set_status(TaskStatus::Running);
    }
}
pub fn task_get_state(task: *const Task) -> TaskStatus {
    if task.is_null() {
        return TaskStatus::Invalid;
    }
    unsafe { (*task).status() }
}
pub fn task_is_ready(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Ready
}
pub fn task_is_running(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Running
}
pub fn task_is_blocked(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Blocked
}
pub fn task_is_terminated(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Terminated
}
pub fn task_is_invalid(task: *const Task) -> bool {
    task_get_state(task) == TaskStatus::Invalid
}

pub fn task_fork(parent_task: *mut Task) -> u32 {
    if parent_task.is_null() {
        klog_info!("task_fork: null parent task");
        return INVALID_TASK_ID;
    }

    let parent = unsafe { &*parent_task };

    if parent.process_id == INVALID_PROCESS_ID {
        klog_info!("task_fork: parent has no process VM (kernel task?)");
        return INVALID_TASK_ID;
    }

    if parent.flags & TASK_FLAG_KERNEL_MODE != 0 {
        klog_info!("task_fork: cannot fork kernel-mode task");
        return INVALID_TASK_ID;
    }

    let child_process_id = process_vm_clone_cow(parent.process_id);
    if child_process_id == INVALID_PROCESS_ID {
        klog_info!("task_fork: process_vm_clone_cow failed");
        return INVALID_TASK_ID;
    }

    let child_kernel_stack = kmalloc(TASK_KERNEL_STACK_SIZE as usize);
    if child_kernel_stack.is_null() {
        klog_info!("task_fork: failed to allocate kernel stack");
        destroy_process_vm(child_process_id);
        return INVALID_TASK_ID;
    }

    if fileio_clone_table_for_process(parent.process_id, child_process_id) != 0 {
        klog_info!("task_fork: failed to clone file table");
        kfree(child_kernel_stack);
        destroy_process_vm(child_process_id);
        return INVALID_TASK_ID;
    }

    let (child_task_ptr, child_task_id) = with_task_manager(|mgr| {
        if mgr.num_tasks >= MAX_TASKS as u32 {
            return (ptr::null_mut(), INVALID_TASK_ID);
        }

        let mut slot: *mut Task = ptr::null_mut();
        for t in mgr.tasks.iter_mut() {
            if t.status() == TaskStatus::Invalid {
                slot = t as *mut Task;
                break;
            }
        }

        if slot.is_null() {
            return (ptr::null_mut(), INVALID_TASK_ID);
        }

        let task_id = mgr.next_task_id;
        mgr.next_task_id = task_id.wrapping_add(1);

        if let Some(idx) = task_slot_index_inner(mgr, slot) {
            mgr.exit_records[idx] = TaskExitRecord::empty();
        }

        (slot, task_id)
    });

    if child_task_ptr.is_null() {
        klog_info!("task_fork: no free task slots");
        fileio_destroy_table_for_process(child_process_id);
        kfree(child_kernel_stack);
        destroy_process_vm(child_process_id);
        return INVALID_TASK_ID;
    }

    let child = unsafe { &mut *child_task_ptr };

    child.clone_from(parent);

    child.task_id = child_task_id;
    child.process_id = child_process_id;
    child.set_status(TaskStatus::Ready);

    child.kernel_stack_base = child_kernel_stack as u64;
    child.kernel_stack_top = child_kernel_stack as u64 + TASK_KERNEL_STACK_SIZE;
    child.kernel_stack_size = TASK_KERNEL_STACK_SIZE;

    child.context.rax = 0;

    let child_page_dir = process_vm_get_page_dir(child_process_id);
    if !child_page_dir.is_null() {
        child.context.cr3 = unsafe { (*child_page_dir).pml4_phys.as_u64() };
    }

    child.time_slice_remaining = child.time_slice;
    child.total_runtime = 0;
    child.creation_time = kdiag_timestamp();
    child.yield_count = 0;
    child.last_run_timestamp = 0;
    child.waiting_on.store(INVALID_TASK_ID, Ordering::Release);
    child.exit_reason = TaskExitReason::None;
    child.fault_reason = TaskFaultReason::None;
    child.exit_code = 0;
    child.fate_token = 0;
    child.fate_value = 0;
    child.fate_pending = 0;
    child.next_ready = ptr::null_mut();
    child.next_inbox.store(ptr::null_mut(), Ordering::Release);
    child.refcnt.store(0, Ordering::Release);

    with_task_manager(|mgr| {
        mgr.num_tasks = mgr.num_tasks.saturating_add(1);
        mgr.tasks_created = mgr.tasks_created.saturating_add(1);
    });

    klog_info!(
        "task_fork: created child task {} (process {}) from parent task {} (process {})",
        child_task_id,
        child_process_id,
        parent.task_id,
        parent.process_id
    );

    child_task_id
}

pub unsafe fn task_manager_force_unlock() {
    unsafe { TASK_MANAGER.force_unlock() };
}

use super::ffi_boundary::task_entry_wrapper;
