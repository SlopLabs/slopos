# SlopOS Development Plans

This directory contains architectural analysis and improvement roadmaps for SlopOS.

## Documents

| Document | Description |
|----------|-------------|
| [UNIFIED_PERCPU_SCHEDULER.md](./UNIFIED_PERCPU_SCHEDULER.md) | **Priority** - Symmetric per-CPU scheduler following Theseus/Redox patterns |
| [ANALYSIS_SLOPOS_VS_LINUX_REDOX.md](./ANALYSIS_SLOPOS_VS_LINUX_REDOX.md) | Comprehensive comparison of SlopOS against Linux/GNU and Redox OS |
| [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) | Detailed implementation plan for the retained-mode widget toolkit |
| [KNOWN_ISSUES.md](./KNOWN_ISSUES.md) | Open performance issues and notes for future development |

---

## Completed: SMP Safety Infrastructure

The Rust-native SMP safety infrastructure is now complete:

| Component | Status |
|-----------|--------|
| `TaskStatus` enum | Implemented in `abi/src/task.rs` |
| `BlockReason` enum | Implemented in `abi/src/task.rs` |
| Type-safe state transitions | `mark_ready()`, `mark_running()`, `block()`, `terminate()` |
| `TaskHandle` safe wrapper | Implemented in `core/src/scheduler/task_lock.rs` |
| `IrqRwLock` primitives | Implemented in `lib/src/spinlock.rs` |
| `SwitchContext` with `offset_of!` | Compile-time safe struct offsets |

---

## Completed: Unified PCR Infrastructure

The Unified Processor Control Region (PCR) following Redox OS patterns is complete:

| Component | Status |
|-----------|--------|
| `ProcessorControlRegion` struct | Implemented in `lib/src/pcr.rs` |
| Per-CPU GDT/TSS embedding | Embedded in PCR |
| Fast GS-based CPU access | `gs:[24]` for instant CPU ID (~1-3 cycles) |
| AP user-mode execution | Fixed - tasks run on any CPU |
| SYSCALL/context switch assembly | Updated for PCR offsets |

---

## Completed: Kernel Foundation

The kernel foundation is complete. All critical systems are implemented:
- VFS Layer with ext2, ramfs, devfs
- exec() syscall with ELF loading from filesystem
- libc minimal C runtime (read/write/exit/malloc)
- CRT0, argv/envp passing, brk syscall
- Per-CPU page caches, VMA red-black tree
- ASLR, RwLock primitives
- Copy-on-Write, Demand Paging, fork() syscall
- SYSCALL/SYSRET fast path
- Priority-based scheduling
- TLB shootdown, FPU state save

---

## In Progress: Unified Per-CPU Scheduler

**See [UNIFIED_PERCPU_SCHEDULER.md](./UNIFIED_PERCPU_SCHEDULER.md) for complete design.**

The current scheduler has an asymmetric architecture where BSP (CPU 0) operates differently from APs. This plan unifies all CPUs under a symmetric per-CPU model following Theseus OS and Redox OS patterns.

| Phase | Description | Status | Risk |
|-------|-------------|:------:|:----:|
| Phase 0 | Add inbox drain to timer tick (immediate win) | Planned | Very Low |
| Phase 1 | State consolidation (eliminate SchedulerInner duplication) | Planned | Low |
| Phase 2 | Unified scheduler_loop() for all CPUs | Planned | Medium |
| Phase 3 | BSP enters scheduler_loop() instead of halting | Planned | Medium-High |
| Phase 4 | Lock-free cross-CPU scheduling | Planned | Low |
| Phase 5 | Cleanup and code deletion | Planned | Low |

**Key Benefits:**
- Lock-free cross-CPU task wakeups (<10μs vs ~1ms with current mutex)
- Symmetric code paths (no special-case BSP handling)
- Foundation for pluggable scheduler policies
- Matches production Rust OS architectures

---

## Next: UI Toolkit

**See [UI_TOOLKIT_DETAILED_PLAN.md](./UI_TOOLKIT_DETAILED_PLAN.md) for complete implementation details.**

| Task | Complexity | Status | Plan Reference |
|------|:----------:|:------:|----------------|
| Widget system API design | Low | | Phase 1 |
| Core widgets (Button, Label, Container) | Medium | | Phase 3 |
| Layout engine (Vertical, Horizontal, Grid) | Medium | | Phase 2 |
| Port shell to use toolkit | Medium | | Phase 5 |
| Theming system | Low | | Phase 4 |

### Implementation Phases

1. **Phase 1**: Foundation - `Widget` trait, `WidgetRegistry`, event types, renderer integration
2. **Phase 2**: Basic Widgets - Button, Label, Theme system
3. **Phase 3**: Layout - Column, Row, Container with flex-based layout
4. **Phase 4**: Advanced Widgets - TextInput, Scrollable
5. **Phase 5**: Shell Migration - Port existing 1500-line shell to use toolkit

---

## Contributing

When adding new plans or analysis documents:

1. Use descriptive filenames with `UPPERCASE_SNAKE_CASE.md`
2. Include a table of contents for documents over 200 lines
3. Reference specific file paths and line numbers where applicable
4. Update this README with new document entries
