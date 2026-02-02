# Scheduler Full Symmetry Plan

**Status**: 📋 PLANNED  
**Priority**: Low (cleanup, no functional impact)  
**Prerequisite**: Scheduler Unification (COMPLETE)

---

## Overview

The scheduler unification is complete - all CPUs use `enter_scheduler(cpu_id)` and `scheduler_loop()`. However, several asymmetries remain:

1. **`SchedulerInner` duplication**: BSP still uses a global `SchedulerInner` struct alongside per-CPU state
2. **BSP special cases**: 14 places check `cpu_id == 0` for different behavior
3. **Idle task differences**: BSP and APs use different idle loop implementations

This plan eliminates these asymmetries via instant replacement (no migration period).

---

## Workflow Rules

### Per-Phase Process

1. Implement the phase changes
2. Run `make test` - all 364 tests must pass
3. User tests boot manually and reports result
4. If issues: fix and iterate until user confirms working
5. Commit the phase
6. Proceed to next phase

### Code Rules

- **No plan-specific comments**: Do not add comments like "Phase 2" or "Part of scheduler symmetry plan" in code
- **No migration code**: Each phase deletes old code immediately, no temporary compatibility layers
- **Clean commits**: Each commit should read as a standalone improvement, not reference the plan

---

## Current State Analysis

### SchedulerInner Fields (BSP-only global state)

| Field | Purpose | Elimination |
|-------|---------|-------------|
| `ready_queues` | Global fallback queues | Delete (per-CPU queues sufficient) |
| `current_task` | BSP current task pointer | Delete (use per-CPU only) |
| `idle_task` | BSP idle task pointer | Delete (use per-CPU only) |
| `policy` | Scheduling policy (always cooperative) | Delete (unused) |
| `enabled` | Scheduler enabled flag | Delete (use `SCHEDULER_ENABLED` atomic) |
| `preemption_enabled` | Preemption flag | Delete (use `PREEMPTION_ENABLED` atomic) |
| `time_slice` | Default time slice | Delete (use constant) |
| `return_context` | BSP return context | Delete (use per-CPU) |
| `total_switches` | Context switch counter | Delete (aggregate from per-CPU) |
| `total_yields` | Yield counter | Delete (aggregate from per-CPU) |
| `idle_time` | Idle time counter | Delete (aggregate from per-CPU) |
| `total_ticks` | Timer tick counter | Delete (aggregate from per-CPU) |
| `total_preemptions` | Preemption counter | Delete (aggregate from per-CPU) |
| `schedule_calls` | Schedule call counter | Delete (aggregate from per-CPU) |

### BSP Special Cases (14 locations)

| Location | Line | Purpose |
|----------|------|---------|
| `execute_task` | 534 | Sync global `current_task` |
| `schedule` | 639 | AP path vs BSP path |
| `yield` | 771 | BSP uses global stats |
| `scheduler_task_exit_impl` | 923 | AP calls `ap_task_exit_to_idle` |
| `scheduler_task_exit_impl` | 942 | Sync global `current_task` |
| `scheduler_task_exit_impl` | 949 | AP-specific exit |
| `create_idle_task_for_cpu` | 1094 | BSP sets global idle_task |
| `enter_scheduler` | 1109 | Double-enable guard |
| `enter_scheduler` | 1121 | BSP init global state |
| `enter_scheduler` | 1154 | BSP stays on boot stack |
| `scheduler_loop` | 1231 | BSP syncs global `current_task` |
| `scheduler_timer_tick` | 1366 | BSP uses global path |
| `init_scheduler_for_ap` | 1553 | AP-only init |

### Idle Task Differences

| Aspect | BSP (`idle_task_function`) | AP (`ap_idle_loop`) |
|--------|---------------------------|---------------------|
| Wakeup callback | Checks `IDLE_WAKEUP_CB` | None |
| Stats | Uses global `SchedulerInner.idle_time` | Uses per-CPU `idle_time` |
| Yield | Every 1000 iterations | Never |
| Halt | `hlt` | `sti; hlt; cli` |

---

## Implementation Phases

Each phase is atomic: implement unified code AND delete old code in the same commit.

### Phase 1: Delete `current_task`/`idle_task` from SchedulerInner

**Delete**:
- `SchedulerInner.current_task` field
- `SchedulerInner.idle_task` field
- All `sched.current_task = ...` sync statements
- All `sched.idle_task = ...` sync statements

**Update**:
- `scheduler_get_current_task()` → use per-CPU only
- `create_idle_task_for_cpu()` → use per-CPU only
- Remove BSP special cases in `execute_task`, `scheduler_loop`, `scheduler_task_exit_impl`

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "sched\.current_task\|sched\.idle_task" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

### Phase 2: Delete `ready_queues` from SchedulerInner

**Delete**:
- `SchedulerInner.ready_queues` field
- `SchedulerInner.enqueue_task()` method
- `SchedulerInner.dequeue_highest_priority()` method
- `SchedulerInner.remove_task()` method
- `SchedulerInner.total_ready_count()` method
- Global `ReadyQueue` struct (keep per-CPU version)

**Update**:
- `select_next_task()` → use per-CPU queues only
- `schedule_task()` → always use per-CPU queues
- `unschedule_task()` → use per-CPU only

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "sched\.ready_queues\|sched\.enqueue\|sched\.dequeue" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

