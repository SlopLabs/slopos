# Memory Management Allocator Fixes

**Status**: PENDING  
**Priority**: HIGH (Blocking scheduler symmetry and potentially other features)  
**Discovered**: During Phase 6 of Scheduler Full Symmetry plan

---

## Overview

During attempts to unify scheduler task exit paths (Phase 6 of SCHEDULER_FULL_SYMMETRY.md), random freezes and animation issues were observed. Deep analysis revealed these are **NOT scheduler bugs** but fundamental memory management issues that manifest as non-deterministic failures.

---

## Root Cause Analysis

### Issue 1: Rust Global Allocator Never Frees Memory

**Location**: `mm/src/lib.rs:38`

The Rust global allocator (`#[global_allocator]`) is implemented as a **2 MiB bump allocator** that:
- Allocates memory by incrementing a pointer
- **Never reclaims or frees memory**
- When exhausted, triggers `#[alloc_error_handler]` which **hard-halts the CPU**

**Impact**: Any code using Rust's `alloc` crate (Vec, BTreeMap, String, Box, etc.) permanently consumes memory until OOM halt.

**Affected subsystems**:
| Subsystem | Allocation Source |
|-----------|-------------------|
| Compositor | `BTreeMap`, `VecDeque` in `video/src/compositor_context.rs:17` |
| Exec/ELF loader | `Vec` for ELF data in `core/src/exec/mod.rs:65` |
| Any `alloc` usage | Permanent leak |

---

### Issue 2: Task Self-Termination Leaks Resources

**Location**: `core/src/scheduler/task.rs:615`, `core/src/scheduler/task.rs:654`

When a task terminates itself (via `exit()` syscall), `task_terminate()` **skips cleanup**:
- Only frees stacks and process VM for **non-current tasks**
- Current task cleanup is deferred but never executed
- Task stacks and VM spaces accumulate

**Call path**:
```
syscall_exit (core/src/syscall/handlers.rs:92)
  -> task_terminate(u32::MAX)  // terminates current task
  -> scheduler_task_exit_impl()  // schedules away immediately
  // Cleanup for current task never runs!
```

---

### Issue 3: OOM Handler Halts Without Panic

**Location**: `kernel/src/main.rs:64`

```rust
#[alloc_error_handler]
fn alloc_error_handler(layout: Layout) -> ! {
    // Prints "Allocation failure:" then halts
}
```

When allocation fails:
- No kernel panic with backtrace
- CPU simply halts
- On SMP: if AP halts, cross-CPU operations hang (work stealing, IPIs)
- Appears as "random freeze" at different locations

---

## Observed Symptoms

| Symptom | Explanation |
|---------|-------------|
| Random freezes at different test locations | OOM halts wherever next `alloc()` call happens |
| Roulette animation terminates mid-way | Upstream allocation failure halts system |
| Freezes during rapid task create/destroy | Task self-termination leaks compound |
| "kfree: Invalid block or double free" messages | Mix of defensive checks and intentional test triggers |

---

## Verification

Check serial logs for these messages to confirm OOM:

1. **Global allocator OOM**: `Allocation failure:` (definitive)
2. **Page frame exhaustion**: `expand_heap: Failed to allocate physical page`
3. **Kernel heap exhaustion**: `kmalloc: No suitable block`

---

## Proposed Fixes

### Phase 1: Replace Bump Allocator with Freeing Allocator

**Effort**: Medium (1-2 days)

Replace the global allocator with one backed by `kmalloc`/`kfree`:

```rust
// Current (mm/src/lib.rs)
unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Bump pointer, never free
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // NO-OP!
    }
}

// Proposed
unsafe impl GlobalAlloc for KmallocAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        kmalloc(layout.size())
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        kfree(ptr)  // Actually free!
    }
}
```

---

### Phase 2: Fix Task Self-Termination Leak

**Effort**: Medium (1 day)

Options:
1. **Post-switch reaper**: After context switch to idle, clean up the just-exited task
2. **Deferred cleanup queue**: Queue terminated tasks for cleanup by idle loop
3. **Immediate cleanup before exit**: Restructure to clean up before scheduling away

**Recommended**: Post-switch reaper in `scheduler_loop()`:
```rust
// After execute_task returns (task yielded or exited)
if task_is_terminated(next_task) {
    // Clean up task resources here
    free_task_stack(next_task);
    destroy_process_vm(next_task.process_id);
}
```

---

### Phase 3: Improve OOM Handling

**Effort**: Short (hours)

- Add OOM panic with backtrace instead of silent halt
- Consider fallible allocation APIs for non-critical paths
- Add memory pressure monitoring/logging

---

## Memory Layout Reference

| Region | Size | Location |
|--------|------|----------|
| Rust global allocator heap | 2 MiB | `mm/src/lib.rs:38` |
| Kernel heap VA window | 256 MiB | `abi/src/arch/x86_64/memory.rs:57` |
| Task kernel stacks | 32 KiB each | Allocated per task |

---

## Dependencies

- **SCHEDULER_FULL_SYMMETRY.md Phase 6+**: Blocked until MM issues resolved
- Any feature using heavy `alloc` usage risks OOM

---

## Files to Modify

```
mm/src/lib.rs                    # Global allocator
mm/src/kernel_heap.rs            # kmalloc/kfree integration
core/src/scheduler/task.rs       # Task cleanup on self-termination
core/src/scheduler/scheduler.rs  # Post-switch reaper
kernel/src/main.rs               # OOM handler improvement
```

---

## Success Criteria

1. `make test` passes reliably (no random freezes)
2. Long-running workloads don't accumulate leaked memory
3. Scheduler symmetry Phase 6 can be completed without issues
4. Serial logs show no `Allocation failure:` messages during normal operation
