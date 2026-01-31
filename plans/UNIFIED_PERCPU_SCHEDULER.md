# Unified Per-CPU Scheduler Architecture

**Status**: Planned  
**Priority**: High  
**Estimated Effort**: 5-7 days  
**Prerequisite**: Lock-free task termination (implemented in a00c5fe)  
**Author**: AI Analysis based on Theseus OS, Redox OS, Linux CFS architectures  
**Date**: 2026-01-31  
**Revision**: 2.0 (Gold Standard Edition)

---

## Table of Contents

1. [Executive Summary](#executive-summary)
2. [Problem Analysis](#problem-analysis)
3. [Gold Standard Architecture](#gold-standard-architecture)
4. [Design Principles](#design-principles)
5. [Data Structures](#data-structures)
6. [Implementation Phases](#implementation-phases)
7. [Critical Implementation Details](#critical-implementation-details)
8. [Migration Strategy](#migration-strategy)
9. [Testing Strategy](#testing-strategy)
10. [Success Criteria](#success-criteria)
11. [Risks and Mitigations](#risks-and-mitigations)
12. [Future Extensions](#future-extensions)
13. [References](#references)

---

## Executive Summary

The current SlopOS scheduler has an **asymmetric architecture** where CPU 0 (BSP) operates fundamentally differently from Application Processors (APs). This prevents proper use of lock-free cross-CPU scheduling primitives and requires a "quickfix" that introduces mutex contention.

This plan unifies all CPUs under a **symmetric per-CPU scheduler model**, following patterns established by production Rust operating systems (Theseus OS, Redox OS) and informed by Linux's per-CPU runqueue design.

### Key Outcomes

| Before | After |
|--------|-------|
| BSP uses global `SchedulerInner` | All CPUs use identical `PerCpuScheduler` |
| BSP halts after init, no loop | All CPUs run `scheduler_loop()` |
| Cross-CPU wakes use mutex | Cross-CPU wakes use lock-free MPSC inbox |
| Special-case code paths | Symmetric, unified code |
| ~1ms cross-CPU wake latency | <10μs cross-CPU wake latency |

---

## Problem Analysis

### Current Architecture (Asymmetric)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                           CPU 0 (BSP)                                    │
├─────────────────────────────────────────────────────────────────────────┤
│  Global SchedulerInner (scheduler.rs:44)                                 │
│  ├── ready_queues[4]          ← Duplicated state (also in PerCpu!)      │
│  ├── current_task             ← Only valid for CPU 0                    │
│  ├── idle_task                ← Only valid for CPU 0                    │
│  └── return_context           ← Only valid for CPU 0                    │
│                                                                          │
│  Execution Model:                                                        │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ start_scheduler()                                                │    │
│  │   ├── context_switch(boot_stack → idle_task)                    │    │
│  │   └── loop { hlt }   ← NO SCHEDULER LOOP, just halts!           │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  schedule() called ONLY by:                                              │
│    • Timer tick (preemption)                                            │
│    • Task yield/block/exit                                              │
│                                                                          │
│  ❌ Remote inbox NEVER drained                                          │
│  ❌ push_remote_wake() to CPU 0 = task stuck forever                   │
│  ❌ Requires quickfix: enqueue_local() with mutex for ALL cross-CPU    │
└─────────────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────────────┐
│                           APs (CPU 1+)                                   │
├─────────────────────────────────────────────────────────────────────────┤
│  PerCpuScheduler (per_cpu.rs:156)                                        │
│  ├── ready_queues[4]                                                    │
│  ├── current_task_atomic                                                │
│  ├── idle_task_atomic                                                   │
│  ├── return_context                                                     │
│  └── remote_inbox_head (lock-free MPSC Treiber stack)                   │
│                                                                          │
│  Execution Model:                                                        │
│  ┌─────────────────────────────────────────────────────────────────┐    │
│  │ scheduler_run_ap()                                               │    │
│  │   ├── Switch from boot stack to idle task's kernel stack       │    │
│  │   └── ap_scheduler_loop() {                    ← PROPER LOOP    │    │
│  │         drain_remote_inbox();                                    │    │
│  │         next = dequeue_highest_priority();                       │    │
│  │         if next { execute_task(); }                              │    │
│  │         else { try_work_steal(); hlt; }                          │    │
│  │       }                                                          │    │
│  └─────────────────────────────────────────────────────────────────┘    │
│                                                                          │
│  ✓ Remote inbox drained every loop iteration                            │
│  ✓ Lock-free cross-CPU wakeups work correctly                           │
└─────────────────────────────────────────────────────────────────────────┘
```

### Root Cause: Historical Evolution

The asymmetry exists because:
1. BSP scheduler was written first (single-CPU design)
2. AP support was added later with proper per-CPU design
3. BSP was never retrofitted to use the per-CPU model
4. Result: Two incompatible scheduler architectures coexisting

### The Quickfix (Current Workaround)

Location: `core/src/scheduler/scheduler.rs:321`

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    let target_cpu = per_cpu::select_target_cpu(task);
    
    // QUICKFIX: Always use enqueue_local() with mutex instead of
    // push_remote_wake() because CPU 0 never drains its inbox.
    let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| {
        sched.enqueue_local(task)  // Takes queue_lock mutex!
    });
    // ...
}
```

**Problems with quickfix:**
- Mutex contention on hot path
- Cache line bouncing between CPUs
- Serializes cross-CPU scheduling
- O(n) latency under contention

---

## Gold Standard Architecture

### Reference: Theseus OS

Theseus uses CPU-local storage with a pluggable scheduler trait:

```rust
// CPU-local scheduler reference (no global state)
#[cls::cpu_local]
static SCHEDULER: Option<Arc<ConcurrentScheduler>> = None;

// Unified schedule() for ALL CPUs
pub fn schedule() -> bool {
    let preemption_guard = preemption::hold_preemption();
    let cpu_id = preemption_guard.cpu_id();
    
    // SAME code path regardless of CPU
    let next_task = SCHEDULER.update_guarded(
        |scheduler| scheduler.as_ref().unwrap().lock().next(),
        &preemption_guard,
    );
    
    task_switch(next_task, cpu_id, preemption_guard)
}

// Pluggable scheduler policy
pub trait Scheduler: Send + Sync + 'static {
    fn next(&mut self) -> TaskRef;
    fn add(&mut self, task: TaskRef);
    fn remove(&mut self, task: &TaskRef) -> bool;
    fn busyness(&self) -> usize;
    fn drain(&mut self) -> Box<dyn Iterator<Item = TaskRef> + '_>;
}
```

**Key insights:**
- No global scheduler state
- CPU-local storage for per-CPU scheduler
- Identical code path for all CPUs
- Trait-based pluggable scheduler policies

### Reference: Redox OS

Redox uses per-CPU context switch state:

```rust
pub struct ContextSwitchPercpu {
    switch_result: Cell<Option<SwitchResultInner>>,
    switch_time: Cell<u128>,
    pit_ticks: Cell<usize>,
    current_ctxt: RefCell<Option<Arc<ContextLock>>>,
    idle_ctxt: RefCell<Option<Arc<ContextLock>>>,  // Per-CPU idle
}

// Unified switch() for ALL CPUs
pub fn switch(token: &mut CleanLockToken) -> SwitchResult {
    let percpu = PercpuBlock::current();  // CPU-local via GS segment
    
    // Round-robin through contexts, SAME logic for all CPUs
    for next_context_lock in contexts.range(...) {
        if update_runnable(&mut next_context, cpu_id) == CanSwitch {
            // Found runnable context, switch to it
            arch::switch_to(prev_context, next_context);
            return SwitchResult::Switched;
        }
    }
    
    SwitchResult::AllContextsIdle
}
```

**Key insights:**
- Per-CPU idle context (not global)
- Per-CPU switch state
- Symmetric switch() function
- Uses percpu block accessed via GS segment (similar to SlopOS PCR)

### Target Architecture (SlopOS Gold Standard)

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     ALL CPUs (Unified Architecture)                      │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │                    PerCpuScheduler (per CPU)                       │  │
│  ├───────────────────────────────────────────────────────────────────┤  │
│  │  cpu_id: usize                                                     │  │
│  │  ready_queues: [ReadyQueue; NUM_PRIORITIES]                        │  │
│  │  current_task: AtomicPtr<Task>                                     │  │
│  │  idle_task: AtomicPtr<Task>                                        │  │
│  │  return_context: TaskContext      ← Saved scheduler loop context  │  │
│  │  remote_inbox_head: AtomicPtr<Task>  ← Lock-free MPSC inbox       │  │
│  │  stats: SchedulerStats                                             │  │
│  │  policy: Box<dyn SchedulerPolicy>    ← Future: pluggable policy   │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  Execution Model (IDENTICAL for all CPUs):                               │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ enter_scheduler_loop(cpu_id)                                       │  │
│  │   ├── Transition from boot stack to idle task's kernel stack     │  │
│  │   └── scheduler_loop(cpu_id) {                                    │  │
│  │         loop {                                                     │  │
│  │           drain_remote_inbox();        // Lock-free               │  │
│  │           if let Some(task) = select_next_task() {                │  │
│  │             execute_task(task);        // Returns on yield/block  │  │
│  │           } else {                                                 │  │
│  │             try_work_steal() || halt_until_interrupt();           │  │
│  │           }                                                        │  │
│  │         }                                                          │  │
│  │       }                                                            │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  Cross-CPU Wake Protocol:                                                │
│  ┌───────────────────────────────────────────────────────────────────┐  │
│  │ schedule_task(task, target_cpu):                                   │  │
│  │   if target_cpu == current_cpu:                                    │  │
│  │     enqueue_local(task)              // No lock needed            │  │
│  │   else:                                                            │  │
│  │     push_remote_wake(target_cpu, task)  // Lock-free CAS          │  │
│  │     send_reschedule_ipi(target_cpu)     // Wake from hlt          │  │
│  └───────────────────────────────────────────────────────────────────┘  │
│                                                                          │
│  ✓ Symmetric architecture - all CPUs identical                          │
│  ✓ Lock-free cross-CPU wakeups - O(1) latency                           │
│  ✓ Per-CPU isolation - no shared mutable state on hot path             │
│  ✓ Extensible - pluggable scheduler policies                           │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Design Principles

### 1. Symmetry First

Every CPU runs identical code. No `if cpu_id == 0` special cases in the scheduler hot path.

```rust
// WRONG - Asymmetric
pub fn schedule() {
    if get_current_cpu() == 0 {
        bsp_schedule();  // Different path for BSP
    } else {
        ap_schedule();   // Different path for APs
    }
}

// CORRECT - Symmetric
pub fn schedule() {
    let cpu_id = get_current_cpu();
    with_local_scheduler(|sched| {
        sched.schedule()  // Same code for all CPUs
    })
}
```

### 2. Per-CPU Isolation

No shared mutable state on the scheduling hot path. Each CPU owns its scheduler state.

```rust
// WRONG - Global state
static SCHEDULER: Mutex<Scheduler> = ...;  // Contention!

// CORRECT - Per-CPU state
static CPU_SCHEDULERS: [PerCpuScheduler; MAX_CPUS] = ...;
// Each CPU only mutates its own scheduler (via cpu_id index)
```

### 3. Lock-Free Cross-CPU Communication

Use lock-free data structures for cross-CPU task wakeups.

```rust
// WRONG - Mutex for cross-CPU
target_cpu.scheduler.lock().enqueue(task);  // Contention!

// CORRECT - Lock-free MPSC
target_cpu.scheduler.push_remote_wake(task);  // CAS, no lock
send_ipi(target_cpu);  // Wake from hlt
```

### 4. Boot Stack Transition

All CPUs must transition from transient boot stack to persistent idle task stack before entering the scheduler loop.

```rust
// Boot sequence for ANY CPU:
fn enter_scheduler(cpu_id: usize) -> ! {
    // 1. Get idle task's kernel stack
    let idle_task = get_cpu_idle_task(cpu_id);
    let idle_stack_top = idle_task.kernel_stack_top;
    
    // 2. Switch RSP from boot stack to idle stack
    //    CRITICAL: Boot stack is HHDM-mapped, will cause crashes if saved
    unsafe {
        asm!("mov rsp, {}", "mov rbp, rsp", in(reg) idle_stack_top);
    }
    
    // 3. Now safe to enter scheduler loop (stack is persistent)
    scheduler_loop(cpu_id)
}
```

### 5. Return-to-Loop Scheduling

When a task yields/blocks/exits, it returns to the scheduler loop (not inline switching).

```rust
// Task execution model:
//
// scheduler_loop():
//   ┌─────────────────────────────────────────┐
//   │  loop {                                  │
//   │    drain_inbox();                        │
//   │    task = select_next();                 │
//   │    execute_task(task); ───────┐          │
//   │  }                            │          │
//   └───────────────────────────────┼──────────┘
//                                   │
//   execute_task(task):             │
//   ┌───────────────────────────────▼──────────┐
//   │  context_switch(idle_ctx, task_ctx);     │
//   │  // ... task runs ...                    │
//   │  // task calls schedule() or exits       │
//   │  // context_switch back to idle_ctx      │
//   │  return; ← returns to scheduler_loop     │
//   └──────────────────────────────────────────┘
```

---

## Data Structures

### PerCpuScheduler (Refined)

```rust
/// Per-CPU scheduler state.
/// 
/// Each CPU has exactly one instance. All fields are either:
/// - Only accessed by the owning CPU (no sync needed), or
/// - Atomic for lock-free cross-CPU access
#[repr(C, align(64))]  // Cache line aligned to prevent false sharing
pub struct PerCpuScheduler {
    // === Identity ===
    /// CPU index (0 = BSP, 1+ = APs)
    pub cpu_id: usize,
    
    // === Run Queues ===
    /// Priority-indexed ready queues (lower index = higher priority)
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    /// Spinlock protecting ready_queues (only for local CPU access)
    queue_lock: Mutex<()>,
    
    // === Current Execution State ===
    /// Currently executing task (null if idle)
    current_task: AtomicPtr<Task>,
    /// Idle task for this CPU (never null after init)
    idle_task: AtomicPtr<Task>,
    /// Flag: are we currently inside execute_task()?
    executing_task: AtomicBool,
    
    // === Scheduler Loop Context ===
    /// Saved context of scheduler_loop() for return-to-loop switching
    /// When a task yields, we switch to this context to return to the loop.
    pub loop_context: TaskContext,
    
    // === Lock-Free Remote Wake Inbox ===
    /// Head of Treiber stack for cross-CPU task wakes (MPSC)
    /// Other CPUs push here; owning CPU drains.
    remote_inbox_head: AtomicPtr<Task>,
    /// Approximate inbox count (informational, may drift)
    inbox_count: AtomicU32,
    
    // === Statistics ===
    pub stats: SchedulerStats,
    
    // === Control ===
    /// Is this scheduler enabled?
    enabled: AtomicBool,
    /// Has this scheduler been initialized?
    initialized: AtomicBool,
    /// Default time slice in ticks
    pub time_slice: u16,
}

#[derive(Default)]
pub struct SchedulerStats {
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_ticks: AtomicU64,
    pub inbox_drains: AtomicU64,
    pub work_steals: AtomicU64,
}
```

### Task Inbox Linkage

```rust
// In abi/src/task.rs - Task struct additions for lock-free inbox

pub struct Task {
    // ... existing fields ...
    
    /// Intrusive link for remote wake inbox (Treiber stack)
    /// Only valid when task is in a remote inbox.
    /// Uses AtomicPtr for lock-free push.
    pub next_inbox: AtomicPtr<Task>,
    
    /// Intrusive link for ready queue
    /// Only valid when task is in a ready queue.
    pub next_ready: *mut Task,
}
```

### ReadyQueue (Per-Priority)

```rust
/// Single-priority ready queue (intrusive linked list)
struct ReadyQueue {
    head: *mut Task,
    tail: *mut Task,
    count: AtomicU32,
}

impl ReadyQueue {
    /// Enqueue at tail (O(1))
    fn enqueue(&mut self, task: *mut Task) -> i32;
    
    /// Dequeue from head (O(1))
    fn dequeue(&mut self) -> *mut Task;
    
    /// Remove specific task (O(n) but rare)
    fn remove(&mut self, task: *mut Task) -> i32;
    
    /// Steal from tail for work stealing (O(n) traversal)
    fn steal_from_tail(&mut self) -> Option<*mut Task>;
}
```

---

## Implementation Phases

### Phase 0: Immediate Win (No Architecture Change)
**Effort**: 2 hours  
**Risk**: Very Low  
**Benefit**: Enables lock-free wakes to CPU 0 immediately

Add inbox drain to timer tick handler for CPU 0:

```rust
// In timer tick handler (drivers/src/irq.rs or similar)
pub fn timer_tick_handler() {
    let cpu_id = get_current_cpu();
    
    // Drain inbox on EVERY timer tick (all CPUs)
    per_cpu::with_cpu_scheduler(cpu_id, |sched| {
        sched.drain_remote_inbox();
    });
    
    // ... existing preemption logic ...
}
```

**Why this works**: Even without the unified loop, CPU 0 will process its inbox every ~1ms (timer tick frequency). This immediately enables `push_remote_wake()` for cross-CPU scheduling.

**Verification**:
```bash
# Before: grep for quickfix
grep -n "enqueue_local" core/src/scheduler/scheduler.rs

# After: Can replace with push_remote_wake() for cross-CPU
```

---

### Phase 1: State Consolidation
**Effort**: 4 hours  
**Risk**: Low

Ensure all scheduling state is in `PerCpuScheduler`, eliminate `SchedulerInner` duplication.

#### 1.1 Audit State Locations

| Field | SchedulerInner | PerCpuScheduler | Action |
|-------|---------------|-----------------|--------|
| `ready_queues` | Yes | Yes | Remove from SchedulerInner |
| `current_task` | Yes (raw ptr) | Yes (AtomicPtr) | Remove from SchedulerInner |
| `idle_task` | Yes (raw ptr) | Yes (AtomicPtr) | Remove from SchedulerInner |
| `return_context` | Yes | Yes (`loop_context`) | Remove from SchedulerInner |
| `policy` | Yes | No | Move to PerCpuScheduler |
| `enabled` | Yes (u8) | Yes (AtomicBool) | Remove from SchedulerInner |
| `time_slice` | Yes | Yes | Remove from SchedulerInner |

#### 1.2 Create Compatibility Shim

```rust
// Temporary shim for code still using with_scheduler()
pub fn with_scheduler<F, R>(f: F) -> R 
where 
    F: FnOnce(&mut SchedulerCompat) -> R 
{
    // Delegate to CPU 0's per-CPU scheduler with compatibility wrapper
    with_cpu_scheduler(0, |percpu| {
        let mut compat = SchedulerCompat::new(percpu);
        f(&mut compat)
    }).expect("CPU 0 scheduler must be initialized")
}

/// Compatibility wrapper presenting PerCpuScheduler as SchedulerInner
struct SchedulerCompat<'a> {
    inner: &'a mut PerCpuScheduler,
}

impl<'a> SchedulerCompat<'a> {
    // Implement SchedulerInner's interface by delegating to PerCpuScheduler
    pub fn enqueue_task(&mut self, task: *mut Task) -> c_int {
        self.inner.enqueue_local(task)
    }
    // ... other methods ...
}
```

#### 1.3 Migrate Callers

Update all `with_scheduler()` callers to use `with_local_scheduler()` or `with_cpu_scheduler()` directly where appropriate.

---

### Phase 2: Unified Scheduler Loop
**Effort**: 6 hours  
**Risk**: Medium

Create a single `scheduler_loop()` function that works identically on all CPUs.

#### 2.1 The Unified Loop

```rust
/// Unified scheduler loop for all CPUs.
/// 
/// This is the main scheduling loop that runs on every CPU after initialization.
/// It is symmetric - the same code runs on BSP and APs.
/// 
/// # Never Returns
/// This function runs forever (or until system shutdown).
pub fn scheduler_loop(cpu_id: usize) -> ! {
    // Sanity check: must be called on correct CPU
    debug_assert_eq!(cpu_id, get_current_cpu());
    
    // Get idle task (must exist)
    let idle_task = with_cpu_scheduler(cpu_id, |s| s.idle_task())
        .expect("scheduler_loop requires idle task");
    assert!(!idle_task.is_null(), "CPU {} has no idle task", cpu_id);
    
    // Enable this CPU's scheduler
    with_cpu_scheduler(cpu_id, |s| s.enable());
    
    loop {
        // === 1. Process Remote Wakes ===
        // Drain lock-free inbox before checking local queues.
        // This ensures cross-CPU wakes are processed promptly.
        with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });
        
        // === 2. Check for Test Pause ===
        // Test infrastructure may pause APs during reinitialization.
        // BSP (cpu_id == 0) is never paused.
        if are_aps_paused() && cpu_id != 0 {
            // Wait for resume IPI
            unsafe { asm!("sti; hlt; cli", options(nomem, nostack)); }
            continue;
        }
        
        // === 3. Select Next Task ===
        let next_task = with_cpu_scheduler(cpu_id, |sched| {
            sched.dequeue_highest_priority()
        }).unwrap_or(ptr::null_mut());
        
        // === 4. Execute or Idle ===
        if !next_task.is_null() {
            // Validate task is still runnable (may have been terminated)
            if task_is_ready(next_task) && !task_is_terminated(next_task) {
                execute_task(cpu_id, idle_task, next_task);
                // execute_task returns when task yields/blocks/exits
                continue;
            } else {
                // Task no longer runnable, skip it
                continue;
            }
        }
        
        // === 5. No Local Work - Try Work Stealing ===
        if try_work_steal(cpu_id) {
            // Stole a task, loop back to execute it
            continue;
        }
        
        // === 6. Nothing to Do - Idle ===
        with_cpu_scheduler(cpu_id, |s| s.stats.idle_ticks.fetch_add(1, Ordering::Relaxed));
        
        // Halt until next interrupt (timer tick, IPI, device IRQ)
        // sti enables interrupts, hlt halts until interrupt, cli disables again
        unsafe { asm!("sti; hlt; cli", options(nomem, nostack)); }
    }
}
```

#### 2.2 Execute Task Function

```rust
/// Execute a task until it yields, blocks, or exits.
/// 
/// This function context-switches from the scheduler loop (idle context)
/// to the task. When the task calls schedule() (via yield/block/exit),
/// we context-switch back to the idle context, which returns here.
fn execute_task(cpu_id: usize, idle_task: *mut Task, task: *mut Task) {
    // === Pre-Switch Setup ===
    
    // Double-check task state (may have changed since dequeue)
    if task_is_terminated(task) || !task_is_ready(task) {
        return;
    }
    
    // Mark scheduler as executing a task
    with_cpu_scheduler(cpu_id, |sched| {
        sched.set_executing_task(true);
        sched.set_current_task(task);
        sched.stats.total_switches.fetch_add(1, Ordering::Relaxed);
    });
    
    // Update global current task pointer
    task_set_current(task);
    
    // Set task state to running
    task_set_state(unsafe { (*task).task_id }, TASK_STATE_RUNNING);
    
    // === Address Space Switch ===
    unsafe {
        let is_user_mode = (*task).flags & TASK_FLAG_USER_MODE != 0;
        
        // Set kernel RSP0 in TSS for syscall/interrupt returns
        let kernel_rsp = if is_user_mode && (*task).kernel_stack_top != 0 {
            (*task).kernel_stack_top
        } else {
            kernel_stack_top() as u64
        };
        platform::gdt_set_kernel_rsp0(kernel_rsp);
        
        // Switch to task's address space if it has one
        if (*task).process_id != INVALID_TASK_ID {
            let page_dir = process_vm_get_page_dir((*task).process_id);
            if !page_dir.is_null() && !(*page_dir).pml4_phys.is_null() {
                (*task).context.cr3 = (*page_dir).pml4_phys.as_u64();
                paging_set_current_directory(page_dir);
            }
        }
    }
    
    // === Context Switch: Idle → Task ===
    let idle_ctx = unsafe { &raw mut (*idle_task).context };
    let task_ctx = unsafe { &(*task).context };
    
    unsafe {
        if (*task).flags & TASK_FLAG_USER_MODE != 0 {
            validate_user_context(task_ctx, task);
            context_switch_user(idle_ctx, task_ctx);
        } else {
            context_switch(idle_ctx, task_ctx);
        }
    }
    
    // === Post-Switch Cleanup (we're back from task) ===
    
    // Restore kernel address space
    unsafe {
        let kernel_dir = paging_get_kernel_directory();
        paging_set_current_directory(kernel_dir);
    }
    
    // Update scheduler state
    with_cpu_scheduler(cpu_id, |sched| {
        sched.set_current_task(idle_task);
        sched.set_executing_task(false);
    });
    
    task_set_current(idle_task);
    
    // Re-queue task if still runnable (preemption case)
    // For yield/block/exit, the task was already re-queued or terminated
    // by the code that triggered the schedule.
    unsafe {
        if !task_is_terminated(task) && task_is_running(task) {
            // Task was preempted (timer tick), re-queue it
            if task_set_state((*task).task_id, TASK_STATE_READY) == 0 {
                with_cpu_scheduler(cpu_id, |sched| {
                    sched.enqueue_local(task);
                });
            }
        }
    }
}
```

#### 2.3 Unified schedule() Function

```rust
/// Request a context switch.
/// 
/// Called when:
/// - Task yields (cooperative)
/// - Task blocks (waiting for I/O, mutex, etc.)
/// - Task exits/terminates
/// - Timer preemption
/// 
/// This function returns control to the scheduler loop, which will
/// select the next task to run.
pub fn schedule() {
    let _preempt_guard = PreemptGuard::new();
    let cpu_id = get_current_cpu();
    
    let current = with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    
    let idle_task = with_cpu_scheduler(cpu_id, |sched| sched.idle_task())
        .unwrap_or(ptr::null_mut());
    
    if idle_task.is_null() {
        // No idle task = can't schedule (shouldn't happen after init)
        return;
    }
    
    if current.is_null() || current == idle_task {
        // Already in idle context, nothing to do
        return;
    }
    
    // Handle re-queueing based on task state
    unsafe {
        if !task_is_terminated(current) {
            if task_is_running(current) {
                // Task is yielding or being preempted - re-queue it
                if task_set_state((*current).task_id, TASK_STATE_READY) == 0 {
                    with_cpu_scheduler(cpu_id, |sched| {
                        sched.enqueue_local(current);
                    });
                }
            }
            // If task is blocked or terminated, don't re-queue
        }
    }
    
    // === Context Switch: Task → Idle ===
    // This returns control to execute_task(), which returns to scheduler_loop()
    let current_ctx = unsafe { &raw mut (*current).context };
    let idle_ctx = unsafe { &(*idle_task).context };
    
    unsafe {
        context_switch(current_ctx, idle_ctx);
    }
    
    // NOTE: We return here when this task is scheduled again.
    // The context was saved above and will be restored by the next
    // context_switch() call that switches TO this task.
}
```

---

### Phase 3: BSP Entry Point Modification
**Effort**: 4 hours  
**Risk**: Medium-High (boot sequence change)

Modify BSP boot sequence to enter the unified scheduler loop.

#### 3.1 Boot Stack Transition

**Critical**: The boot stack is transient (Limine-provided, HHDM-mapped). We must switch to the idle task's kernel stack before entering the scheduler loop.

```rust
/// Enter the scheduler loop for a CPU.
/// 
/// This function:
/// 1. Switches from the boot stack to the idle task's kernel stack
/// 2. Enters the scheduler loop (never returns)
/// 
/// # Safety
/// - Must be called exactly once per CPU
/// - Must be called after per-CPU scheduler is initialized
/// - Must be called after idle task is created
pub fn enter_scheduler(cpu_id: usize) -> ! {
    let idle_task = with_cpu_scheduler(cpu_id, |s| s.idle_task())
        .expect("enter_scheduler: scheduler not initialized");
    
    if idle_task.is_null() {
        panic!("enter_scheduler: CPU {} has no idle task", cpu_id);
    }
    
    // Get idle task's kernel stack top
    let idle_stack_top = unsafe { (*idle_task).kernel_stack_top };
    if idle_stack_top == 0 {
        panic!("enter_scheduler: idle task has no kernel stack");
    }
    
    // Save scheduler loop context in idle task
    // This will be used when tasks switch back to the scheduler
    unsafe {
        let return_ctx = &raw mut (*idle_task).context;
        crate::ffi_boundary::init_kernel_context(return_ctx);
    }
    
    // === CRITICAL: Switch from boot stack to idle stack ===
    // 
    // The boot stack is HHDM-mapped physical memory provided by Limine.
    // It's valid during boot but its address would be saved into task
    // contexts by context_switch, causing crashes when we try to
    // restore that stack later (after HHDM might not be valid).
    //
    // The idle task's kernel stack is kmalloc'd and always valid.
    unsafe {
        asm!(
            "mov rsp, {0}",
            "mov rbp, rsp",
            in(reg) idle_stack_top,
            options(nostack)
        );
    }
    
    // Now running on idle task's stack - safe to enter scheduler loop
    scheduler_loop(cpu_id)
}
```

#### 3.2 BSP Initialization Changes

```rust
// In boot/src/early_init.rs or equivalent

pub fn kernel_main() -> ! {
    // ... early hardware init (GDT, IDT, APIC, memory) ...
    
    // Initialize per-CPU scheduler for BSP
    per_cpu::init_percpu_scheduler(0);
    
    // Create idle task for BSP
    let idle_task = create_cpu_idle_task(0);
    per_cpu::with_cpu_scheduler(0, |s| s.set_idle_task(idle_task));
    
    // Start Application Processors
    // Each AP will call enter_scheduler() independently
    smp::start_aps();
    
    // Create initial userland tasks
    userland::spawn_compositor();
    userland::spawn_shell();
    
    // BSP enters the scheduler loop (same as APs)
    // NOTE: This never returns!
    sched::enter_scheduler(0)
}
```

#### 3.3 AP Entry (Already Correct, Just Rename)

```rust
// In boot/src/smp.rs

pub fn ap_entry(cpu_info: &CpuInfo) -> ! {
    let cpu_id = cpu_info.cpu_index;
    
    // ... AP-specific init (PCR, GDT, IDT, APIC) ...
    
    // Initialize per-CPU scheduler for this AP
    per_cpu::init_percpu_scheduler(cpu_id);
    
    // Create idle task for this AP
    let idle_task = create_cpu_idle_task(cpu_id);
    per_cpu::with_cpu_scheduler(cpu_id, |s| s.set_idle_task(idle_task));
    
    // Enter scheduler loop (same function as BSP)
    sched::enter_scheduler(cpu_id)
}
```

---

### Phase 4: Lock-Free Cross-CPU Scheduling
**Effort**: 2 hours  
**Risk**: Low (existing infrastructure)

With the unified loop draining inboxes on all CPUs, enable lock-free scheduling.

#### 4.1 Restore Optimal schedule_task()

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() || !task_is_ready(task) {
        return -1;
    }
    
    // Reset time slice if needed
    with_local_scheduler(|sched| {
        if unsafe { (*task).time_slice_remaining } == 0 {
            reset_task_quantum(task);
        }
    });
    
    let target_cpu = select_target_cpu(task);
    let current_cpu = get_current_cpu();
    
    if target_cpu == current_cpu {
        // === Same CPU: Direct Local Enqueue ===
        // No synchronization needed (we own this queue)
        with_cpu_scheduler(target_cpu, |sched| {
            sched.enqueue_local(task)
        }).unwrap_or(-1)
    } else {
        // === Different CPU: Lock-Free Remote Wake ===
        // Push to target's inbox (lock-free CAS)
        with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
        });
        
        // Send IPI to wake target from hlt (if halted)
        send_reschedule_ipi(target_cpu);
        
        0
    }
}
```

#### 4.2 Remote Wake Inbox (Already Implemented)

The MPSC Treiber stack is already implemented in `per_cpu.rs`. Verify it's correct:

```rust
impl PerCpuScheduler {
    /// Push a task to this CPU's remote wake inbox.
    /// 
    /// This is a lock-free MPSC (multi-producer single-consumer) push.
    /// Can be called from ANY CPU safely.
    /// 
    /// Uses Treiber stack pattern with compare-and-swap.
    pub fn push_remote_wake(&self, task: *mut Task) {
        if task.is_null() {
            return;
        }
        
        loop {
            let old_head = self.remote_inbox_head.load(Ordering::Acquire);
            
            // Point task's next to current head
            unsafe {
                (*task).next_inbox.store(old_head, Ordering::Relaxed);
            }
            
            // Try to become new head
            match self.remote_inbox_head.compare_exchange_weak(
                old_head,
                task,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.inbox_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    core::hint::spin_loop();
                    // Retry
                }
            }
        }
    }
    
    /// Drain all tasks from remote inbox into local ready queues.
    /// 
    /// This is the single-consumer part of MPSC.
    /// MUST only be called by the owning CPU!
    pub fn drain_remote_inbox(&mut self) {
        // Atomically take entire inbox
        let head = self.remote_inbox_head.swap(ptr::null_mut(), Ordering::AcqRel);
        
        if head.is_null() {
            return;
        }
        
        self.stats.inbox_drains.fetch_add(1, Ordering::Relaxed);
        
        let mut count = 0u32;
        let mut current = head;
        
        // Reverse the stack to maintain FIFO order
        let mut reversed: *mut Task = ptr::null_mut();
        while !current.is_null() {
            let next = unsafe { (*current).next_inbox.load(Ordering::Acquire) };
            unsafe {
                (*current).next_inbox.store(reversed, Ordering::Relaxed);
            }
            reversed = current;
            current = next;
            count += 1;
        }
        
        // Enqueue all tasks into local ready queues
        current = reversed;
        while !current.is_null() {
            let next = unsafe { (*current).next_inbox.load(Ordering::Acquire) };
            
            // Clear inbox linkage
            unsafe {
                (*current).next_inbox.store(ptr::null_mut(), Ordering::Release);
            }
            
            // Enqueue locally
            self.enqueue_local(current);
            current = next;
        }
        
        self.inbox_count.fetch_sub(count, Ordering::Relaxed);
    }
}
```

---

### Phase 5: Cleanup and Code Deletion
**Effort**: 3 hours  
**Risk**: Low

Remove deprecated code paths and special cases.

#### 5.1 Delete These Functions

| Function | Location | Replacement |
|----------|----------|-------------|
| `schedule_on_ap()` | scheduler.rs | Merged into `schedule()` |
| `ap_scheduler_loop()` | scheduler.rs | Replaced by `scheduler_loop()` |
| `ap_execute_task()` | scheduler.rs | Replaced by `execute_task()` |
| `start_scheduler()` | scheduler.rs | Replaced by `enter_scheduler()` |

#### 5.2 Eliminate SchedulerInner

```rust
// BEFORE: Global state with shim
static SCHEDULER: Once<IrqMutex<SchedulerInner>> = Once::new();

pub fn with_scheduler<F, R>(f: F) -> R { ... }

// AFTER: No global state, direct per-CPU access
pub fn with_local_scheduler<R>(f: impl FnOnce(&mut PerCpuScheduler) -> R) -> R {
    let cpu_id = get_current_cpu();
    with_cpu_scheduler(cpu_id, f).expect("scheduler not initialized")
}
```

#### 5.3 Remove Special-Case Comments

Search and remove comments like:
- "BSP uses different path"
- "CPU 0 special case"
- "QUICKFIX"
- "Only works for BSP"

```bash
# Find all special-case comments
grep -rn "cpu_id == 0\|cpu_id != 0\|BSP\|quickfix\|CPU 0" core/src/scheduler/
```

---

## Critical Implementation Details

### Boot Stack Transition (MUST NOT SKIP)

**Why it's critical**:

The boot stack is provided by Limine and mapped in the HHDM (Higher Half Direct Map). When we context_switch, the current RSP is saved into the task's context. If we save the boot stack's address, then later try to restore it, we may crash because:

1. The HHDM mapping might not be valid in all contexts
2. The boot stack is meant to be transient
3. The address is in a different region than kernel stacks

**The fix**: Before entering the scheduler loop, switch RSP from boot stack to the idle task's kernel stack (which is kmalloc'd and always valid).

```rust
// This MUST happen before scheduler_loop()
unsafe {
    asm!("mov rsp, {}", in(reg) idle_task.kernel_stack_top);
}
```

### Context Switch Semantics

The key insight is that `context_switch(from, to)`:
1. Saves current registers into `from` context
2. Loads registers from `to` context
3. Returns to wherever `to`'s RIP points

This means:
- When we switch idle→task, idle's RIP points into `execute_task()`
- When task calls `schedule()` → idle, we return to `execute_task()`
- `execute_task()` returns to `scheduler_loop()`

```
scheduler_loop()
  └── execute_task(task)
        ├── context_switch(idle_ctx, task_ctx)
        │     └── [task runs, eventually calls schedule()]
        │           └── schedule()
        │                 └── context_switch(task_ctx, idle_ctx)
        │                       └── [returns to execute_task]
        └── [cleanup, return to scheduler_loop]
```

### Preemption Guard

During context switching, preemption must be disabled to prevent recursive scheduling:

```rust
pub fn schedule() {
    let _guard = PreemptGuard::new();  // Disables preemption
    // ... context switch ...
}  // _guard dropped, preemption re-enabled

impl PreemptGuard {
    pub fn new() -> Self {
        // Increment per-CPU preempt_count
        // Timer tick checks this before calling schedule()
        unsafe { asm!("cli"); }  // Also disable interrupts during switch
        ...
    }
}

impl Drop for PreemptGuard {
    fn drop(&mut self) {
        // Decrement preempt_count
        // Re-enable interrupts
        unsafe { asm!("sti"); }
        ...
    }
}
```

---

## Migration Strategy

### Incremental Rollout

| Step | Action | Risk | Rollback |
|------|--------|------|----------|
| 0 | Add inbox drain to timer tick | Very Low | Remove one line |
| 1 | Add compatibility shim for with_scheduler() | Low | Keep both paths |
| 2 | Implement scheduler_loop() alongside old code | Low | Don't call it |
| 3 | APs use scheduler_loop() | Medium | Revert to ap_scheduler_loop() |
| 4 | BSP uses scheduler_loop() | High | Revert BSP init sequence |
| 5 | Delete old code | Low | N/A (no rollback needed) |

### Feature Flag (Optional)

```rust
#[cfg(feature = "unified_scheduler")]
pub fn enter_scheduler(cpu_id: usize) -> ! {
    // New unified path
    scheduler_loop(cpu_id)
}

#[cfg(not(feature = "unified_scheduler"))]
pub fn enter_scheduler(cpu_id: usize) -> ! {
    // Old path (BSP uses start_scheduler, APs use scheduler_run_ap)
    if cpu_id == 0 {
        start_scheduler()
    } else {
        scheduler_run_ap(cpu_id)
    }
}
```

Enable with: `cargo build --features unified_scheduler`

---

## Testing Strategy

### Unit Tests (where applicable)

```rust
#[cfg(test)]
mod tests {
    #[test]
    fn test_inbox_push_pop() {
        let sched = PerCpuScheduler::new();
        let task = create_test_task();
        
        sched.push_remote_wake(task);
        assert!(sched.has_pending_inbox());
        
        sched.drain_remote_inbox();
        assert!(!sched.has_pending_inbox());
        assert_eq!(sched.dequeue_highest_priority(), task);
    }
}
```

### Integration Tests

```bash
# Run test harness (already uses make test)
make test

# Expected: All 360 tests pass
```

### Manual Verification Checklist

After each phase:

- [ ] `make test` passes (360/360)
- [ ] `make boot VIDEO=1` - compositor renders smoothly
- [ ] `make boot-log` - no kernel panics in log
- [ ] Task termination during compositor rendering - no stutter
- [ ] Multi-CPU task distribution - tasks run on all CPUs

### Performance Verification

```rust
// Add to compositor or test task
fn measure_schedule_latency() {
    let start = rdtsc();
    schedule_task(task);  // Cross-CPU schedule
    let end = rdtsc();
    
    let cycles = end - start;
    // Before (quickfix): ~1000-5000 cycles (mutex contention)
    // After (lock-free): ~100-300 cycles
    log!("Cross-CPU schedule: {} cycles", cycles);
}
```

---

## Success Criteria

### Functional Requirements

| Requirement | Verification Method |
|-------------|---------------------|
| BSP uses scheduler_loop() | Code inspection |
| APs use scheduler_loop() | Code inspection |
| Same code path for all CPUs | grep for `cpu_id == 0` in scheduler |
| Remote inbox drained on all CPUs | Add logging, verify in boot log |
| Lock-free cross-CPU schedule_task() | Code inspection, no mutex in hot path |
| Compositor 60fps during task churn | Manual test with task spawn/exit loop |
| All existing tests pass | `make test` |

### Non-Functional Requirements

| Requirement | Target | Verification |
|-------------|--------|--------------|
| Cross-CPU wake latency | <10μs | Cycle counter measurement |
| No mutex on scheduling hot path | 0 mutex acquisitions | Code audit |
| No special-case BSP code | 0 occurrences | grep audit |
| Code reduction | >200 lines deleted | git diff --stat |

### Grep Audit Commands

```bash
# Must find 0 results after implementation
grep -rn "cpu_id == 0\|cpu_id != 0" core/src/scheduler/
grep -rn "quickfix\|QUICKFIX" core/src/scheduler/
grep -rn "BSP.*special\|CPU 0.*different" core/src/scheduler/

# Should find unified functions
grep -n "scheduler_loop\|enter_scheduler" core/src/scheduler/scheduler.rs
```

---

## Risks and Mitigations

| Risk | Severity | Likelihood | Mitigation |
|------|----------|------------|------------|
| BSP boot failure | Critical | Medium | Incremental rollout, feature flag |
| Context corruption | Critical | Low | Extensive testing, keep old code until verified |
| Timer tick handling breaks | High | Medium | Test preemption explicitly |
| Compositor regression | High | Medium | Measure FPS before/after each phase |
| Boot stack crash | Critical | Low | Explicit stack transition, assertions |
| Deadlock in scheduler | Critical | Low | No locks on hot path, inbox is lock-free |
| Race in task state | High | Medium | Atomic state transitions, careful ordering |

### Rollback Plan

Each phase has an explicit rollback:

1. **Phase 0**: Remove inbox drain from timer tick
2. **Phase 1**: Remove compatibility shim, restore direct SchedulerInner access
3. **Phase 2**: Don't call scheduler_loop(), keep ap_scheduler_loop()
4. **Phase 3**: Restore start_scheduler() for BSP
5. **Phase 4**: Restore quickfix (enqueue_local for cross-CPU)
6. **Phase 5**: N/A (just code deletion)

---

## Future Extensions

### Pluggable Scheduler Policies

Following Theseus OS's trait-based design:

```rust
/// Scheduler policy trait (future extension)
pub trait SchedulerPolicy: Send + Sync + 'static {
    /// Select next task to run
    fn select_next(&mut self) -> Option<*mut Task>;
    
    /// Add task to run queue
    fn enqueue(&mut self, task: *mut Task);
    
    /// Remove task from run queue
    fn remove(&mut self, task: *mut Task) -> bool;
    
    /// Report queue busyness (for load balancing)
    fn busyness(&self) -> usize;
}

/// Round-robin policy (default)
pub struct RoundRobinPolicy {
    queues: [ReadyQueue; NUM_PRIORITIES],
}

impl SchedulerPolicy for RoundRobinPolicy { ... }

/// CFS-like policy (future)
pub struct FairSchedulerPolicy {
    rb_tree: RBTree<VrunTime, TaskRef>,
}

impl SchedulerPolicy for FairSchedulerPolicy { ... }

// In PerCpuScheduler:
pub struct PerCpuScheduler {
    // ...
    policy: Box<dyn SchedulerPolicy>,
}
```

### NUMA-Aware Scheduling

For future multi-socket support:

```rust
pub struct NumaTopology {
    /// CPU → NUMA node mapping
    cpu_to_node: [u8; MAX_CPUS],
    /// NUMA node → memory latency matrix
    latency_matrix: [[u16; MAX_NODES]; MAX_NODES],
}

fn select_target_cpu(task: *mut Task) -> usize {
    let preferred_node = task.preferred_numa_node;
    
    // Prefer CPUs in same NUMA node
    for cpu in numa.cpus_in_node(preferred_node) {
        if cpu_has_capacity(cpu) {
            return cpu;
        }
    }
    
    // Fall back to least-loaded CPU
    find_least_loaded_cpu(0)
}
```

### Real-Time Scheduling Class

```rust
pub enum SchedulingClass {
    /// Normal timesharing (default)
    Normal,
    /// Real-time FIFO (no preemption within class)
    RealTimeFifo,
    /// Real-time round-robin (preemption within class)
    RealTimeRR,
    /// Idle (only runs when no other work)
    Idle,
}

// RT tasks always run before normal tasks
fn select_next(&mut self) -> Option<*mut Task> {
    // Check RT queue first
    if let Some(rt_task) = self.rt_queue.dequeue() {
        return Some(rt_task);
    }
    // Then normal queues
    self.normal_queues.dequeue_highest_priority()
}
```

---

## References

### Production Rust OS Schedulers

1. **Theseus OS** - https://github.com/theseus-os/Theseus
   - `kernel/task/src/scheduler.rs` - Pluggable policy trait
   - `kernel/cpu/src/lib.rs` - CPU-local storage
   - Uses `#[cls::cpu_local]` for per-CPU state

2. **Redox OS** - https://github.com/redox-os/kernel
   - `src/context/switch.rs` - Symmetric context switching
   - `src/percpu.rs` - Per-CPU block via GS segment
   - Uses `PercpuBlock` for all per-CPU state

3. **Linux Kernel** - Reference (not Rust)
   - `kernel/sched/core.c` - Per-CPU runqueues
   - `kernel/sched/smp.c` - Cross-CPU wake protocol
   - Uses `struct rq` per-CPU with lock-free wake paths

### Lock-Free Data Structures

4. **Treiber Stack** - Classic lock-free stack
   - Used for remote wake inbox (MPSC)
   - CAS-based push, single-consumer drain

5. **Michael-Scott Queue** - Lock-free FIFO
   - Alternative for inbox if FIFO ordering matters

### SlopOS Current Implementation

6. **Current Quickfix** - `core/src/scheduler/scheduler.rs:321`
   - Uses `enqueue_local()` for all cross-CPU
   - Works but has mutex contention

7. **Per-CPU Infrastructure** - `core/src/scheduler/per_cpu.rs`
   - `PerCpuScheduler` struct
   - `push_remote_wake()` / `drain_remote_inbox()`

8. **AP Scheduler Loop** - `core/src/scheduler/scheduler.rs:1436`
   - `ap_scheduler_loop()` - correct design, just not used by BSP

---

## Appendix A: Current Code Locations

| Component | File | Line | Notes |
|-----------|------|------|-------|
| SchedulerInner | scheduler.rs | 44 | To be eliminated |
| PerCpuScheduler | per_cpu.rs | 156 | Gold standard |
| schedule() | scheduler.rs | 533 | Asymmetric, to be unified |
| schedule_on_ap() | scheduler.rs | 638 | To be merged |
| ap_scheduler_loop() | scheduler.rs | 1436 | To become scheduler_loop() |
| ap_execute_task() | scheduler.rs | 1494 | To become execute_task() |
| start_scheduler() | scheduler.rs | 1055 | To be replaced |
| scheduler_run_ap() | scheduler.rs | 1392 | To become enter_scheduler() |
| Quickfix | scheduler.rs | 321 | To be removed |
| push_remote_wake() | per_cpu.rs | 362 | Keep |
| drain_remote_inbox() | per_cpu.rs | 401 | Keep |

---

## Appendix B: Commit Message Template

```
sched: unify BSP and AP scheduler architecture

Implement symmetric per-CPU scheduler following Theseus/Redox patterns:

- Replace global SchedulerInner with per-CPU PerCpuScheduler for all CPUs
- Create unified scheduler_loop() used by BSP and APs
- BSP now enters scheduler_loop() instead of halting after init
- Enable lock-free cross-CPU wakeups via remote inbox on all CPUs
- Delete special-case BSP code paths (schedule_on_ap, ap_scheduler_loop)

Performance: Cross-CPU schedule latency reduced from ~1000 to ~200 cycles
by eliminating mutex contention in schedule_task().

Closes: #XXX
```

---

*This plan transforms SlopOS from an asymmetric historical accident to a symmetric, production-grade scheduler architecture matching the best practices of modern Rust operating systems.*
