# Phase 8.1 Unsafe Invariant Registry

Status: in-tree baseline registry
Date: 2026-02-11

This registry captures kernel-critical unsafe boundaries and their invariants.

Risk tags:
- `R0`: invariant locally enforced and test-covered
- `R1`: invariant enforced but cross-module assumptions exist
- `R2`: invariant partially enforced; further hardening planned

## lib/

| File | Boundary | Invariant | Risk |
|------|----------|-----------|------|
| `lib/src/spinlock.rs` | raw `UnsafeCell` deref in guards | lock state grants exclusive/shared access before deref | R0 |
| `lib/src/pcr.rs` | per-CPU pointer and callback transmute | current CPU is pinned and pointer targets initialized PCR slot | R1 |
| `lib/src/service_cell.rs` | static pointer store/load | only stores valid `'static` references; non-null pointer contract | R0 |
| `lib/src/preempt.rs` | callback fn pointer transmute | pointer set only via registration path with exact fn signature | R1 |

## core/

| File | Boundary | Invariant | Risk |
|------|----------|-----------|------|
| `core/src/scheduler/task.rs` | raw `Task` slot cloning and lookup | slot ownership is exclusive during mutation; lifecycle guarded by manager lock | R1 |
| `core/src/scheduler/scheduler.rs` | context switch pointers and runqueue links | tasks in queues are valid and state transitions are atomic-consistent | R1 |
| `core/src/syscall/dispatch.rs` | raw interrupt frame/task pointers | syscall entry validates non-null frame; task pointer comes from scheduler current task | R0 |
| `core/src/exec/mod.rs` | user entry pointer transmute | user entry VA must lie in user text range and mapped executable region | R1 |

## mm/

| File | Boundary | Invariant | Risk |
|------|----------|-----------|------|
| `mm/src/page_alloc.rs` | frame descriptor pointer arithmetic | frame index validated against allocator bounds before deref | R1 |
| `mm/src/process_vm.rs` | raw process/page-dir pointers | process id lookup returns active process with initialized page directory | R1 |
| `mm/src/user_copy.rs` | direct user pointer reads/writes | user range validated against process page tables before copy | R0 |
| `mm/src/paging/tables.rs` | static current directory and page table deref | caller provides valid page-dir ownership and mapping invariants | R2 |

## drivers/

| File | Boundary | Invariant | Risk |
|------|----------|-----------|------|
| `drivers/src/virtio_blk.rs` | MMIO/descriptor pointer access | virtqueue buffers are mapped, aligned, and owned while request in flight | R1 |
| `drivers/src/tty.rs` | raw scheduler task pointers in wait queues | queued task pointers remain valid until dequeue/unblock paths complete | R1 |
| `drivers/src/pci.rs` | static driver registry pointers | registry stores only `'static` driver references | R0 |

## boot/

| File | Boundary | Invariant | Risk |
|------|----------|-----------|------|
| `boot/src/idt.rs` | interrupt frame pointer and handler dispatch | CPU-supplied frame pointer is valid for active exception context | R1 |
| `boot/src/gdt.rs` | descriptor/table loads and TSS writes | GDT/TSS memory remains pinned for lifetime of CPU | R1 |
| `boot/src/early_init.rs` | linker section iteration pointer math | start/stop section symbols delimit valid contiguous step descriptors | R1 |

## Unresolved R2 boundaries

1. `mm/src/paging/tables.rs`: residual global current-page-directory compatibility path; eliminate hidden mutable global access.
2. Any future raw transmute of callback/function pointers requires explicit registration provenance checks and tests.

## Gate policy

- New unsafe blocks in kernel-critical crates must add an entry in this registry.
- New entry requires explicit invariant statement and risk tag.
- `R2` entries block phase promotion unless accompanied by mitigation plan.
