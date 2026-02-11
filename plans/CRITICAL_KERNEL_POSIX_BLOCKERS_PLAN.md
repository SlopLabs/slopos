# SlopOS Critical Kernel + POSIX Blockers Plan

> Generated: 2026-02-11
> Purpose: Detailed execution plan for critical reliability/safety work and minimum POSIX/Linux ABI compatibility required for libc/glibc bring-up

---

## Executive Summary

This plan is both the control board and the implementation playbook.

This roadmap is ordered by risk:

1. Stop kernel-wide panic vectors and SMP races.
2. Stabilize syscall contracts and core FD semantics.
3. Add minimum VM/process/thread/signal primitives expected by modern libc stacks.
4. Harden with stress and compatibility gates before claiming milestones.

If followed in order, this plan reduces the probability of catastrophic regressions while moving SlopOS toward practical POSIX/Linux userspace compatibility.

## Current Status Snapshot (2026-02-11)

Latest verified run:

- `make test` -> `TESTS SUMMARY: total=370 passed=370 failed=0`

Phase-level status:

| Phase | Status | Notes |
|------|--------|-------|
| Phase 0 | done | Baseline/guardrail pass completed earlier; reproducible test baseline captured in workflow |
| Phase 1 | done | Shared memory panic path, allocator unwrap hardening, and user-copy overflow fixes are in-tree |
| Phase 2 | done | Page-dir global swap removal, panic-recovery lock discipline, driver sync, and syscall provider lifecycle hardening completed |
| Phase 3 | done | ABI matrix/ENOSYS behavior, identity syscalls, and FD bootstrap contract implemented |
| Phase 4 | done | `mmap`/`munmap`/`mprotect` + core FD ops (`dup*`, `fcntl`, `lseek`, `fstat`) implemented |
| Phase 5 | done | Clone/thread-group, futex baseline, and signal baseline (including `SIGCHLD`/wait interaction) are implemented and test-covered |
| Phase 6 | done | Exec auxv contract + TLS setup/preservation and validation tests are implemented |
| Phase 7 | done | Poll/select baseline, pipe/pipe2 plumbing, ioctl baseline, and process-group/session syscalls are in-tree with regression tests |
| Phase 8 | done | Unsafe invariant registry, stress extensions, and compatibility smoke coverage are in-tree and `make test` is green |

Work package snapshot for active tail (5.x/6.x):

| Work package | Status | State details |
|--------------|--------|---------------|
| 5.1 `clone` + thread groups | done | Clone thread-group semantics + lifecycle interaction validated with join-like and mixed fork/clone tests |
| 5.2 futex baseline | done | Wait/wake baseline wired with mismatch/lost-wakeup and contention-path regression coverage |
| 5.3 signals baseline | done | `rt_sigaction`, `rt_sigprocmask`, `kill`, `rt_sigreturn`, handler delivery/return, and `SIGCHLD`+wait coupling are implemented |
| 6.1 exec/auxv contract | done | Stack contract and required auxv entry validation harnesses are in-tree |
| 6.2 TLS setup | done | `arch_prctl` + FS base preservation and clone TLS isolation tests are in-tree |

---

## Table of Contents

