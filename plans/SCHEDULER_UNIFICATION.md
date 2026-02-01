# Scheduler Unification Plan

**Status**: Partial Fix Applied  
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

### Files Involved

```
core/src/scheduler/
├── scheduler.rs    # Main scheduler, has BSP vs AP split
├── per_cpu.rs      # PerCpuScheduler, inbox, AP-focused
├── work_steal.rs   # Work stealing (APs only currently)
└── sched_tests.rs  # Tests including new inbox tests
```

---

## Why This Matters (Or Doesn't)

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

## Full Unification: What's Required

### Phase 1: Unified Scheduler Loop

Create single `scheduler_loop()` that ALL CPUs use:

```rust
pub fn scheduler_loop(cpu_id: usize) -> ! {
    loop {
        drain_remote_inbox();
        if let Some(task) = dequeue_task() {
            execute_task(task);
        } else if !try_work_steal() {
            halt_until_interrupt();
        }
    }
}
```

**Effort**: ~4 hours  
**Risk**: Medium - changes core scheduling path

### Phase 2: BSP Boot Stack Transition

BSP must switch from Limine boot stack to idle task's kernel stack before entering loop:

```rust
pub fn enter_scheduler(cpu_id: usize) -> ! {
    let idle_stack_top = get_idle_task(cpu_id).kernel_stack_top;
    
    // CRITICAL: Switch from transient boot stack to persistent idle stack
    unsafe { asm!("mov rsp, {}", in(reg) idle_stack_top); }
    
    scheduler_loop(cpu_id)
}
```

**Why**: Boot stack is HHDM-mapped, transient. Saving its address in context_switch would crash later.

**Effort**: ~2 hours  
**Risk**: High - boot sequence change, easy to break

### Phase 3: Eliminate SchedulerInner

Remove global `SchedulerInner` struct, use only `PerCpuScheduler`:

| Before | After |
|--------|-------|
| `with_scheduler()` → global mutex | `with_local_scheduler()` → per-CPU direct |
| `SchedulerInner.ready_queues` | Only `PerCpuScheduler.ready_queues` |
| `SchedulerInner.current_task` | Only `PerCpuScheduler.current_task` |

**Effort**: ~3 hours  
**Risk**: Medium - many callers to update

### Phase 4: Delete Old Code

Remove:
- `start_scheduler()` 
- `schedule_on_ap()`
- `ap_scheduler_loop()`
- `ap_execute_task()`
- All `if cpu_id == 0` / `if cpu_id != 0` checks in scheduler

**Effort**: ~2 hours  
**Risk**: Low - just deletion after verification

---

## Verification

### Before Starting
```bash
make test  # Must pass 364/364
make boot VIDEO=1  # Must render compositor
```

### After Each Phase
```bash
make test  # Still 364/364
make boot-log  # No panics
grep -rn "cpu_id == 0" core/src/scheduler/  # Should decrease
```

### After Full Unification
```bash
grep -rn "cpu_id == 0\|cpu_id != 0" core/src/scheduler/
# Should find 0 results in scheduling hot path

grep -n "scheduler_loop\|enter_scheduler" core/src/scheduler/scheduler.rs
# Should find unified functions
```

---

## Decision

**Do Nothing**: Current state is functional. Lock-free cross-CPU scheduling works. 1ms inbox latency on BSP is acceptable for most workloads.

**Do Full Unification**: If you want gold-standard architecture, cleaner code, or need lower latency. Estimate: 2-3 days of careful work.

---

## Recent Changes (2026-02-01)

Applied partial fix:

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
