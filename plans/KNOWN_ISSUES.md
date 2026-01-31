# SlopOS Known Issues

Last updated: 2026-01-31

---

## Performance: Compositor Frame Rate During Task Termination

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

When a task terminates, `pause_all_aps()` is called which blocks all AP scheduler loops. While this is necessary for safe task cleanup, it can cause brief stalls in compositor frame rendering if the compositor happens to be scheduled on an AP.

### Current Behavior

1. Task calls `task_terminate()`
2. `pause_all_aps()` sets `AP_PAUSED = true` and waits for APs to stop executing
3. `release_task_dependents()` unblocks waiting tasks
4. `resume_all_aps()` sets `AP_PAUSED = false` and sends wake IPIs

During steps 2-3, any task on an AP (including compositor) is paused.

### Impact

- Brief frame drops (1-2 frames) during task termination
- More noticeable with frequent task spawning/termination

### Potential Optimizations

1. **Fine-grained locking**: Instead of pausing all APs, use per-task locks
2. **RCU-style cleanup**: Defer task cleanup to a dedicated kernel thread
3. **Lock-free dependent release**: Use atomic operations instead of global pause

### Related Files

- `core/src/scheduler/task.rs` - `task_terminate()`
- `core/src/scheduler/per_cpu.rs` - `pause_all_aps()`, `resume_all_aps()`

---

## Performance: Scheduler Lock Contention

**Status**: Open - Minor  
**Severity**: Low  
**Component**: `core/src/scheduler`

### Description

The scheduler uses a global `SCHEDULER` mutex that can cause contention when multiple CPUs try to schedule tasks simultaneously.

### Current Architecture

```
SCHEDULER (global IrqMutex)
├── ready_queues[4]     // Priority-based queues
├── current_task
├── idle_task
└── various counters

CPU_SCHEDULERS[MAX_CPUS] (per-CPU)
├── ready_queues[4]     // Local priority queues
├── current_task_atomic
└── queue_lock (per-CPU mutex)
```

### Contention Points

1. `schedule()` calls `with_scheduler()` which locks global mutex
2. `schedule_task()` may fall back to global queue if per-CPU enqueue fails
3. `select_next_task()` checks both per-CPU and global queues

### Impact

- Minor latency spikes under high task churn
- Not significant with current workloads (compositor + shell)
- Would become more noticeable with many concurrent tasks

### Potential Optimizations

1. **Fully per-CPU scheduling**: Eliminate global ready queue entirely
2. **Lock-free queues**: Use compare-and-swap for enqueue/dequeue
3. **Batch operations**: Coalesce multiple schedule operations

### Related Files

- `core/src/scheduler/scheduler.rs` - `SCHEDULER`, `with_scheduler()`
- `core/src/scheduler/per_cpu.rs` - `CPU_SCHEDULERS`

---

## Notes for Future Development

### SMP Architecture

The kernel uses a unified Processor Control Region (PCR) per CPU, following Redox OS patterns:

- Each CPU has its own `ProcessorControlRegion` containing embedded GDT, TSS, and kernel stack
- `GS_BASE` always points to the current CPU's PCR in kernel mode
- Fast per-CPU access via `gs:[offset]` (~1-3 cycles vs ~100 cycles for LAPIC MMIO)
- `get_current_cpu()` uses `gs:[24]` for instant CPU ID lookup

See `lib/src/pcr.rs` for architecture details.
