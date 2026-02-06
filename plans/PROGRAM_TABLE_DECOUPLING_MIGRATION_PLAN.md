# SlopOS PROGRAM_TABLE Decoupling Migration Plan

> **Status**: Completed  
> **Date**: 2026-02-06  
> **Scope**: Remove kernel-owned user program registry (`PROGRAM_TABLE`) and move launch policy fully into userspace init/service logic while preserving deterministic boot.

---

## 1. Purpose

This document is the source of truth for the spawn/exec decoupling migration.

Goals:
- Remove kernel hardcoding of userland program names and paths.
- Keep kernel focused on mechanism (`exec`/spawn-by-path), not launch policy.
- Move service orchestration policy to userspace init.
- Preserve deterministic boot (`/sbin/init` remains the only kernel-launched user process).
- Keep migration incremental with compatibility paths until all callers are moved.

---

## 2. Current Problem

1. Kernel contains a static `PROGRAM_TABLE` (`core/src/exec/mod.rs`) with name/path/priority/flags.
2. `SYSCALL_SPAWN_TASK` resolves names through kernel lookup (`core/src/syscall/handlers.rs`).
3. Userland launch policy is partially coupled to kernel table entries.
4. Adding/changing service names and paths requires kernel changes.

---

## 3. Target Architecture

1. **Kernel provides generic process launch primitives only**
   - Keep `exec(path, ...)` for current-process image replacement semantics.
   - Add or standardize a spawn-by-path syscall for creating a new task/process from an executable path.
2. **Userspace owns launch policy**
   - Init/service manager resolves program names to paths.
   - Init decides startup ordering, restart behavior, and policy.
3. **Minimal kernel policy remains**
   - Kernel launches `/sbin/init` only during boot services phase.
   - No kernel-owned name registry for user applications.
4. **W/L accounting is explicit**
   - Spawn/exec success awards win.
   - Recoverable launch failures award loss.

---

## 4. Compatibility Contract

During migration:
- Existing `process::spawn(name)` callers must continue to work.
- Legacy name-based syscall may remain as a compatibility wrapper.
- New path-based API becomes preferred for all new code.

After cutover:
- Kernel no longer owns a static program name table.
- Name-to-path resolution exists only in userspace policy code.
- `SYSCALL_SPAWN_TASK` is removed or permanently documented as compatibility-only.

---

## 5. Implementation Phases

## Phase 0: Baseline and Scope Lock

Deliverables:
- Capture current spawn/exec call graph and known userland callers.
- Freeze compatibility expectations for boot and tests.

Primary targets:
- `core/src/exec/mod.rs`
- `core/src/syscall/handlers.rs`
- `userland/src/syscall/process.rs`
- `userland/src/init_process.rs`
- `userland/src/shell.rs`
- `userland/src/compositor.rs`

Acceptance:
- Baseline references recorded in PR notes.
- No behavior changes landed.

## Phase 1: Introduce Path-First Spawn API

Deliverables:
- Add a path-based process creation syscall (new syscall number or repurposed compatibility layer).
- Define argument/limit contract for path, argv, envp, and spawn attributes.

Primary targets:
- `abi/src/syscall.rs`
- `core/src/syscall/handlers.rs`
- `core/src/exec/mod.rs`
- `userland/src/syscall/process.rs`

Acceptance:
- Userspace can spawn by absolute path without name lookup table.
- Existing name-based path still works via compatibility behavior.

## Phase 2: Move Name Resolution to Userspace Init

Deliverables:
- Introduce userspace registry in init (initially static table or manifest parser).
- Convert init service launch from kernel-name calls to userspace name->path resolution.

Primary targets:
- `userland/src/init_process.rs`
- `userland/src/bin/init.rs`
- optional userspace service registry module/file

Acceptance:
- Init launches roulette/compositor/shell by resolving names in userspace.
- Kernel no longer required to know those names.

## Phase 3: Migrate Remaining Callers to Path-Based Spawn

Deliverables:
- Migrate shell/compositor and other userland callers from `spawn(name)` to path-first API or init-managed service requests.
- Keep error mapping stable for callers.

Primary targets:
- `userland/src/shell.rs`
- `userland/src/compositor.rs`
- any other userland callers of `process::spawn`

Acceptance:
- No direct dependency on kernel name lookup remains in active userland code.

## Phase 4: Remove Kernel PROGRAM_TABLE

