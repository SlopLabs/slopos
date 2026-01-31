# Unified Per-CPU Scheduler Architecture

**Status**: Planned  
**Priority**: Medium  
**Estimated Effort**: 3-5 days  
**Prerequisite**: COMPOSITOR_SAFE_TASK_CLEANUP.md (partially implemented)  
**Author**: AI Analysis based on Theseus OS, Redox OS architectures  
**Date**: 2026-01-31

---

## Executive Summary

The current SlopOS scheduler has an **asymmetric architecture** where CPU 0 (BSP) operates fundamentally differently from APs. This prevents proper use of lock-free cross-CPU scheduling primitives like the remote wake inbox.

This plan unifies all CPUs under a single per-CPU scheduler model, eliminating the special-cased BSP path and enabling true lock-free cross-CPU task wakeups.

---

## Problem Analysis

### Current Architecture (Broken)

```
┌─────────────────────────────────────────────────────────────────┐
│                        CPU 0 (BSP)                               │
├─────────────────────────────────────────────────────────────────┤
│  Global SchedulerInner                                           │
│  ├── current_task, idle_task, return_context                    │
│  ├── ready_queues[4]                                            │
│  └── Direct task execution (no polling loop)                    │
│                                                                  │
│  schedule() called only on:                                      │
│  - Task yield                                                    │
│  - Task block                                                    │
│  - Timer preemption                                              │
│                                                                  │
│  ❌ Remote inbox NEVER drained (compositor runs at 1 FPS)       │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│                        APs (CPU 1+)                              │
├─────────────────────────────────────────────────────────────────┤
│  PerCpuScheduler (per CPU)                                       │
│  ├── current_task, idle_task                                    │
│  ├── ready_queues[4]                                            │
│  ├── remote_inbox_head (lock-free MPSC)                         │
│  └── Continuous ap_scheduler_loop() polling                     │
│                                                                  │
│  ✓ Remote inbox drained every loop iteration                    │
│  ✓ Lock-free cross-CPU wakeups work correctly                   │
└─────────────────────────────────────────────────────────────────┘
```

### Why This Is Wrong

1. **Two different scheduler models** - BSP uses global state, APs use per-CPU state
2. **No continuous loop on BSP** - `schedule()` is event-driven, not polling
3. **Remote inbox unusable for CPU 0** - Tasks get stuck, never processed
4. **Quickfix required** - Must use direct `enqueue_local()` with mutex contention
5. **Comments needed to explain non-obvious constraints** - Architecture smell

### Gold Standard (Theseus OS / Redox OS)

```
┌─────────────────────────────────────────────────────────────────┐
│                     ALL CPUs (Unified)                           │
├─────────────────────────────────────────────────────────────────┤
│  PerCpuScheduler (identical on every CPU)                        │
│  ├── current_task, idle_task                                    │
│  ├── ready_queues[N]                                            │
│  ├── remote_inbox_head (lock-free MPSC)                         │
│  └── Continuous scheduler_loop() on ALL CPUs                    │
│                                                                  │
│  Every CPU:                                                      │
│  1. Drains remote inbox                                          │
│  2. Picks highest priority task                                  │
│  3. Runs task until yield/block/preempt                         │
│  4. Returns to scheduler loop                                    │
│                                                                  │
│  ✓ Symmetric architecture                                        │
│  ✓ Lock-free cross-CPU wakeups on ALL CPUs                      │
│  ✓ No special cases, no comments needed                         │
└─────────────────────────────────────────────────────────────────┘
```

---

## Design Goals

1. **Eliminate global SchedulerInner** - All state moves to PerCpuScheduler
2. **BSP uses same loop as APs** - `cpu_scheduler_loop()` for all CPUs
3. **Lock-free cross-CPU wakeups everywhere** - Remote inbox works on CPU 0
4. **Remove quickfix** - `schedule_task()` uses `push_remote_wake()` for cross-CPU
5. **Simplify code** - Delete special-case BSP paths

---

## Implementation Phases

### Phase 1: Migrate Global State to Per-CPU
**Effort**: 4 hours  
**Risk**: Medium

Move remaining global `SchedulerInner` fields to `PerCpuScheduler`:

