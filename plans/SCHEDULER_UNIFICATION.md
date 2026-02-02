# Scheduler Unification Plan

**Status**: ✅ COMPLETE  
**Priority**: Medium (functional, not urgent)  
**Completed**: 2026-02-02

---

## Implementation Summary

All 5 phases completed successfully. The scheduler now uses a unified architecture for BSP and APs.

| Phase | Description | Commit | Status |
|-------|-------------|--------|--------|
| 1 | BSP Participates in Work Stealing | `2d8a57d30` | ✅ |
| 2 | Unify `schedule()`, delete `schedule_on_ap()` | `1f8e8ae84` | ✅ |
| 3 | Unified `execute_task()`, delete `ap_execute_task()` | `42ac4c24e` | ✅ |
| 4 | Unified scheduler loop and entry point | `36f654f27` | ✅ |
| 5 | Lock-free atomics for hot-path checks | `5529ed48c` | ✅ |

All 364 tests pass after each phase.

---

## What Changed

### Before (Asymmetric)

| Aspect | BSP (CPU 0) | APs (CPU 1+) |
|--------|-------------|--------------|
| Entry | `start_scheduler()` → `loop { hlt }` | `scheduler_run_ap()` → `ap_scheduler_loop()` |
| Inbox drain | Timer tick only (~1ms latency) | Every loop iteration (continuous) |
| Work stealing | Did not participate | Active |
| State storage | Global `SchedulerInner` | Per-CPU `PerCpuScheduler` |
| `schedule()` | Used global path | Used `schedule_on_ap()` |
| Task execution | `prepare_switch()` + `do_context_switch()` | `ap_execute_task()` |

### After (Unified)

| Aspect | All CPUs |
|--------|----------|
| Entry | `enter_scheduler(cpu_id)` |
| Main loop | `scheduler_loop(cpu_id, idle_task)` |
| Inbox drain | Continuous (every loop iteration) |
| Work stealing | All CPUs participate |
| Task execution | `execute_task(cpu_id, from_task, to_task)` |
| Hot-path checks | Lock-free atomics (`SCHEDULER_ENABLED`, `PREEMPTION_ENABLED`) |

### Key Differences Preserved

- **BSP**: Stays on .bss boot stack (stable kernel memory), sets global `SchedulerInner.current_task`
- **APs**: Switch from HHDM boot stack to idle's kernel stack

---

## Deleted Code

| Function | Phase | Lines Removed |
|----------|-------|---------------|
| `schedule_on_ap()` | 2 | ~50 |
| `ap_execute_task()` | 3 | ~80 |
| `ap_scheduler_loop()` | 4 | ~130 |

`start_scheduler()` retained as thin wrapper around `enter_scheduler(0)` for API compatibility.

---

## Files Modified

```
core/src/scheduler/scheduler.rs   # Main changes
```

---

## Phase Details

### Phase 1: BSP Participates in Work Stealing

Added `try_work_steal()` call to BSP's `select_next_task()` when local and global queues are empty.

### Phase 2: Unify `schedule()` and delete `schedule_on_ap()`

- Inlined AP scheduling logic into unified `schedule()` function
- Both BSP and APs now get current/idle from per-CPU scheduler
- Re-queue to per-CPU instead of global queue

### Phase 3: Unify Task Execution

Created unified `execute_task(cpu_id, from_task, to_task)` handling:
- Context switch timestamp recording
- Per-CPU current_task updates
- Page directory setup with kernel mappings sync
- TSS RSP0 setup
- context_from_user handling
- User context validation

### Phase 4: Unify Scheduler Loop and Entry Point

- Created `enter_scheduler(cpu_id)` as unified entry for all CPUs
- Created `scheduler_loop(cpu_id, idle_task)` as unified main loop
- BSP sets current_task before loop; APs rely on execute_task
- Fixed bug: `execute_task()` now syncs global `SchedulerInner.current_task` for BSP

### Phase 5: Lock-free Atomics for Hot-Path Checks

- Added `SCHEDULER_ENABLED` and `PREEMPTION_ENABLED` standalone atomics
- Added `is_scheduling_active()` helper for lock-free hot-path checks
- Migrated `deferred_reschedule_callback`, `scheduler_request_reschedule_from_interrupt`, and `scheduler_handle_post_irq` to use atomics
- `SchedulerInner` retained for stats, return_context, and global queues

---

## Verification Commands

```bash
# Tests pass
make test        # 364/364

# Boot works with graphics
make boot VIDEO=1

# Check unified functions exist
grep -n "scheduler_loop\|enter_scheduler\|execute_task" core/src/scheduler/scheduler.rs

# Legacy AP functions removed
grep -n "schedule_on_ap\|ap_scheduler_loop\|ap_execute_task" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

## Future Work (Optional)

1. **Full `SchedulerInner` elimination**: Move remaining state (stats, return_context, global queues) to per-CPU or standalone atomics
2. **Remove BSP special cases**: Make BSP switch stacks like APs (requires careful boot sequence changes)
3. **Unified idle handling**: Currently BSP and APs have slightly different idle behavior