Deliverables:
- Remove `ProgramSpec` registry and `resolve_program_spec` lookup from kernel exec module.
- Keep `launch_init()` minimal and path-only (`/sbin/init`).

Primary targets:
- `core/src/exec/mod.rs`
- `core/src/exec/tests.rs`
- `core/src/syscall/handlers.rs`

Acceptance:
- Kernel compiles and boots without static application table.
- No behavior regression in init launch and userspace startup.

## Phase 5: W/L and Error-Hardening

Deliverables:
- Ensure spawn/exec success/failure paths explicitly integrate W/L currency.
- Harden path/argv/env copying, bounds, and error reporting.

Primary targets:
- `core/src/syscall/handlers.rs`
- `core/src/exec/mod.rs`
- `drivers/src/wl_currency.rs` integration call sites

Acceptance:
- Recoverable failures: `award_loss()` path covered.
- Successful launches: `award_win()` path covered.
- No unsafe widening of user pointer attack surface.

## Phase 6: Cleanup and Docs

Deliverables:
- Remove legacy syscall wrappers if no longer needed.
- Update docs to reflect userspace-owned launch policy.
- Update plans index and migration notes.

Primary targets:
- `plans/README.md`
- `docs/PRIVILEGE_SEPARATION.md` (if it references outdated model)
- syscall documentation

Acceptance:
- Documentation matches shipped behavior.
- No dead compatibility paths remain undocumented.

---

## 6. Safety Rules During Refactor

1. Do not change boot determinism: kernel must still launch `/sbin/init` only.
2. Do not mix policy into kernel while adding path-based APIs.
3. Keep argument and pointer validation strict and bounded.
4. Land in small, verifiable commits per phase.
5. Maintain W/L accounting on all new launch paths.

---

## 7. Execution Ledger

Update this table as work lands.

| Phase | Status | PR/Commit | Evidence | Notes |
|------|--------|-----------|----------|-------|
| Phase 0 | Done | working tree | call graph captured in `core/src/exec/mod.rs`, `core/src/syscall/handlers.rs`, `userland/src/init_process.rs`, `userland/src/shell.rs`, `userland/src/compositor.rs` | Baseline recorded and verified during migration. |
| Phase 1 | Done | working tree | `abi/src/syscall.rs` adds `SYSCALL_SPAWN_PATH`; `core/src/syscall/handlers.rs` adds `syscall_spawn_path`; `userland/src/syscall/process.rs` adds `spawn_path_with_attrs()` | Path-first spawn syscall is now primary. |
| Phase 2 | Done | working tree | `userland/src/program_registry.rs`; `userland/src/init_process.rs` resolves names in userspace and spawns by path+attrs | Name resolution moved out of kernel into userspace policy. |
| Phase 3 | Done | working tree | `userland/src/shell.rs` and `userland/src/compositor.rs` migrated to userspace registry + `spawn_path_with_attrs()` | Active userland callers no longer depend on kernel name lookup. |
| Phase 4 | Done | working tree | `core/src/exec/mod.rs` no longer contains `PROGRAM_TABLE`, `ProgramSpec`, or `resolve_program_spec`; `core/src/exec/tests.rs` updated | Kernel only launches `/sbin/init` at boot. |
| Phase 5 | Done | working tree | `core/src/exec/mod.rs` integrates `wl_currency::award_win/award_loss`; `core/src/syscall/handlers.rs` integrates W/L in `syscall_exec` | Launch success/failure paths now explicitly account W/L. |
| Phase 6 | Done | working tree | `tests/src/lib.rs` updated for removed legacy exec tests; this plan updated with landed evidence | Legacy name-based path removed from active code and docs synchronized. |

Evidence examples:
- command output summary (`make test`, `make boot-log`)
- file path references
- removed symbol confirmations (e.g. `PROGRAM_TABLE` removed)

---

## 8. Definition of Done

- [x] Kernel has no static user-program name registry.
- [x] Userspace init owns name->path policy.
- [x] Path-based spawn API is primary and documented.
- [x] Legacy name-based compatibility path removed or explicitly frozen.
- [x] W/L accounting integrated for launch success/failure paths.
- [x] `make test` passes with no new critical regressions.
- [x] Boot-to-init and init-driven service launch remain deterministic.

---

## 9. Out of Scope

1. Full POSIX process model completion.
2. Full service supervision/restart framework with dependency graph.
3. Package manager/runtime installer design.
4. Non-ext2 rootfs format migration.
