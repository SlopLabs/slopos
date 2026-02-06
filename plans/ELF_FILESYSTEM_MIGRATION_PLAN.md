# SlopOS ELF Filesystem Migration Plan

> **Status**: Approved Planning Baseline  
> **Date**: 2026-02-06  
> **Scope**: Clean breaking refactor to remove embedded user ELF loading and make filesystem binaries the only execution source.

---

## 1. Purpose

This document is the single source of truth for the ELF loading refactor.

Goals:
- Remove duplicated ELF loading implementations.
- Remove embedded user binaries from kernel image flow.
- Boot userland from filesystem binaries only (`/sbin/init` first).
- Improve maintainability with strict DRY/YAGNI/KISS boundaries suitable for kernel code.

---

## 2. Current Problems (Baseline)

1. ELF loading logic is duplicated in multiple subsystems.
2. Boot path still supports embedded user ELF flow and kernel-coupled userland bootstrap.
3. Filesystem ext2 adapter contains duplicated `FileSystem` implementation paths.
4. Layer boundaries are weakened by cross-crate coupling and link-retention hacks.

---

## 3. Target Architecture

1. **Single canonical program loader**
   - One implementation for address translation, segment mapping, zeroing, and validation.
   - All exec and spawn paths call this loader.
2. **Filesystem-only user program source**
   - User binaries are installed into rootfs image (`/sbin`, `/bin`).
   - Kernel and boot crates no longer embed user ELF payloads.
3. **Init-driven userland startup**
   - Kernel launches `/sbin/init` only.
   - Init owns policy for launching shell/compositor/other services.
4. **Clear crate boundaries**
   - `mm`: memory primitives only.
   - `core`: process/exec orchestration and syscall semantics.
   - `userland`: ring3 code and user runtime; no boot linker-section registration.

---

## 4. Breaking Change Contract

This is intentionally breaking and expected to invalidate old boot behavior.

After cutover:
- No automatic launch from embedded `.user_*` payload flow.
- No fallback to kernel-embedded user binaries.
- Boot fails fast with a clear reason if `/sbin/init` is missing or non-executable.

---

## 5. Implementation Phases

## Phase 0: Baseline Freeze

Deliverables:
- Capture baseline boot/test behavior and known regressions.
- Record current call graph for `exec`, spawn, and ELF load call sites.

Acceptance:
- Baseline artifacts saved in commit notes or PR description.
- No functional code changes.

## Phase 1: Loader Unification (Highest Priority)

Deliverables:
- Consolidate duplicate ELF load logic into one canonical path.
- Remove duplicate helpers from non-canonical module.
- Keep behavior parity for existing syscall `exec`.

Primary targets:
- `core/src/exec/mod.rs`
- `mm/src/process_vm.rs`
- `core/src/syscall/handlers.rs`

Acceptance:
- Only one loader implementation for segment copy/zero/address translation remains.
- `exec` path passes existing tests.
- No behavior regressions in process entry/stack setup.

## Phase 2: Rootfs Binary Packaging Pipeline

Deliverables:
- Build system emits user binaries and installs them into ext2 image.
- Define canonical paths: `/sbin/init`, `/bin/shell`, `/bin/compositor`, etc.
- Remove kernel image dependence on embedded user ELF blobs.

Primary targets:
- `Makefile` and rootfs image scripts/build steps
- userland binary artifact staging

Acceptance:
- Rootfs image contains expected binaries with deterministic paths.
- Boot image can start userland without `include_bytes!` user payload embedding.

## Phase 3: Init-First Boot Cutover

Deliverables:
- Replace kernel-side user app orchestration with `/sbin/init` exec.
- Remove boot-time userland linker-section registration path.

Primary targets:
- `userland/src/bootstrap.rs`
- boot init registration code paths
- kernel boot handoff flow

Acceptance:
- Kernel launches only `/sbin/init`.
- Init launches remaining user processes by policy.
- Missing init yields explicit panic/log and deterministic failure.

## Phase 4: Remove Dead Compatibility Paths

Deliverables:
- Remove no-op/legacy paths tied to embedded loader model.
- Remove force-link retention hooks no longer needed.
- Keep only compatibility aliases still used by active call sites.

Primary targets:
- `mm/src/process_vm.rs` legacy stubs
- `kernel/src/main.rs` link guards
- boot/userland coupling points

Acceptance:
- Dead paths removed with clean compile.
- No hidden dependency on removed flow.

## Phase 5: ext2 VFS DRY Cleanup

Deliverables:
- Collapse duplicated ext2 `FileSystem` implementations into one shared implementation path.
- Keep static/global mount semantics unchanged unless explicitly refactored.

Primary targets:
- `fs/src/ext2_vfs.rs`

Acceptance:
- Single authoritative ext2 `FileSystem` behavior path.
- Existing VFS/ext2 tests remain green.

## Phase 6: Hardening + Completion Gate

Deliverables:
- Add integration tests for init launch and filesystem exec path.
- Confirm no embedded user ELF data path exists.
- Finalize migration notes and cleanup checklist.

Acceptance:
- `make test` passes.
- Boot-to-init and init-to-shell path verified.
- Done criteria in Section 8 all checked.

---

## 6. Safety Rules During Refactor

1. Do not simplify away required kernel invariants (permissions, paging semantics, trap safety).
2. Keep unsafe changes tightly scoped and reviewed with explicit rationale.
3. Prefer behavior-preserving moves before behavior-changing steps.
4. Land in small, verifiable commits per phase.

---

## 7. Execution Ledger (Retrospective Proof)

Update this table as work lands.

| Phase | Status | PR/Commit | Evidence | Notes |
|------|--------|-----------|----------|-------|
| Phase 0 | Planned | | | |
| Phase 1 | Planned | | | |
| Phase 2 | Planned | | | |
| Phase 3 | Planned | | | |
| Phase 4 | Planned | | | |
| Phase 5 | Planned | | | |
| Phase 6 | Planned | | | |

Evidence examples:
- command output summary (`make test`, boot log excerpt)
- file path references
- removed symbol/path confirmations

---

## 8. Definition of Done

All items must be true:

1. Exactly one canonical ELF loader path remains.
2. User binaries are loaded from filesystem paths only.
3. `/sbin/init` is the only kernel-launched user process.
4. Embedded user ELF loading codepaths are removed.
5. ext2 VFS duplication is removed or consolidated behind a single implementation source.
6. Test harness passes with no new critical regressions.
7. Execution ledger is fully populated with evidence links/notes.

---

## 9. Out of Scope (For This Plan)

1. Full POSIX process model completion.
2. Dynamic linker and shared object runtime.
3. Package manager or installer subsystem.
4. Non-ext2 rootfs formats.

