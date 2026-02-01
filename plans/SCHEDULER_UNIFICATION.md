# Scheduler Unification Plan

**Status**: Ready for Implementation  
**Priority**: Medium (functional, not urgent)  
**Last Updated**: 2026-02-01

---

## Current State

### What Works Now

Cross-CPU scheduling is **lock-free** and functional:

```
CPU A wants to schedule task on CPU B:
  1. push_remote_wake(task) → lock-free CAS to CPU B's inbox
  2. send_reschedule_ipi(CPU B) → wakes CPU B from hlt
  3. CPU B drains inbox on timer tick (~1ms) or loop iteration
```

All 364 tests pass. Boot works. No mutex contention on hot path.

### What's Still Asymmetric

| Aspect | BSP (CPU 0) | APs (CPU 1+) |
|--------|-------------|--------------|
| Entry | `start_scheduler()` → `loop { hlt }` | `scheduler_run_ap()` → `ap_scheduler_loop()` |
| Inbox drain | Timer tick only (~1ms latency) | Every loop iteration (continuous) |
| Work stealing | Does not participate | Active |
| State storage | Global `SchedulerInner` | Per-CPU `PerCpuScheduler` |
| `schedule()` | Uses global path | Uses `schedule_on_ap()` |
| Task execution | `prepare_switch()` + `do_context_switch()` | `ap_execute_task()` |

### Files Involved

```
core/src/scheduler/
├── scheduler.rs    # Main scheduler, has BSP vs AP split
├── per_cpu.rs      # PerCpuScheduler, inbox, AP-focused
├── work_steal.rs   # Work stealing (APs only currently)
└── sched_tests.rs  # Tests including new inbox tests
```

---

## Why This Matters

### Current Architecture is Fine For:
- General-purpose workloads
- Learning/hobby OS development
- Workloads without strict latency requirements

### Full Unification Would Help:
- Real-time or latency-sensitive workloads (BSP inbox: 1ms → continuous)
- Code maintainability (one path instead of two)
- BSP participating in work stealing
- Eliminating duplicated state (`SchedulerInner` vs `PerCpuScheduler`)

---

## Unification Phases (Incremental, No Dead Code)

Each phase is atomic: changes and deletions happen together. No legacy code remains between phases. `make test` must pass after each phase.

### Phase 1: BSP Participates in Work Stealing

**Goal**: BSP joins the load balancing dance.

**Changes**:
- Add `try_work_steal()` call to BSP's `select_next_task()` when local queues empty
- Import work_steal module in BSP path

**Deletes**: None (pure addition)

**Effort**: ~1 hour  
**Risk**: Low - additive change only

```rust
fn select_next_task(sched: &mut SchedulerInner) -> *mut Task {
    // ... existing dequeue logic ...
    
    if next.is_null() {
        // NEW: BSP tries work stealing before falling back to idle
        if try_work_steal() {
            next = per_cpu::with_cpu_scheduler(cpu_id, |local| {
                local.dequeue_highest_priority()
            }).unwrap_or(ptr::null_mut());
        }
    }
    
    // ... idle fallback ...
}
```

---

### Phase 2: Unify `schedule()` and `schedule_on_ap()`

**Goal**: Single scheduling path for all CPUs.

Both functions do the same thing:
1. Re-queue current task if running
2. Select next task  
3. Context switch

**Changes**:
- Refactor `schedule()` to use `PerCpuScheduler` for ALL CPUs
- Remove the `if cpu_id != 0 { schedule_on_ap(); return; }` branch
- Inline the AP logic into the unified path

**Deletes**: `schedule_on_ap()` (in same commit)

**Effort**: ~3 hours  
**Risk**: Medium - core scheduling path change

```rust
pub fn schedule() {
    let cpu_id = slopos_lib::get_current_cpu();
    let preempt_guard = PreemptGuard::new();
    
    // UNIFIED: Same logic for BSP and APs
    let current = per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.current_task())
        .unwrap_or(ptr::null_mut());
    
    // Re-queue current if running...
    // Select next task...
    // Context switch...
}
```

---

### Phase 3: Unify Task Execution

**Goal**: Single task execution function for all CPUs.

Currently:
- BSP: `prepare_switch()` + `do_context_switch()`
- APs: `ap_execute_task()`

**Changes**:
- Create unified `execute_task(cpu_id, from_task, to_task)` 
- Handles both kernel and user mode
- Handles page directory switching, TSS updates, context validation
- Update both BSP and AP paths to use it

**Deletes**: `ap_execute_task()` (in same commit)

**Effort**: ~3 hours  
**Risk**: Medium - context switch is sensitive

```rust
fn execute_task(cpu_id: usize, from_task: *mut Task, to_task: *mut Task) {
    // Record context switch timestamp
    // Update per-CPU current_task
    // Set up page directory
    // Set TSS RSP0
    // Validate user context if needed
    // context_switch() or context_switch_user()
}
```

---

### Phase 4: Unify Scheduler Loop and Entry Point

**Goal**: All CPUs use the same scheduler loop structure.

Currently:
- BSP: `start_scheduler()` → dispatch idle → `loop { hlt }`
- APs: `scheduler_run_ap()` → stack switch → `ap_scheduler_loop()`