0. [Current Status Snapshot (2026-02-11)](#current-status-snapshot-2026-02-11)
1. [Scope, Assumptions, and Non-Goals](#1-scope-assumptions-and-non-goals)
2. [Severity and Priority Model](#2-severity-and-priority-model)
3. [Dependency Graph](#3-dependency-graph)
4. [Phase 0 - Baseline and Guardrails](#4-phase-0---baseline-and-guardrails)
5. [Phase 1 - Kernel Safety Hotfixes](#5-phase-1---kernel-safety-hotfixes)
6. [Phase 2 - Concurrency and Recovery Correctness](#6-phase-2---concurrency-and-recovery-correctness)
7. [Phase 3 - Syscall ABI Baseline for libc](#7-phase-3---syscall-abi-baseline-for-libc)
8. [Phase 4 - VM and FD Semantics](#8-phase-4---vm-and-fd-semantics)
9. [Phase 5 - Threads and Signals Foundations](#9-phase-5---threads-and-signals-foundations)
10. [Phase 6 - Exec Runtime Contracts (TLS/Auxv/Interp)](#10-phase-6---exec-runtime-contracts-tlsauxvinterp)
11. [Phase 7 - Interactive POSIX Usability](#11-phase-7---interactive-posix-usability)
12. [Phase 8 - Hardening and Verification Gates](#12-phase-8---hardening-and-verification-gates)
13. [Work Tracking Template](#13-work-tracking-template)
14. [Definition of Done Levels](#14-definition-of-done-levels)

---

## 1. Scope, Assumptions, and Non-Goals

### 1.1 Scope

In scope:

- Critical correctness bugs with panic/corruption potential.
- SMP and synchronization hazards.
- Syscall ABI and semantics needed for progressive libc compatibility.
- Core process/thread/signal/VM contracts.

Out of scope for this plan:

- Network stack completeness.
- Full POSIX feature closure in one pass.
- Performance-only work that does not influence correctness.
- Compositor/UI roadmap work.

### 1.2 Assumptions

- Build/test entry points remain `make build` and `make test`.
- Syscall ABI numbering lives in `abi/src/syscall.rs`.
- Task/process model source of truth is in `core/src/scheduler` + `mm/src/process_vm.rs`.
- Existing lore style remains untouched; this document is engineering-first execution guidance.

### 1.3 Non-Goals

- Do not redesign entire architecture before fixing known critical faults.
- Do not introduce broad refactors inside bugfix tasks.
- Do not chase full glibc compatibility before baseline kernel safety is complete.

---

## 2. Severity and Priority Model

| Level | Meaning | Release Policy |
|-------|---------|----------------|
| P0 | Kernel panic/data corruption/race with high blast radius | Must fix before new ABI surface expansion |
| P1 | ABI contracts that block libc/pthreads bring-up | Must fix before compatibility claims |
| P2 | Missing POSIX usability semantics | Can parallelize after P0/P1 stabilization |
| P3 | Hardening/documentation/developer ergonomics | Continuous, can batch |

Decision rule:

- If a task can crash or corrupt kernel state under normal usage, it is P0 regardless of feature value.

---

## 3. Dependency Graph

High-level dependency chain:

1. **Phase 0** -> required by all later phases.
2. **Phase 1/2 (P0)** -> must complete before major ABI expansion.
3. **Phase 3/4 (P1 ABI + VM/FD semantics)** -> must complete before thread/signal runtime work.
4. **Phase 5/6 (threads/signals/runtime contract)** -> must complete before interactive POSIX layer.
5. **Phase 7** depends on Phase 3-6.
6. **Phase 8** runs continuously but has mandatory final gate criteria.

---

## 4. Phase 0 - Baseline and Guardrails

Objective: Ensure every subsequent change is measured and reversible.

Status: [x] Completed.

### 4.1 Baseline snapshot

- [ ] Capture current `make test` result and keep artifact log in `test_output.log`.
- [ ] Capture syscall inventory from `abi/src/syscall.rs` (implemented/partial/missing).
- [ ] Record known critical hotspots from this plan into issue tracker/checklist board.

### 4.2 Safety guardrails

- [ ] Define coding guardrail: no new `unwrap()` in kernel-critical paths.
- [ ] Define guardrail: all new unsafe blocks require explicit invariant comment.
- [ ] Define guardrail: no semantic ABI changes without update to syscall compatibility notes.

### 4.3 Exit criteria

- [ ] Baseline tests are reproducible.
- [ ] Change discipline and review checklist agreed and documented.

Rollback:

- N/A (documentation/setup phase).

---

## 5. Phase 1 - Kernel Safety Hotfixes

Objective: Remove immediate panic and corruption vectors.

Status: [x] Completed.

### Work Package 1.1 - Shared memory mapping panic removal (P0)

Primary file:

- `mm/src/shared_memory.rs:398`

Tasks:

- [ ] Replace `position(...).unwrap()` with graceful failure path.
- [ ] Return deterministic error when mapping slots are exhausted.
- [ ] Ensure partial mapping rollback remains correct when slot allocation fails.

Tests:

- [ ] Add test: map same shared token until slot exhaustion; kernel must not panic.
- [ ] Add test: verify expected error code/result for exhausted mappings.

Done criteria:

- [ ] No panic under slot exhaustion.
- [ ] Behavior documented in shared memory syscall contract.

Rollback strategy:

- Revert only mapping-slot change if regressions appear in SHM map/unmap tests.

### Work Package 1.2 - Page allocator unwrap hardening (P0)

Primary files:

- `mm/src/page_alloc.rs`

Tasks:

- [ ] Find and replace chained `unwrap()` on fallible frame descriptor paths.
- [ ] Propagate allocation/index validation errors through safe return channels.
- [ ] Add debug assertions only where invariants are strictly local and provable.

Tests:

- [ ] Add negative tests for invalid frame index handling.
- [ ] Add stress tests for allocator failure paths.

Done criteria:

- [ ] No kernel panic due to allocator `unwrap()` in invalid/failure conditions.

Rollback strategy:

- Keep allocator behavior changes behind minimal isolated commits for selective revert.

### Work Package 1.3 - User-copy overflow and range validation (P0)

Primary files:

- `mm/src/user_copy.rs`

Tasks:

- [ ] Replace wrapping arithmetic range checks with overflow-checked variants.
- [ ] Reject wrapped or kernel-space crossing ranges before traversal.
- [ ] Keep error mapping deterministic for userland.

Tests:

- [ ] Add tests for near-`u64::MAX` addresses and oversized lengths.
- [ ] Add tests for boundary crossings at user/kernel split.

Done criteria:

- [ ] Range validation cannot be bypassed via wraparound input.

Rollback strategy:

- Revert only validation logic if false positives break legitimate copies; retain test cases.

---

## 6. Phase 2 - Concurrency and Recovery Correctness

Objective: Remove SMP race patterns and make panic recovery explicit and bounded.

Status: [x] Completed.

### Work Package 2.1 - Remove mutable global page-dir switching (P0)

Primary files:

- `mm/src/paging/tables.rs:44`
- `mm/src/paging/tables.rs:218`

Tasks:

- [ ] Eliminate `static mut CURRENT_PAGE_DIR` mutation pattern for process translations.
- [ ] Route all address translation through explicit page-dir argument APIs.
- [ ] Ensure API design avoids hidden global state coupling.

Tests:

- [ ] SMP stress: concurrent translations across multiple process page dirs.
- [ ] Regression test: ensure existing translation users still resolve correct physical addresses.

Done criteria:

- [ ] No mutable global current page-dir swap path remains.

Rollback strategy:

- Keep old and new translation path behind temporary adapter until tests pass; then remove adapter.

### Work Package 2.2 - Panic recovery lock discipline (P0)

Primary files:

- `core/src/scheduler/scheduler.rs:851`
- `core/src/scheduler/task.rs:1073`
- `lib/src/spinlock.rs:36`

Tasks:

- [ ] Replace blind `force_unlock` calls with subsystem-specific recovery protocol.
- [ ] Introduce explicit recovery states (e.g., "poisoned", "reinit required") for critical managers.
- [ ] Ensure recovered subsystems reinitialize invariants before accepting normal operations.

Tests:

- [ ] Panic-in-critical-section simulation with post-recovery scheduler/task operations.
- [ ] Validate no use of structures before recovery complete.

Done criteria:

- [ ] Recovery is explicit, validated, and test-covered.
- [ ] No hidden lock-state reset without invariant checks.

Rollback strategy:

- If recovery protocol destabilizes boot, temporarily gate panic recovery to safe halt path while preserving invariant checks.

### Work Package 2.3 - Driver global state synchronization (P0)

Primary files:

- `drivers/src/virtio_blk.rs:59`

Tasks:

- [ ] Replace mutable global device state with synchronized container.
- [ ] Define IRQ-safe request submission protocol.
- [ ] Ensure ownership/claim state and request path share one coherent synchronization model.

Tests:

- [ ] Concurrent read/write workload with forced request timeouts.
- [ ] Repeated probe/init failure and re-entry scenarios.

Done criteria:

- [ ] No unsynchronized mutable global device state remains in hot request path.

Rollback strategy:

- Maintain fallback single-request mode behind compile-time/test flag during migration.

### Work Package 2.4 - Syscall task-provider lifecycle hardening (P0)

Primary files:

- `core/src/syscall/dispatch.rs`
- `mm/src/user_copy.rs`

Tasks:

- [ ] Ensure process identity provider setup/restore cannot leak across nested/failing flows.
- [ ] Tie provider lifecycle to robust guard pattern to avoid stale callback state.
- [ ] Define behavior for null/current-task missing scenarios.

Tests:

- [ ] Nested syscall path simulation.
- [ ] Faulted syscall path ensuring provider restore always executes.

Done criteria:

- [ ] No stale task-provider state across syscall boundaries.

Rollback strategy:

- Revert provider-guard wiring independently if it impacts unrelated syscall behavior.

---

## 7. Phase 3 - Syscall ABI Baseline for libc

Objective: Make syscall surface predictable and explicitly versioned enough for libc iteration.

Status: [x] Completed.

### Work Package 3.1 - ABI matrix and ENOSYS contract (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`

Tasks:

- [ ] Build syscall matrix: implemented, partial, missing, intentionally unsupported.
- [ ] Enforce uniform unsupported behavior (`ENOSYS`) across dispatch paths.
- [ ] Add ABI tests for syscall number stability.

Tests:

- [ ] Table-driven test that probes every syscall number in registry.
- [ ] Verify unknown syscall returns expected standardized error.

Done criteria:

- [ ] ABI matrix exists and matches runtime behavior.

Rollback strategy:

- Keep compatibility shim for legacy unknown-syscall behavior for one transition cycle only.

### Work Package 3.2 - Process identity syscalls (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`
- `core/src/scheduler/task.rs` (as needed)

Tasks:

- [ ] Add `getpid`, `getppid`.
- [ ] Add minimal uid/gid identity syscalls with documented policy.
- [ ] Ensure fork/exec maintains coherent identity semantics.

Tests:

- [ ] Parent/child identity tests.
- [ ] Identity stability tests across exec.

Done criteria:

- [ ] Identity calls are present and semantically stable.

Rollback strategy:

- Revert newly added identity syscalls if semantics are wrong; keep syscall numbers reserved if released.

### Work Package 3.3 - FD 0/1/2 bootstrap contract (P1)

Primary files:

- `fs/src/fileio.rs`
- `core/src/exec/mod.rs`
- process creation path in `core/src/scheduler/task.rs`

Tasks:

- [ ] Ensure every user process has deterministic stdin/stdout/stderr setup.
- [ ] Define fallback behavior if console/tty backend unavailable.
- [ ] Prevent silent empty-FD process creation.

Tests:

- [ ] Smoke test for stdout/stderr writes in fresh process.
- [ ] Verify stdin read behavior under no-input conditions.

Done criteria:

- [ ] New processes always satisfy FD bootstrap contract.

Rollback strategy:

- Keep fallback redirection to safe sink/source device if full terminal plumbing not yet ready.

---

## 8. Phase 4 - VM and FD Semantics

Objective: Deliver foundational semantics libc and UNIX-style userspace rely on.

Status: [x] Completed.

### Work Package 4.1 - `mmap`/`munmap`/`mprotect` baseline (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`
- `mm/src/process_vm.rs`
- VMA logic (`mm/src/vma_tree.rs`, related mapping helpers)

Tasks:

- [ ] Define minimal supported flags/protections and explicit unsupported flag behavior.
- [ ] Implement map/unmap/protection changes with strict alignment and range checks.
- [ ] Integrate with demand paging and COW behavior without violating VM invariants.

Tests:

- [ ] Anonymous map/unmap tests.
- [ ] Protection transition tests (RW -> RO, RO -> RX if supported).
- [ ] Overlap and invalid flag rejection tests.

Done criteria:

- [ ] VM syscalls operational for baseline libc allocator/runtime needs.

Rollback strategy:

- Keep `mmap` feature subset minimal first; reject complex flags instead of partial buggy support.

### Work Package 4.2 - Core FD operations (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/fs.rs`
- `core/src/syscall/handlers.rs`
- `fs/src/fileio.rs`

Tasks:

- [ ] Implement `dup`, `dup2`/`dup3`.
- [ ] Implement minimal `fcntl` operations (`FD_CLOEXEC` and required flag controls).
- [ ] Implement `lseek` and `fstat` with coherent per-FD offset/stat behavior.

Tests:

- [ ] Duplication and close-on-exec behavior tests.
- [ ] Seek + read/write offset consistency tests.
- [ ] Fstat correctness tests for regular files and device nodes.

Done criteria:

- [ ] UNIX-style descriptor manipulation works for common userland patterns.

Rollback strategy:

- If one syscall path regresses, isolate and disable that syscall while preserving stable behavior of others.

---

## 9. Phase 5 - Threads and Signals Foundations

Objective: Add minimum kernel primitives for libc threading and process control.

Status: [x] Completed.

### Work Package 5.1 - `clone` and thread-group scaffolding (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`
- scheduler/task model files in `core/src/scheduler`

Tasks:

- [x] Introduce minimal `clone` variant sufficient for early libc/pthread pathways.
- [x] Define thread-group identifiers and parent/child semantics.
- [x] Integrate lifecycle with existing task termination and wait mechanics.

Tests:

- [x] Basic thread create/exit/join-like behavior tests.
- [x] Mixed fork + clone interaction tests.

Done criteria:

- [x] Thread creation semantics are explicit and repeatable.

Rollback strategy:

- Keep feature-gated clone flags; reject unsupported combinations with clear errors.

### Work Package 5.2 - Futex baseline (P1)

Primary files:

- syscall layer + synchronization/waitqueue infrastructure

Tasks:

- [x] Implement `FUTEX_WAIT` and `FUTEX_WAKE` core behavior.
- [x] Handle wake races and value mismatch semantics correctly.
- [x] Ensure timeout and interruption behavior is documented.

Tests:

- [x] Contended lock simulation on multiple CPUs.
- [x] Lost-wakeup regression tests.

Done criteria:

- [x] Futex baseline supports basic userspace mutex/condvar operation.

Rollback strategy:

- Ship wait/wake subset first; return explicit unsupported error for advanced ops.

### Work Package 5.3 - Real signal baseline (P1)

Primary files:

- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`
- task/process state and interrupt return paths

Tasks:

- [x] Implement `rt_sigaction`, `rt_sigprocmask`, `kill`.
- [x] Implement `rt_sigreturn` return-path correctness.
- [x] Define and implement child-exit signaling interaction with wait.

Tests:

- [x] Handler install/deliver/return tests.
- [x] Masking/unmasking behavior tests.
- [x] SIGCHLD + wait interaction tests.

Done criteria:

- [x] Signal core is functional for fundamental process control use cases.

Rollback strategy:

- If full signal frame support is unstable, temporarily restrict to subset while keeping syscall contracts clear.

---

## 10. Phase 6 - Exec Runtime Contracts (TLS/Auxv/Interp)

Objective: Make exec/runtime handoff compatible with dynamic userspace expectations.

Status: [x] Completed.

### Work Package 6.1 - Exec interpreter and auxv contract (P1/P2 bridge)

Primary files:

- `core/src/exec/mod.rs`
- ELF loader paths in `mm/src/elf.rs` and process VM loader code

Tasks:

- [x] Define support policy for interpreter-style execution contract.
- [x] Populate minimal auxiliary vector expected by libc runtime.
- [x] Validate stack layout contract (`argc`, `argv`, `envp`, `auxv`) end-to-end.

Tests:

- [x] Stack layout validation harness.
- [x] Runtime smoke test verifying auxv entries needed by startup code.

Done criteria:

- [x] Exec handoff is deterministic and documented.

Rollback strategy:

- Keep strict validation and fail-fast behavior for unsupported binary/runtime patterns.

### Work Package 6.2 - TLS setup syscall path (P1)

Primary files:

- `abi/src/syscall.rs`
- syscall handlers + per-task context structures

Tasks:

- [x] Implement x86_64 TLS base setup syscall behavior.
- [x] Ensure scheduler/context switch preserves TLS register/base state.
- [x] Validate per-thread isolation for TLS data.

Tests:

- [x] Multi-thread TLS isolation tests.
- [x] Context switch TLS preservation tests.

Done criteria:

- [x] TLS setup and preservation reliable in threaded workloads.

Rollback strategy:

- Disable multithreaded runtime smoke tests if TLS is gated; keep single-thread path valid.

---

## 11. Phase 7 - Interactive POSIX Usability

Objective: Enable shell-like interactive behavior and process orchestration basics.

Status: [x] Completed.

### Work Package 7.1 - I/O multiplexing baseline (P2)

Tasks:

- [x] Implement `poll` and/or `select` with documented subset.
- [x] Integrate readiness semantics with FD/device model.

Tests:

- [x] Readiness change tests for file/input/pipe descriptors.

### Work Package 7.2 - Pipe primitives (P2)

Tasks:

- [x] Implement `pipe`/`pipe2`.
- [x] Ensure interaction with `dup2`, `fork`, `exec`, and close semantics.

Tests:

- [x] Pipeline tests (`producer | consumer`) and EOF behavior checks.

### Work Package 7.3 - TTY and process groups (P2)

Tasks:

- [x] Implement minimal terminal `ioctl` behavior needed by shell use.
- [x] Add `setpgid`, `getpgid`, `setsid` baseline semantics.
- [x] Validate foreground/background process behavior coherence.

Tests:

- [x] Basic job-control smoke tests.

Done criteria (Phase 7):

- [x] Interactive shell workflows no longer blocked by missing core control primitives.

---

## 12. Phase 8 - Hardening and Verification Gates

Objective: Prove the system is robust, not just feature-present.

Status: [x] Completed.

### 12.1 Unsafe invariant registry (P3)

- [x] Build per-file unsafe inventory for kernel-critical crates.
- [x] Attach invariant notes to each unsafe boundary.
- [x] Mark unresolved unsafe boundaries with explicit risk tags.

### 12.2 Concurrency stress suite (P3)

- [x] SMP scheduler wakeup race stress tests.
- [x] VM map/unmap/protect concurrent stress tests.
- [x] Storage driver request contention stress tests.

### 12.3 Compatibility smoke suite (P3)

- [x] Syscall ABI smoke tests (identity, FD, VM, process control).
- [x] Fork/exec/wait and signal interaction smoke tests.
- [x] Thread/futex/TLS baseline smoke tests.

### 12.4 Gate policy

- [x] No milestone promoted without passing relevant smoke + stress gates.
- [x] Pre-existing failures explicitly tracked and separated from new regressions.

---

## 13. Work Tracking Template

Use this block per task or issue:

```markdown
### Task ID: <phase.wp.task>

- Status: [ ] not started [-] in progress [x] done
- Priority: P0/P1/P2/P3
- Owner:
- Dependencies:

#### Scope
- Files:
- Public contract affected:

#### Implementation Checklist
- [ ]
- [ ]

#### Verification Checklist
- [ ] Unit/integration tests added or updated
- [ ] `make test` pass (or pre-existing failures documented)
- [ ] Negative/failure-path test coverage

#### Notes
- Risks:
- Rollback:
```

---

## 14. Definition of Done Levels

### DoD-Task

Task is done when:

- [ ] Code path implemented per scope.
- [ ] Failure paths are explicit and non-panicking for recoverable errors.
- [ ] Relevant tests added/updated and passing.

### DoD-Phase

Phase is done when:

- [ ] All phase work packages marked complete.
- [ ] No open P0 regressions introduced by phase.
- [ ] Compatibility notes updated (if ABI/semantics changed).

### DoD-Milestone (release-quality gate)

Milestone is done when:

- [ ] All required phase gates pass in CI-like flow (`make test`).
- [ ] Stress tests pass for changed concurrency areas.
- [ ] Outstanding known issues are explicitly documented with severity.

---

## Immediate Next Sprint Recommendation

Sprint focus should move to Phase 7 + targeted hardening:

1. Phase 7.1: implement `poll`/`select` baseline with explicit readiness subset.
2. Phase 7.2: implement `pipe`/`pipe2` with `dup2`/`fork`/`exec`/close semantics coverage.
3. Phase 7.3: add minimal terminal `ioctl` + process-group/session baseline (`setpgid`/`getpgid`/`setsid`).
4. Phase 8 hardening: add dedicated stress suites for scheduler wake races, VM map/protect races, and storage contention.

Reason:

- Phases 0-6 are now closed; the main compatibility gap is interactive POSIX process control and readiness semantics, with stress hardening needed before milestone promotion.