```rust
// BEFORE: Global state (only works for CPU 0)
struct SchedulerInner {
    ready_queues: [ReadyQueue; 4],
    current_task: *mut Task,
    idle_task: *mut Task,
    return_context: TaskContext,
    // ... stats ...
}

// AFTER: All state in PerCpuScheduler (works for all CPUs)
pub struct PerCpuScheduler {
    pub cpu_id: usize,
    ready_queues: [ReadyQueue; 4],
    current_task_atomic: AtomicPtr<Task>,
    idle_task_atomic: AtomicPtr<Task>,
    pub return_context: TaskContext,
    remote_inbox_head: AtomicPtr<Task>,
    // ... stats ...
}
```

**Changes:**
- `SchedulerInner` becomes a thin wrapper or is eliminated
- `with_scheduler()` delegates to `with_cpu_scheduler(0, ...)`
- Global `SCHEDULER` static remains for backward compatibility but uses CPU 0's per-CPU state

### Phase 2: Unify Scheduler Loop Entry
**Effort**: 4 hours  
**Risk**: Medium

Create a single `cpu_scheduler_loop()` that works for ALL CPUs:

```rust
/// Unified scheduler loop for all CPUs (BSP and APs)
pub fn cpu_scheduler_loop(cpu_id: usize) -> ! {
    let idle_task = per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task())
        .expect("CPU must have idle task");

    loop {
        // 1. Drain remote inbox (lock-free)
        per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.drain_remote_inbox();
        });

        // 2. Check for pause (test infrastructure only)
        if per_cpu::are_aps_paused() && cpu_id != 0 {
            unsafe { core::arch::asm!("sti; hlt; cli", options(nomem, nostack)); }
            continue;
        }

        // 3. Pick next task
        let next_task = per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.dequeue_highest_priority()
        }).unwrap_or(ptr::null_mut());

        // 4. Execute or idle
        if !next_task.is_null() && task_is_ready(next_task) {
            execute_task(cpu_id, idle_task, next_task);
        } else {
            // Try work stealing, then halt
            if !try_work_steal(cpu_id) {
                per_cpu::with_cpu_scheduler(cpu_id, |s| s.increment_idle_time());
                unsafe { core::arch::asm!("sti; hlt; cli", options(nomem, nostack)); }
            }
        }
    }
}
```

**Changes:**
- Rename `ap_scheduler_loop()` to `cpu_scheduler_loop()`
- BSP calls `cpu_scheduler_loop(0)` after initialization
- Delete `schedule()` function (or make it trigger return to loop)

### Phase 3: Refactor schedule() to Return-to-Loop
**Effort**: 3 hours  
**Risk**: High

The current `schedule()` does inline context switching. Change it to return control to the scheduler loop:

```rust
// BEFORE: schedule() picks next task and switches inline
pub fn schedule() {
    let next = select_next_task();
    context_switch(current, next);  // Complex inline switching
}

// AFTER: schedule() signals "return to loop" and the loop handles switching
pub fn schedule() {
    let cpu_id = get_current_cpu();
    
    // Re-queue current task if runnable
    let current = per_cpu::with_cpu_scheduler(cpu_id, |s| s.current_task());
    if !current.is_null() && task_is_running(current) {
        task_set_state(current.task_id, TASK_STATE_READY);
        per_cpu::with_cpu_scheduler(cpu_id, |s| s.enqueue_local(current));
    }
    
    // Return to scheduler loop via idle task context
    return_to_scheduler_loop(cpu_id);
}

fn return_to_scheduler_loop(cpu_id: usize) {
    let idle_task = per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task());
    // Context switch to idle, which returns to cpu_scheduler_loop
    context_switch_to_idle(idle_task);
}
```

**Key insight:** The idle task's saved context points back into `cpu_scheduler_loop()`. Switching to idle == returning to the loop.

### Phase 4: BSP Initialization Changes
**Effort**: 2 hours  
**Risk**: Medium

Modify BSP boot sequence to enter the unified loop:

```rust
// kernel/src/main.rs or equivalent
pub fn kernel_main() -> ! {
    // ... early init, memory, drivers ...
    
    // Initialize per-CPU scheduler for BSP
    per_cpu::init_cpu_scheduler(0);
    
    // Create BSP idle task (same as AP idle tasks)
    let idle_task = create_idle_task(0);
    per_cpu::with_cpu_scheduler(0, |s| s.set_idle_task(idle_task));
    
    // Start APs (they enter cpu_scheduler_loop independently)
    smp::start_aps();
    
    // Create initial userland tasks
    userland::spawn_init();
    
    // BSP enters the SAME loop as APs
    cpu_scheduler_loop(0);
}
```