**Changes**:
- Create unified `scheduler_loop(cpu_id)` with continuous inbox drain + work stealing
- Create unified `enter_scheduler(cpu_id)` that does stack transition for ALL CPUs
- BSP switches from Limine boot stack to idle task's kernel stack (like APs already do)

**Deletes**: `start_scheduler()`, `ap_scheduler_loop()` (in same commit)

**Effort**: ~4 hours  
**Risk**: High - boot sequence change, stack transition

```rust
pub fn enter_scheduler(cpu_id: usize) -> ! {
    // Enable per-CPU scheduler
    per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.enable());
    
    let idle_task = per_cpu::with_cpu_scheduler(cpu_id, |s| s.idle_task())
        .unwrap_or(ptr::null_mut());
    
    // CRITICAL: Switch from boot stack to idle task's kernel stack
    // Boot stack is HHDM-mapped and transient - would crash on restore
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
    
    scheduler_loop(cpu_id, idle_task)
}

fn scheduler_loop(cpu_id: usize, idle_task: *mut Task) -> ! {
    loop {
        // 1. Drain remote inbox (continuous, not just timer tick)
        per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.drain_remote_inbox());
        
        // 2. Check for pause (test reinitialization)
        if per_cpu::are_aps_paused() {
            unsafe { core::arch::asm!("sti; hlt; cli", options(nomem, nostack)); }
            continue;
        }
        
        // 3. Dequeue highest priority task
        let next = per_cpu::with_cpu_scheduler(cpu_id, |sched| {
            sched.dequeue_highest_priority()
        }).unwrap_or(ptr::null_mut());
        
        // 4. Execute task or try work stealing
        if !next.is_null() {
            execute_task(cpu_id, idle_task, next);
        } else if !try_work_steal() {
            // 5. Nothing to do - halt until interrupt
            unsafe { core::arch::asm!("sti; hlt; cli", options(nomem, nostack)); }
        }
    }
}
```

---

### Phase 5: Eliminate `SchedulerInner`

**Goal**: Single source of truth - only `PerCpuScheduler`.

After phases 2-4, `SchedulerInner` is largely redundant.

**Changes**:
- Move global settings (`preemption_enabled`) to standalone atomics
- Update all 25 `with_scheduler()` calls to use `with_cpu_scheduler()`
- Stats aggregation queries all per-CPU schedulers

**Deletes**: `SchedulerInner`, `SCHEDULER`, `with_scheduler()`, `try_with_scheduler()` (in same commit)

**Effort**: ~3 hours  
**Risk**: Medium - many call sites to update

| Before | After |
|--------|-------|
| `with_scheduler(\|s\| s.current_task)` | `with_cpu_scheduler(cpu_id, \|s\| s.current_task())` |
| `with_scheduler(\|s\| s.enabled)` | `with_cpu_scheduler(cpu_id, \|s\| s.is_enabled())` |
| `with_scheduler(\|s\| s.total_switches)` | Aggregate from all per-CPU schedulers |
| `sched.preemption_enabled` | `PREEMPTION_ENABLED.load(Ordering::Acquire)` |

---

## Summary

| Phase | Change | Delete | Effort | Risk |
|-------|--------|--------|--------|------|
| 1 | BSP work stealing | - | ~1h | Low |
| 2 | Unify `schedule()` | `schedule_on_ap()` | ~3h | Medium |
| 3 | Unify task execution | `ap_execute_task()` | ~3h | Medium |
| 4 | Unify loop + entry | `start_scheduler()`, `ap_scheduler_loop()` | ~4h | High |
| 5 | Eliminate global state | `SchedulerInner`, `with_scheduler()` | ~3h | Medium |

**Total**: ~14 hours of careful work

---

## Verification

### Before Starting
```bash
make test        # Must pass 364/364
make boot VIDEO=1  # Must render compositor
```

### After Each Phase
```bash
make test        # Still 364/364
make boot-log    # No panics, check test_output.log
```

### After Full Unification
```bash
# No CPU-specific branches in hot path
grep -rn "cpu_id == 0\|cpu_id != 0" core/src/scheduler/
# Should find 0 results in scheduling logic (only in AP pause mechanism)

# Unified functions exist
grep -n "scheduler_loop\|enter_scheduler\|execute_task" core/src/scheduler/scheduler.rs

# No legacy functions
grep -n "schedule_on_ap\|ap_scheduler_loop\|ap_execute_task\|start_scheduler" core/src/scheduler/
# Should find 0 results
```

---

## Recent Changes (2026-02-01)

Applied partial fix (lock-free cross-CPU scheduling):

1. **Timer tick drains inbox for ALL CPUs** (`scheduler_timer_tick()`)
   - BSP now processes remote wakes at timer tick frequency
   
2. **Lock-free cross-CPU scheduling** (`schedule_task()`)
   - Same-CPU: `enqueue_local()` directly
   - Cross-CPU: `push_remote_wake()` + IPI

3. **Tests added**:
   - `test_remote_inbox_push_drain`
   - `test_remote_inbox_multiple_tasks`
   - `test_timer_tick_drains_inbox`
   - `test_cross_cpu_schedule_lockfree`