### Phase 3: Delete duplicate flags from SchedulerInner

**Delete**:
- `SchedulerInner.enabled` field (use `SCHEDULER_ENABLED` atomic)
- `SchedulerInner.preemption_enabled` field (use `PREEMPTION_ENABLED` atomic)
- `SchedulerInner.policy` field (unused, always cooperative)
- `SchedulerInner.time_slice` field (use constant `SCHED_DEFAULT_TIME_SLICE`)

**Update**:
- `init_scheduler()` → remove duplicate flag initialization
- `scheduler_set_preemption_enabled()` → use atomic only
- `enter_scheduler()` → use atomic only
- `stop_scheduler()` → use atomic only

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "sched\.enabled\|sched\.preemption_enabled\|sched\.policy" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

### Phase 4: Unify idle tasks, delete `idle_task_function`

**Delete**:
- `idle_task_function()` in scheduler.rs
- `ap_idle_loop()` in per_cpu.rs

**Create**:
- Single `unified_idle_loop()` for all CPUs
- Use per-CPU stats for all CPUs
- Use consistent halt sequence (`sti; hlt; cli`)
- Support wakeup callback for all CPUs

**Update**:
- `create_idle_task()` → use `unified_idle_loop`
- `create_ap_idle_task()` → use `unified_idle_loop`

**Files**: 
- `core/src/scheduler/scheduler.rs`
- `core/src/scheduler/per_cpu.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "idle_task_function\|ap_idle_loop" core/src/scheduler/*.rs
# Should find 0 results
```

---

### Phase 5: Unify `schedule()`, delete BSP/AP branches

**Delete**:
- `// === AP PATH ===` branch in `schedule()`
- `// === BSP PATH ===` branch in `schedule()`
- `ScheduleResult` enum (BSP-specific)

**Create**:
- Single unified `schedule()` path for all CPUs
- Use per-CPU state consistently

**Update**:
- `yield()` → use per-CPU stats for all CPUs

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "AP PATH\|BSP PATH\|cpu_id != 0\|cpu_id == 0" core/src/scheduler/scheduler.rs
# Should find minimal results (only intentional ones like double-enable guard)
```

---

### Phase 6: Unify task exit, delete `ap_task_exit_to_idle`

**Delete**:
- `ap_task_exit_to_idle()` function
- BSP/AP branches in `scheduler_task_exit_impl()`

**Create**:
- Single unified task exit path for all CPUs

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "ap_task_exit_to_idle" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

### Phase 7: Unify timer tick, delete `scheduler_timer_tick_ap`

**Delete**:
- `scheduler_timer_tick_ap()` function
- BSP/AP branch in `scheduler_timer_tick()`

**Create**:
- Single unified timer tick handler for all CPUs

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "scheduler_timer_tick_ap" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

### Phase 8: BSP stack switch in `enter_scheduler`

**Delete**:
- `cpu_id != 0` conditional for stack switch in `enter_scheduler()`

**Update**:
- BSP switches to idle task's kernel stack like APs
- Allocate proper kernel stack for BSP idle task

**Files**: 
- `core/src/scheduler/scheduler.rs`
- `core/src/scheduler/per_cpu.rs`

**Verification**:
```bash
make test  # 364/364
```

---

### Phase 9: Delete SchedulerInner, delete stats fields

**Delete**:
- `SchedulerInner` struct entirely
- `SCHEDULER` static
- `with_scheduler()` helper
- `try_with_scheduler()` helper
- `SchedulerInner.return_context` (use per-CPU)
- All stats fields (`total_switches`, `total_yields`, etc.)

**Update**:
- `get_scheduler_stats()` → aggregate from all per-CPU schedulers
- `init_scheduler()` → remove SchedulerInner initialization

**Files**: `core/src/scheduler/scheduler.rs`

**Verification**:
```bash
make test  # 364/364
grep -n "SchedulerInner\|SCHEDULER\.\|with_scheduler" core/src/scheduler/scheduler.rs
# Should find 0 results
```

---

## Summary

| Phase | Delete | Risk |
|-------|--------|------|
| 1 | `current_task`, `idle_task` from SchedulerInner | Low |
| 2 | `ready_queues` from SchedulerInner | Low |
| 3 | Duplicate flags from SchedulerInner | Low |
| 4 | `idle_task_function`, `ap_idle_loop` | Low |
| 5 | BSP/AP branches in `schedule()` | Medium |
| 6 | `ap_task_exit_to_idle` | Medium |
| 7 | `scheduler_timer_tick_ap` | Low |
| 8 | BSP stack switch conditional | Medium |
| 9 | `SchedulerInner` entirely | Low |

All 364 tests must pass after each phase.

---

## Files to Modify

```
core/src/scheduler/scheduler.rs   # All phases
core/src/scheduler/per_cpu.rs     # Phases 4, 8
```

---

## Verification Commands

```bash
# After each phase
make test                    # All 364 tests must pass

# Check for remaining special cases (should decrease each phase)
grep -c "cpu_id == 0\|cpu_id != 0" core/src/scheduler/scheduler.rs

# Check SchedulerInner usage (should reach 0 after Phase 9)
grep -c "SchedulerInner\|with_scheduler" core/src/scheduler/scheduler.rs
```
