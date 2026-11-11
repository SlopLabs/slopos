---
name: slopos-memory-sched
description: >
  Kernel memory accounting, allocation and task lifetimes in SlopOS: the commit
  ledger, the OOM killer, the kernel heap, cpufreq/HWP with hybrid placement,
  and the task-ownership invariants I1 to I8. Use when a task touches `mm/`,
  `sched/`, `cpufreq-core/`, `slopos-ostd/src/mm/`, `slopos-ostd/src/task/`,
  `slopos-ostd/src/process/quota/` or `/bin/cpufreq`, or mentions commit,
  quota, OOM, `KBox`, `KArc`, `TaskRef`, kill, wait queues or HWP.
---

## Read first

- `docs/architecture/memory`: how are pages, the kernel heap and stacks managed?
- `docs/architecture/resource-accounting`: how is memory charged, and whom does OOM kill?
- `docs/architecture/cpu-frequency`: how are HWP and core placement programmed?
- `docs/architecture/task-ownership`: who owns a task, and when is it freed?
- `docs/architecture/scheduler-wait-protocol`: how do tasks wait, wake and get killed?

## Rules a change must keep

- Allocate in the kernel only through `slopos_ostd::mm::heap`, never
  `alloc::*`; every constructor is fallible.
- Build large structs in place (`KBox::try_init` with an `init_…()` recipe);
  do not raise a `stack-sizes` allowlist to fit a value built on the stack.
- A region's commit class never moves back; only `Unreserved` changes, through
  `commit_under`. `fork` charges nothing and is never refused on the ledger.
- The OOM killer weighs a row's own share (`quota::held_by`), never present
  leaves, or a process could pad another's count with shared pages.
- The killer takes only processes whose every capability the writer holds
  (init's write excepted), never init, and consults no uid.
- Hand out a free stack slot that is still backed before a fresh one.
- Each CPU touches only its own MSRs, each read gated on CPUID; a settings
  change is a generation the CPUs converge on, never a cross-CPU call.
- Hold a task only as `TaskRef`; containers move references only through
  `slopos_ostd::task::placement`.
- The final task drop never runs on the task's own stack, with interrupts off
  or under a lock; never decide finality from a `strong_count` read.
- Kill is a flag: a task exits only from its own context, so answer
  `Err(WaitAbort::Killed)` by returning.
- Keep `task_is_dispatch_pinned` a disjunction and PCR offset 40 raw; the
  proof keeps verifying either way, so read `verification/STATUS.md` first.

## Checks

- `just test 'slopos_mm::*'`: paging, commit classes, the OOM killer
- `just test 'slopos_sched::*'`: waits, kills, placement, cpufreq, stack slots
- `just test-host`: `cpufreq-core` and the gates' own fixtures
- `cargo run -p slopos-xtask -- lint task-ownership alloc-dep charge-linearity`: raw task pointers, `alloc::*`, charge tokens
- `just check-framekernel-gates`: every source gate and the `stack-sizes` ELF gate
- `just verify task_ownership`: the Verus model of task ownership

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
