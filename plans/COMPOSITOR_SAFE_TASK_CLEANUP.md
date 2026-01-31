# Compositor-Safe Task Cleanup: Gold Standard Implementation Plan

**Status**: Planned  
**Priority**: High  
**Estimated Effort**: 2-3 days  
**Author**: AI Analysis based on Theseus OS, Redox OS, and lock-free patterns research  
**Date**: 2026-01-31

---

## Executive Summary

This plan eliminates the stop-the-world `pause_all_aps()` mechanism during task termination, replacing it with **lock-free atomic protocols** that allow the compositor (and all real-time tasks) to continue running uninterrupted.

The solution draws from production Rust operating systems (Theseus OS, Redox OS) and modern lock-free programming patterns to achieve:

- **Zero compositor frame drops** during task termination
- **Compile-time safety** via Rust's type system where possible
- **Lock-free hot paths** for scheduler dequeue operations
- **Memory safety** via intrusive reference counting

---

## Table of Contents

1. [Problem Analysis](#1-problem-analysis)
2. [Research Findings](#2-research-findings)
3. [Architecture Design](#3-architecture-design)
4. [Implementation Phases](#4-implementation-phases)
5. [Code Specifications](#5-code-specifications)
6. [Testing Strategy](#6-testing-strategy)
7. [Success Criteria](#7-success-criteria)
8. [Appendix: Quick Wins](#appendix-quick-wins)

---

## 1. Problem Analysis

### 1.1 Current Behavior

When a task terminates, the following sequence occurs:

```rust
// core/src/scheduler/task.rs:547-549
let was_paused = crate::per_cpu::pause_all_aps();  // BLOCKS ALL APs!
release_task_dependents(resolved_id);              // Scans all tasks
crate::per_cpu::resume_all_aps_if_not_nested(was_paused);
```

**Impact**: 
- `pause_all_aps()` spins up to **100,000 iterations** waiting for all APs to become idle
- Any task on an AP (including compositor) is frozen during this window
- Results in **1-2 frame drops** (16-32ms stalls) during task termination

### 1.2 Root Causes

The pause exists to prevent three race conditions:

| Race Condition | Current Field | Problem |
|----------------|---------------|---------|
| **Dependency Scan Race** | `waiting_on_task_id: u32` | Plain u32, not atomic - concurrent R/W corrupts value |
| **State Transition Race** | `state_atomic: AtomicU8` | No single-winner wake - double enqueue possible |
| **Queue Corruption** | `next_ready: *mut Task` | Cross-CPU queue mutations corrupt linked list |

### 1.3 Why The Pause Is Wrong

1. **Overkill**: Stops ALL CPUs to handle a race affecting only tasks waiting on the terminating task
2. **Unbounded latency**: Spin-waits up to 100k iterations
3. **Violates real-time guarantees**: Compositor cannot meet 60fps with random 16-32ms stalls
4. **Not Rust-idiomatic**: Uses global synchronization instead of fine-grained atomics

---

## 2. Research Findings

### 2.1 Theseus OS (MIT License)

Theseus is a Rust OS designed for maximum compile-time safety. Key patterns:

```rust
// Reference-counted task ownership
pub struct TaskRef(Arc<TaskRefInner>);

// Mailbox pattern for exit values - no scanning needed
exit_value_mailbox: Mutex<Option<ExitValue>>,

// Explicit state machine with atomic transitions
self.0.task.runstate().store(RunState::Exited);
fence(Ordering::Release);

// RAII preemption control
let preemption_guard = preemption::hold_preemption();
```

**Why Theseus never needs global pauses**:
- `Arc<Task>` ensures tasks aren't freed while referenced
- Atomic state machines prevent double-enqueue
- Ownership is explicit via `JoinableTaskRef` / `ExitableTaskRef` types
- Per-CPU runqueues with no cross-CPU locking

### 2.2 Redox OS (MIT License)

Redox uses a hybrid approach with per-CPU state:

```rust
// Per-CPU context switch state
pub struct ContextSwitchPercpu {
    current_ctxt: RefCell<Option<Arc<ContextLock>>>,
    switch_result: Cell<Option<SwitchResultInner>>,
    // ...
}

// Arc-protected contexts
pub struct ContextRef(pub Arc<ContextLock>);

// Minimal global lock - only during actual switch
arch::CONTEXT_SWITCH_LOCK.store(false, Ordering::SeqCst);
```

**Key insight**: Global lock is only held during the actual register swap, not during cleanup or scanning.

### 2.3 Lock-Free Patterns

**MPSC (Multi-Producer Single-Consumer) Queue**:
- Multiple CPUs can wake tasks on a target CPU without locking
- Target CPU drains inbox into local runqueue (no contention)
- Treiber stack is simplest implementation

**Intrusive Reference Counting**:
- Embed `refcnt: AtomicU32` in Task struct
- No heap allocation for reference counting
- Safe deferred reclamation when refcount hits zero

---

## 3. Architecture Design

### 3.1 Core Principles

1. **No global pauses**: Replace stop-the-world with fine-grained atomics
2. **Single-winner wakeup**: CAS ensures exactly one waker enqueues a task
3. **Local runqueues**: Each CPU owns its runqueue exclusively
4. **Remote wake inbox**: Cross-CPU wakes use lock-free MPSC push
5. **Deferred reclamation**: Tasks aren't freed until provably unreachable

### 3.2 Data Flow (After Implementation)

```
Task A calls task_wait_for(B):
  1. A.waiting_on.store(B.id, Release)
  2. A.state_atomic.store(BLOCKED, Release)
  3. A removed from runqueue, scheduler picks next task

Task B terminates:
  1. B.state_atomic.store(TERMINATED, Release)
  2. fence(SeqCst)  // Ensure visibility
  3. Scan tasks: for each task T where T.waiting_on == B.id:
     a. CAS T.waiting_on from B.id to WAIT_NONE
     b. If CAS succeeds (we won): unblock_task(T)
     c. If CAS fails: another waker won or T changed wait target
  4. Continue with B's cleanup (no pause needed!)

unblock_task(T):
  1. CAS T.state_atomic from BLOCKED to READY
  2. If CAS succeeds: enqueue T to appropriate CPU
  3. If CAS fails: T already woken by another path (timeout, signal)
```

### 3.3 Memory Safety Model

```
┌─────────────────────────────────────────────────────────────────┐
│                      Task Lifecycle                              │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  CREATED ──► READY ──► RUNNING ──► BLOCKED ──► READY ──► ...   │
│                │                      │                          │
│                │                      │                          │
│                ▼                      ▼                          │
│           TERMINATED ◄────────────────┘                         │
│                │                                                 │
│                ▼                                                 │
│          ZOMBIE (refcnt > 0)                                    │
│                │                                                 │
│                ▼ (when refcnt == 0)                              │
│             FREED                                                │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘

Invariants:
- BLOCKED implies waiting_on != WAIT_NONE
- READY implies exactly one runqueue contains this task
- TERMINATED implies waiting_on == WAIT_NONE
- refcnt > 0 implies Task memory is valid
- refcnt == 0 AND TERMINATED implies safe to free
```

---

## 4. Implementation Phases

### Phase 1: Atomic Task Dependencies
**Effort**: 2 hours  
**Risk**: Low  
**Files**: `abi/src/task.rs`, `core/src/scheduler/scheduler.rs`, `core/src/scheduler/task.rs`

Convert `waiting_on_task_id` from plain `u32` to `AtomicU32` and update all access sites.

### Phase 2: Single-Winner Wakeup Protocol  
**Effort**: 3 hours  
**Risk**: Medium  
**Files**: `core/src/scheduler/scheduler.rs`

Implement CAS-based wakeup that ensures exactly one waker succeeds.

### Phase 3: Remove Global Pause
**Effort**: 1 hour  
**Risk**: Medium  
**Files**: `core/src/scheduler/task.rs`

Remove `pause_all_aps()` / `resume_all_aps()` from task termination path.

### Phase 4: Per-CPU Remote Wake Inbox
**Effort**: 4 hours  
**Risk**: Medium  
**Files**: `abi/src/task.rs`, `core/src/scheduler/per_cpu.rs`, `core/src/scheduler/scheduler.rs`

Add lock-free MPSC inbox for cross-CPU task wakeups.

### Phase 5: Deferred Task Reclamation
**Effort**: 4 hours  
**Risk**: Low  
**Files**: `abi/src/task.rs`, `core/src/scheduler/task.rs`

Add intrusive reference counting and zombie reaper for safe memory reclamation.

### Phase 6: Validation and Stress Testing
**Effort**: 2 hours  
**Risk**: Low  
**Files**: `core/src/scheduler/sched_tests.rs`

Add stress tests for concurrent termination and compositor stability.

---

## 5. Code Specifications

### 5.1 Phase 1: Atomic Task Dependencies

#### 5.1.1 Task Structure Changes

```rust
// abi/src/task.rs

/// Sentinel value indicating task is not waiting on any other task
pub const WAIT_NONE: u32 = 0;

pub struct Task {
    // ... existing fields ...
    
    // CHANGED: From plain u32 to AtomicU32
    // Old: pub waiting_on_task_id: u32,
    /// Task ID this task is waiting on (WAIT_NONE if not waiting)
    pub waiting_on: AtomicU32,
    
    // ... rest of fields ...
}

impl Task {
    pub const fn invalid() -> Self {
        Self {
            // ...
            // CHANGED: Initialize as atomic
            waiting_on: AtomicU32::new(WAIT_NONE),
            // ...
        }
    }
}
```

#### 5.1.2 Blocking Logic Update

```rust
// core/src/scheduler/scheduler.rs

pub fn task_wait_for(task_id: u32) -> c_int {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return -1;
    }
    
    // Validate: can't wait on self or invalid ID
    let current_id = unsafe { (*current).task_id };
    if task_id == INVALID_TASK_ID || current_id == task_id {
        return -1;
    }

    // Check target exists
    let mut target: *mut Task = ptr::null_mut();
    if task_get_info(task_id, &mut target) != 0 || target.is_null() {
        // Target doesn't exist - don't block
        return 0;
    }
    
    // Check target isn't already terminated
    if task_is_terminated(target) {
        return 0;
    }

    // ATOMIC: Set wait target with Release ordering
    // This ensures the write is visible to any CPU that observes our BLOCKED state
    unsafe {
        (*current).waiting_on.store(task_id, Ordering::Release);
    }
    
    // Block current task (will context switch away)
    block_current_task();
    
    // When we wake up, clear the wait target
    unsafe {
        (*current).waiting_on.store(WAIT_NONE, Ordering::Release);
    }
    
    0
}
```

### 5.2 Phase 2: Single-Winner Wakeup Protocol

```rust
// core/src/scheduler/scheduler.rs

/// Attempt to wake a task that was waiting on `completed_id`.
/// Returns true if THIS caller won the wake race and should enqueue the task.
/// Returns false if another caller already woke it or task wasn't waiting on this ID.
/// 
/// This is the key primitive for lock-free task termination.
pub fn try_wake_from_task_wait(task: *mut Task, completed_id: u32) -> bool {
    if task.is_null() || completed_id == WAIT_NONE {
        return false;
    }
    
    // CAS: Atomically clear waiting_on only if it matches completed_id
    // Only ONE caller can succeed this CAS - the "winner"
    let result = unsafe {
        (*task).waiting_on.compare_exchange(
            completed_id,          // expected: waiting on the completed task
            WAIT_NONE,             // desired: no longer waiting
            Ordering::AcqRel,      // success: acquire prior writes, release our write
            Ordering::Acquire,     // failure: just acquire to see current value
        )
    };
    
    match result {
        Ok(_) => {
            // We won the race! Now transition state and enqueue
            unblock_task_winner(task)
        }
        Err(current) => {
            // Lost race OR task is waiting on different ID
            // Either way, not our responsibility to wake
            klog_debug!(
                "try_wake_from_task_wait: lost race or wrong target (expected={}, actual={})",
                completed_id, current
            );
            false
        }
    }
}

/// Called only by the winner of try_wake_from_task_wait.
/// Transitions task to READY and enqueues it.
fn unblock_task_winner(task: *mut Task) -> bool {
    // CAS: BLOCKED -> READY (single-winner state transition)
    let result = unsafe {
        (*task).state_atomic.compare_exchange(
            TASK_STATE_BLOCKED,
            TASK_STATE_READY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    };
    
    match result {
        Ok(_) => {
            // State transition successful - enqueue the task
            core::sync::atomic::fence(Ordering::SeqCst);
            schedule_task(task) == 0
        }
        Err(current_state) => {
            // Task state changed unexpectedly
            // This shouldn't happen if invariants are maintained
            klog_info!(
                "unblock_task_winner: unexpected state {} for task {}",
                current_state,
                unsafe { (*task).task_id }
            );
            false
        }
    }
}

/// Updated unblock_task - now idempotent and safe for concurrent calls
pub fn unblock_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }

    // Fast path: already ready/running
    if !task_is_blocked(task) {
        return 0;
    }

    // Clear any wait target first (we're unblocking regardless of reason)
    unsafe {
        (*task).waiting_on.store(WAIT_NONE, Ordering::Release);
    }

    // Try to transition BLOCKED -> READY
    let result = unsafe {
        (*task).state_atomic.compare_exchange(
            TASK_STATE_BLOCKED,
            TASK_STATE_READY,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
    };

    match result {
        Ok(_) => {
            core::sync::atomic::fence(Ordering::SeqCst);
            schedule_task(task)
        }
        Err(current) => {
            // Already ready, running, or terminated - that's fine
            if task_is_terminated(task) || task_is_invalid(task) {
                -1
            } else {
                0  // Already runnable
            }
        }
    }
}
```

### 5.3 Phase 3: Remove Global Pause

```rust
// core/src/scheduler/task.rs

/// Release all tasks that were waiting on the completed task.
/// 
/// IMPORTANT: This function is now lock-free and does NOT require pausing APs.
/// The atomic CAS protocol ensures exactly one waker succeeds per waiting task.
fn release_task_dependents(completed_task_id: u32) {
    // Scan all tasks under task manager lock (read-only scan)
    // The lock protects the task array structure, not the atomic fields
    with_task_manager(|mgr| {
        for task in mgr.tasks.iter() {
            // Skip invalid tasks
            if task.state() == TASK_STATE_INVALID {
                continue;
            }
            
            // Skip terminated tasks
            if task.state() == TASK_STATE_TERMINATED {
                continue;
            }
            
            // Atomic load - no lock needed
            let waiting_on = task.waiting_on.load(Ordering::Acquire);
            
            // Skip if not waiting on the completed task
            if waiting_on != completed_task_id {
                continue;
            }
            
            // Try to wake - CAS ensures single winner
            let task_ptr = task as *const Task as *mut Task;
            if scheduler::try_wake_from_task_wait(task_ptr, completed_task_id) {
                klog_info!(
                    "release_task_dependents: Unblocked task {} (was waiting on {})",
                    task.task_id,
                    completed_task_id
                );
            }
        }
    });
}

pub fn task_terminate(task_id: u32) -> c_int {
    // ... validation and setup (unchanged) ...
    
    // Unschedule the task from ready queues
    scheduler::unschedule_task(task_ptr);

    // Update runtime statistics
    let now = kdiag_timestamp();
    unsafe {
        if (*task_ptr).last_run_timestamp != 0 && now >= (*task_ptr).last_run_timestamp {
            (*task_ptr).total_runtime += now - (*task_ptr).last_run_timestamp;
        }
        (*task_ptr).last_run_timestamp = 0;
        
        // Record exit reason
        if (*task_ptr).exit_reason == TaskExitReason::None {
            (*task_ptr).exit_reason = TaskExitReason::Kernel;
        }
        record_task_exit(
            task_ptr,
            (*task_ptr).exit_reason,
            (*task_ptr).fault_reason,
            (*task_ptr).exit_code,
        );
    }
    
    // CRITICAL: Mark as terminated FIRST with Release ordering
    // This ensures any task checking our state will see TERMINATED
    unsafe {
        (*task_ptr).set_state(TASK_STATE_TERMINATED);
    }
    
    // Clear our own waiting_on (we're done waiting on anything)
    unsafe {
        (*task_ptr).waiting_on.store(WAIT_NONE, Ordering::Release);
    }
    
    // Memory barrier: ensure TERMINATED state is visible before we wake dependents
    core::sync::atomic::fence(Ordering::SeqCst);
    
    // ========================================
    // NO MORE pause_all_aps()!
    // The atomic protocol in release_task_dependents handles races safely.
    // ========================================
    release_task_dependents(resolved_id);
    
    // ... rest of cleanup (unchanged) ...
}
```

### 5.4 Phase 4: Per-CPU Remote Wake Inbox

#### 5.4.1 Task Structure Addition

```rust
// abi/src/task.rs

pub struct Task {
    // ... existing fields ...
    
    /// Linkage for remote wake inbox (separate from ready queue linkage)
    /// This allows a task to be in the inbox while next_ready is used elsewhere
    pub next_inbox: AtomicPtr<Task>,
    
    /// Reference count for safe deferred reclamation
    pub refcnt: AtomicU32,
}

impl Task {
    pub const fn invalid() -> Self {
        Self {
            // ...
            next_inbox: AtomicPtr::new(ptr::null_mut()),
            refcnt: AtomicU32::new(0),
            // ...
        }
    }
    
    /// Increment reference count. Returns new count.
    #[inline]
    pub fn inc_ref(&self) -> u32 {
        self.refcnt.fetch_add(1, Ordering::AcqRel) + 1
    }
    
    /// Decrement reference count. Returns true if this was the last reference.
    #[inline]
    pub fn dec_ref(&self) -> bool {
        self.refcnt.fetch_sub(1, Ordering::AcqRel) == 1
    }
    
    /// Get current reference count.
    #[inline]
    pub fn ref_count(&self) -> u32 {
        self.refcnt.load(Ordering::Acquire)
    }
}
```

#### 5.4.2 Per-CPU Scheduler Update

```rust
// core/src/scheduler/per_cpu.rs

#[repr(C, align(64))]
pub struct PerCpuScheduler {
    pub cpu_id: usize,
    ready_queues: [ReadyQueue; NUM_PRIORITY_LEVELS],
    queue_lock: Mutex<()>,  // Only for local operations now
    current_task_atomic: AtomicPtr<Task>,
    idle_task_atomic: AtomicPtr<Task>,
    pub enabled: AtomicBool,
    pub time_slice: u16,
    pub total_switches: AtomicU64,
    pub total_preemptions: AtomicU64,
    pub total_ticks: AtomicU64,
    pub idle_time: AtomicU64,
    initialized: AtomicBool,
    pub return_context: TaskContext,
    executing_task: AtomicBool,
    
    // NEW: Lock-free MPSC inbox for cross-CPU wakeups
    /// Head of Treiber stack for remote wake requests
    remote_inbox_head: AtomicPtr<Task>,
    /// Count of tasks in inbox (informational only)
    inbox_count: AtomicU32,
}

impl PerCpuScheduler {
    pub const fn new() -> Self {
        Self {
            // ... existing initializations ...
            remote_inbox_head: AtomicPtr::new(ptr::null_mut()),
            inbox_count: AtomicU32::new(0),
        }
    }
    
    /// Push a task to this CPU's remote wake inbox.
    /// 
    /// This is a lock-free MPSC (multi-producer single-consumer) push.
    /// Can be called from ANY CPU safely.
    /// 
    /// # Safety
    /// - Task must be valid and not already in any queue
    /// - Task's next_inbox field will be overwritten
    pub fn push_remote_wake(&self, task: *mut Task) {
        if task.is_null() {
            return;
        }
        
        // Increment task refcount while in inbox
        unsafe { (*task).inc_ref() };
        
        // Lock-free push using CAS loop (Treiber stack pattern)
        loop {
            // Load current head
            let old_head = self.remote_inbox_head.load(Ordering::Acquire);
            
            // Point our next to current head
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
                    // Success! Update count and return
                    self.inbox_count.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                Err(_) => {
                    // Lost race - retry
                    core::hint::spin_loop();
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
        
        let mut count = 0u32;
        let mut current = head;
        
        // Reverse the stack to maintain FIFO order (optional but nice)
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
            
            // Enqueue into appropriate priority queue
            let priority = unsafe { (*current).priority as usize };
            let idx = priority.min(NUM_PRIORITY_LEVELS - 1);
            
            // Local enqueue - no lock needed since we're the only mutator
            self.ready_queues[idx].enqueue(current);
            
            // Decrement refcount (was incremented on push)
            unsafe { (*current).dec_ref() };
            
            current = next;
        }
        
        // Update inbox count
        self.inbox_count.fetch_sub(count, Ordering::Relaxed);
    }
    
    /// Check if inbox has pending tasks
    #[inline]
    pub fn has_pending_inbox(&self) -> bool {
        !self.remote_inbox_head.load(Ordering::Acquire).is_null()
    }
}
```

#### 5.4.3 Updated Schedule Task

```rust
// core/src/scheduler/scheduler.rs

/// Schedule a task onto an appropriate CPU's ready queue.
/// 
/// If the target CPU is the current CPU, enqueues directly.
/// If the target CPU is different, uses the lock-free remote inbox.
pub fn schedule_task(task: *mut Task) -> c_int {
    if task.is_null() {
        return -1;
    }
    if !task_is_ready(task) {
        return -1;
    }

    // Reset quantum if needed
    with_scheduler(|sched| {
        if unsafe { (*task).time_slice_remaining } == 0 {
            reset_task_quantum(sched, task);
        }
    });

    // Select target CPU based on affinity and load
    let target_cpu = per_cpu::select_target_cpu(task);
    let current_cpu = slopos_lib::get_current_cpu();

    if target_cpu == current_cpu {
        // Same CPU: direct local enqueue
        let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.enqueue_local(task)
        });
        
        if result != Some(0) {
            // Fallback to global queue
            with_scheduler(|sched| sched.enqueue_task(task))
        } else {
            0
        }
    } else {
        // Different CPU: use lock-free remote inbox
        per_cpu::with_cpu_scheduler(target_cpu, |sched| {
            sched.push_remote_wake(task);
        });
        
        // Send IPI to wake target CPU if needed
        if slopos_lib::is_cpu_online(target_cpu) {
            send_reschedule_ipi(target_cpu);
        }
        
        0
    }
}
```

#### 5.4.4 Updated Scheduler Loop

```rust
// core/src/scheduler/scheduler.rs

fn ap_scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    use super::work_steal::try_work_steal;

    loop {
        // FIRST: Drain remote inbox before checking for work
        // This ensures cross-CPU wakes are processed promptly
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });
        
        // Check if APs should be paused (for test reinitialization only now)
        if per_cpu::are_aps_paused() {
            unsafe {
                core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
            }
            continue;
        }

        // Try to get next task from local queue
        let next_task =
            per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.dequeue_highest_priority())
                .unwrap_or(ptr::null_mut());

        if !next_task.is_null() {
            per_cpu::with_cpu_scheduler(cpu_id, |sched| {
                sched.set_executing_task(true);
            });

            // Double-check pause after setting executing flag
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
            
            ap_execute_task(cpu_id, idle_task, next_task);
            continue;
        }

        // Try work stealing if nothing local and not paused
        if !per_cpu::are_aps_paused() && try_work_steal() {
            continue;
        }

        // Increment idle time
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.increment_idle_time();
        });

        // Halt until next interrupt
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
        }
    }
}
```

### 5.5 Phase 5: Deferred Task Reclamation

```rust
// core/src/scheduler/task.rs

use alloc::vec::Vec;

/// List of terminated tasks waiting to be freed
/// Protected by IrqMutex for interrupt safety
static ZOMBIE_LIST: IrqMutex<ZombieList> = IrqMutex::new(ZombieList::new());

struct ZombieList {
    tasks: [Option<*mut Task>; MAX_TASKS],
    count: usize,
}

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
    
    fn drain_ready(&mut self) -> impl Iterator<Item = *mut Task> + '_ {
        let count = self.count;
        self.count = 0;
        
        (0..count).filter_map(move |i| {
            let task = self.tasks[i].take()?;
            
            // Check if task is ready to be freed
            if unsafe { (*task).ref_count() } == 0 {
                Some(task)
            } else {
                // Not ready - put it back
                self.tasks[self.count] = Some(task);
                self.count += 1;
                None
            }
        })
    }
}

/// Add a terminated task to the zombie list for deferred cleanup.
/// The task will be freed when its reference count reaches zero.
fn defer_task_cleanup(task: *mut Task) {
    if task.is_null() {
        return;
    }
    
    // Initial reference for being in zombie list
    unsafe { (*task).inc_ref() };
    
    ZOMBIE_LIST.lock().push(task);
}

/// Reap zombie tasks that are ready to be freed.
/// Should be called periodically (e.g., from timer interrupt or idle loop).
pub fn reap_zombies() {
    let mut list = ZOMBIE_LIST.lock();
    
    for task in list.drain_ready() {
        unsafe {
            klog_debug!("Reaping zombie task {}", (*task).task_id);
            
            // Free kernel stack
            if (*task).kernel_stack_base != 0 {
                kfree((*task).kernel_stack_base as *mut c_void);
                (*task).kernel_stack_base = 0;
            }
            
            // Free user stack (if kernel task)
            if (*task).process_id == INVALID_PROCESS_ID && (*task).stack_base != 0 {
                kfree((*task).stack_base as *mut c_void);
                (*task).stack_base = 0;
            }
            
            // Mark slot as invalid for reuse
            *task = Task::invalid();
        }
    }
}

/// Updated task_terminate to use deferred cleanup
pub fn task_terminate(task_id: u32) -> c_int {
    // ... (phases 1-3 code) ...
    
    if !is_current {
        unsafe {
            if (*task_ptr).process_id != INVALID_PROCESS_ID {
                // Clean up process-specific resources immediately
                fileio_destroy_table_for_process((*task_ptr).process_id);
                video_task_cleanup(resolved_id);
                shm_cleanup_task(resolved_id);
                destroy_process_vm((*task_ptr).process_id);
                
                // Defer memory cleanup until refcount is zero
                defer_task_cleanup(task_ptr);
            } else {
                // Kernel task - defer all cleanup
                defer_task_cleanup(task_ptr);
            }
        }
    }

    // ... (rest of termination) ...
}
```

---

## 6. Testing Strategy

### 6.1 Unit Tests

```rust
// core/src/scheduler/sched_tests.rs

#[test_case]
fn test_atomic_wait_single_waker() {
    // Setup: Task A waits on Task B
    // Action: Task B terminates
    // Verify: Task A is unblocked exactly once
}

#[test_case]
fn test_atomic_wait_concurrent_wakers() {
    // Setup: Task A waits on Task B
    // Action: Two CPUs try to wake A simultaneously (simulated)
    // Verify: Only one CAS succeeds, A enqueued exactly once
}

#[test_case]
fn test_remote_inbox_mpsc() {
    // Setup: Multiple tasks to wake on CPU 1
    // Action: CPU 0, 2, 3 push to CPU 1's inbox concurrently
    // Verify: All tasks appear in CPU 1's queue after drain
}

#[test_case]
fn test_zombie_reaper() {
    // Setup: Terminate task with non-zero refcount
    // Action: Call reap_zombies
    // Verify: Task not freed
    // Action: Decrement refcount to zero, call reap_zombies
    // Verify: Task freed
}
```

### 6.2 Stress Tests

```rust
#[test_case]
fn stress_terminate_while_compositor_runs() {
    // Pin compositor to AP (CPU 1)
    // Spawn 100 tasks that block on each other
    // Terminate them in rapid succession
    // Verify: No frame drops (measure via compositor timing)
    // Verify: All tasks properly cleaned up
}

#[test_case]
fn stress_concurrent_block_terminate() {
    // Spawn tasks that:
    // 1. Block on a target task
    // 2. While target terminates
    // Verify: No deadlocks, no double-enqueue, no UAF
}
```

### 6.3 Integration Tests

Add to `make test`:

```makefile
test-compositor-stability:
    # Run QEMU with compositor pinned to AP
    # Spawn/terminate 1000 tasks
    # Capture frame timing
    # Fail if any frame > 20ms
```

---

## 7. Success Criteria

### 7.1 Functional Requirements

| Requirement | Verification |
|-------------|--------------|
| No `pause_all_aps()` in task termination | Code review |
| Atomic `waiting_on` field | Code review |
| Single-winner wakeup via CAS | Unit tests |
| Lock-free cross-CPU wake | Unit tests |
| Deferred reclamation with refcount | Unit tests |
| All existing tests pass | `make test` |

### 7.2 Performance Requirements

| Metric | Target | Measurement |
|--------|--------|-------------|
| Compositor frame drops during termination | 0 | Frame timing logs |
| Task termination latency | < 100us | Benchmark |
| Cross-CPU wake latency | < 50us | Benchmark |
| Memory leaks | 0 | Long-running test |

### 7.3 Code Quality Requirements

| Requirement | Verification |
|-------------|--------------|
| No `unsafe` without safety comment | Code review |
| All atomics have explicit `Ordering` | Code review |
| No raw pointer dereference without bounds check | Code review |
| Follows existing code style | `cargo fmt --check` |
| No new warnings | `cargo build` |

---

## Appendix: Quick Wins

### A.1 Compositor Priority Bug Fix

**Current** (wrong):
```rust
// userland/src/bootstrap.rs:143
let compositor_id = userland_spawn_with_flags(b"compositor\0", 4, TASK_FLAG_COMPOSITOR);
```

**Fixed**:
```rust
let compositor_id = userland_spawn_with_flags(
    b"compositor\0", 
    TASK_PRIORITY_HIGH,  // 0, not 4
    TASK_FLAG_COMPOSITOR
);
```

### A.2 Compositor CPU Affinity

Consider pinning compositor to CPU 0 (BSP) which is never paused:

```rust
// userland/src/compositor.rs - at initialization
sys_core::set_cpu_affinity(1 << 0);  // Pin to CPU 0
```

### A.3 Documentation Updates

Update `plans/KNOWN_ISSUES.md` after implementation:

```markdown
## Performance: Compositor Frame Rate During Task Termination

**Status**: RESOLVED (v2.0)
**Resolution**: Replaced stop-the-world pause with lock-free atomic protocols

See `plans/COMPOSITOR_SAFE_TASK_CLEANUP.md` for implementation details.
```

---

## References

1. **Theseus OS**: https://github.com/theseus-os/Theseus (MIT License)
2. **Redox OS**: https://github.com/redox-os/kernel (MIT License)
3. **Crossbeam**: https://github.com/crossbeam-rs/crossbeam (Apache-2.0/MIT)
4. **Lock-Free Programming**: https://www.1024cores.net/home/lock-free-algorithms

---

*This plan represents the gold standard for Rust OS task management: atomic state machines, lock-free data structures, explicit ownership, and compile-time safety guarantees.*