### Phase 5: Restore Lock-Free Cross-CPU Scheduling
**Effort**: 1 hour  
**Risk**: Low

With unified architecture, restore the optimal `schedule_task()`:

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    // ... validation ...
    
    let target_cpu = select_target_cpu(task);
    let current_cpu = get_current_cpu();

    if target_cpu == current_cpu {
        // Same CPU: direct local enqueue
        per_cpu::with_cpu_scheduler(target_cpu, |s| s.enqueue_local(task));
    } else {
        // Different CPU: lock-free remote inbox
        per_cpu::with_cpu_scheduler(target_cpu, |s| s.push_remote_wake(task));
        send_reschedule_ipi(target_cpu);
    }
    0
}
```

No more quickfix needed - CPU 0 drains its inbox in the loop just like APs.

### Phase 6: Cleanup and Delete Dead Code
**Effort**: 2 hours  
**Risk**: Low

Remove:
- `schedule_on_ap()` - replaced by unified `return_to_scheduler_loop()`
- Global `SchedulerInner` (or reduce to compatibility shim)
- Special-case BSP comments
- `ap_scheduler_loop()` and `ap_execute_task()` - merged into `cpu_scheduler_loop()`

---

## Migration Strategy

### Backward Compatibility

During migration, maintain:
```rust
// Compatibility shim - delegates to CPU 0's per-CPU scheduler
pub fn with_scheduler<F, R>(f: F) -> R 
where F: FnOnce(&mut SchedulerInner) -> R 
{
    with_cpu_scheduler(0, |percpu| {
        // Wrap percpu in SchedulerInner-compatible interface
        f(&mut SchedulerInnerCompat(percpu))
    })
}
```

### Testing Checkpoints

After each phase, verify:
1. `make test` passes (360/360)
2. `make boot VIDEO=1` - compositor renders at 60fps
3. No "quickfix" comments remain in code

---

## Success Criteria

| Requirement | Verification |
|-------------|--------------|
| BSP uses same scheduler loop as APs | Code review |
| Remote inbox drained on all CPUs | Code review |
| Lock-free cross-CPU schedule_task() | Code review |
| No special-case BSP code paths | grep for "cpu_id == 0" or "cpu_id != 0" in scheduler |
| Compositor 60fps during task termination | Manual test |
| All tests pass | `make test` |

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| BSP boot sequence changes | High - could break boot | Incremental changes, test after each step |
| Context switch semantics change | High - could corrupt state | Extensive testing, keep old code path until verified |
| Idle task behavior differences | Medium - could hang CPU | Ensure idle task context returns to loop correctly |
| Timer tick handling | Medium - preemption could break | Verify timer tick works with new loop structure |

---

## References

1. **Theseus OS** - https://github.com/theseus-os/Theseus
   - See `kernel/scheduler/` for unified per-CPU design
   - All CPUs use identical `schedule()` path

2. **Redox OS** - https://github.com/redox-os/kernel  
   - See `src/context/` for per-CPU context management
   - Symmetric multiprocessing throughout

3. **Current SlopOS Quickfix** - `core/src/scheduler/scheduler.rs:schedule_task()`
   - Uses direct `enqueue_local()` for all CPUs
   - Works but has mutex contention on cross-CPU scheduling

---

## Appendix: Current Quickfix Location

The quickfix that this plan eliminates is in `core/src/scheduler/scheduler.rs`:

```rust
pub fn schedule_task(task: *mut Task) -> c_int {
    // ... 
    // Currently uses enqueue_local() for ALL CPUs because
    // CPU 0 doesn't drain its remote inbox.
    // After this plan: use push_remote_wake() for cross-CPU.
    let result = per_cpu::with_cpu_scheduler(target_cpu, |sched| sched.enqueue_local(task));
    // ...
}
```

---

*This plan transforms SlopOS from an asymmetric BSP-special scheduler to a symmetric per-CPU design matching production Rust operating systems.*
