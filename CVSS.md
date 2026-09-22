# SlopOS Vulnerability Audit and CVSS Scoring

**One finding is open.**

Swept 2026-09-16: the editor — `editor-core`'s buffer, lexer, search and tree
model, the new `appkit` surfaces, the fd-based clipboard transfer in
`windowing`, and the editor's own filesystem boundary, every one of which reads
input the user did not write. Nothing reached the confidence bar. The editor
holds the user's own authority, so a file it opens or writes crosses no
boundary; what the sweep found instead were robustness and data-integrity gaps,
all closed inside the same unreleased change and so never entries.

Swept 2026-09-14: the utilities becoming executables — a multicall binary whose
archive extractor, patch applier, regex engine, inflater and digest all read
attacker-supplied input, plus the `/bin` symlink install and the `execve`
thread-pointer reset. Three guaranteed defects and six below the confidence bar,
all fixed inside the same unreleased change, so none is an entry.

> **Pre-alpha ledger policy.** SlopOS is pre-alpha with no
> backwards-compatibility or audit-trail obligations, so this file tracks **open
> findings only**. A resolved finding is **removed**, never kept as a `fixed`
> record, and a defect fixed inside the change that introduced it never becomes
> one. IDs stay stable while a finding is open and are never reused, so gaps are
> expected. The git history is the audit trail: `git log -p -- CVSS.md` recovers
> any entry that was here, and a fix's own test is the durable record of it.

Swept 2026-09-19: the dynamic loader — the kernel's `PT_INTERP` path and the
userland linker behind it, both of which parse attacker-chosen ELF. Twenty-two
defects across three reviewers, every one fixed inside the same unreleased
change and so not an entry. The sweep also proved a **pre-existing** defect the
interpreter work made visible by contrast, which is the entry below: the
executable's own segments have never been covered by a VMA.

Swept 2026-09-19: the C++ runtime — the cross-built `libc++`/`libc++abi`, the
libc surface it needed (`<math.h>`, `<ctype.h>`, the `strto*` family,
`pthread_once`, the `__cxa_*` trio), the Level-1 unwinder now in every image's
`libc.so`, and the `.init_array` walk a static program needs. Three reviewers.
One memory-safety defect, found by the third: `atexit(3)` registers a null
`__dso_handle` — slibc has one shared `atexit` where glibc links a per-object
copy from `libc_nonshared.a` — so `finalize_range` never reclaimed an `atexit`
made from inside a `dlopen`ed object, and `exit` then called it through an
unmapped address. It is also a slot leak: a `dlopen`/`dlclose` loop consumes
the 256-entry table permanently. Fixed inside this change by testing the
handler and argument addresses against the unloaded span as well as the
handle, so not an entry. No privilege boundary is crossed either way — a
process can only do this to itself, and `dlopen` already runs code of the
caller's choosing. The rest of what the reviewers found was correctness
(`std::stod("0e1")` throwing on a spurious `ERANGE`, a missing sentinel guard,
a nonsense load-bias fallback), all closed in the same unreleased change.

Swept 2026-09-21: the commit ledger and the build-loop plumbing — every
charge point (`mmap`, `brk`, `mprotect`, `fork`, `exec`, `memfd` sizing, the
demand and stack-growth faults), every refund road including the unmap error
arms, `F_DUPFD_CLOEXEC`, the pipe `fstat`, the file map's admission change,
slibc's `posix_spawn` over the spawn primitive with its parent-side descriptor
plan, and the process exit path. Three reviewers, twenty-odd findings, all
closed inside the same unreleased change and so none an entry: an `exec`
refused after the point of no return (now sized and charged beside the old
image), `MAP_NORESERVE` lost across a `PROT_NONE` reservation, refunds missing
on the `munmap` and `MAP_FIXED` error roads and on a failed stack reset, and a
`posix_spawn` that answered `E2BIG` where it should have fallen back to
`fork`. The sweep also proved a **pre-existing** defect the ledger made
visible by measuring it: since the task-to-task switch, a process that ended
itself never had its address space destroyed or its registry entry retired —
the first exit-cleanup pass consumed the "last task left" latch the second
pass needed — so every self-exiting process leaked its frames, page tables and
one of the 1024 process slots until shutdown. Any user reaches the exhaustion
by exiting 1024 processes, but no boundary is crossed and nothing is read or
written that should not be, so it would have been an availability entry at
most; it is fixed here (`TASK_EXIT_LAST_IN_PROCESS`) with the build-loop
test's "commit comes back when a process exits" as its durable record, and is
therefore not an entry. Fixing it reached a second pre-existing defect:
an account row named its parent by a bare arena slot, and a released slot is
reissued under a new generation at once, so a process whose parent exited and
was replaced before its own deferred teardown ran credited its outstanding
charges to the stranger now in that slot — a row underflow that panics the
tests kernel and silently corrupts the ledger in a release one. Any user
reaches it with a fork-and-exit ordering (`exit_stress_test` finds it in
seconds), so it is an availability defect at most; the parent edge carries
the parent's generation now and a released row hands its children to the
grandparent, so it is fixed here and not an entry. The stress test then
reached a third, in the tests kernel only: the per-CPU klog capture ring took
a bare spin flag with interrupts on, so a writer switched out mid-append left
its CPU's next writer spinning forever, and a spinner holding an
interrupt-masking lock stopped acking TLB shootdowns and wedged every CPU
behind it. The shipped kernel registers no capture backend and so never
takes that lock; the ring masks interrupts while held, acks shootdowns while
it waits, and drops an append nested from an NMI, so it is fixed here and
not an entry.

The highest ID issued so far is **SLOPOS-2026-0056**. The next finding is
`SLOPOS-2026-0057`.

## Open findings

### SLOPOS-2026-0056 — the executable's `PT_LOAD` pages are mapped with no VMA

- **Status:** `open`
- **Confidence:** 92 (evidence 40 — every line read directly; exploitability 26
  — a deterministic crafted-ELF path that stays inside the attacker's own
  process, since `execve` can only narrow authority; reproducibility 26 — a
  hand-built ELF with a second `PT_LOAD` above the seeded data region)
- **CVSS:** `CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:N/I:L/A:H` — **6.1 MEDIUM**
- **Evidence:** `mm/src/process_vm.rs:1289-1382` (`load_segments_and_tls` maps
  every segment and inserts no VMA), `:1309-1311` (the only positional check is
  on the *lowest* segment), `:844-851` (`seed_fresh_layout`'s two pre-seeded
  regions, which the loader silently relies on), `:718-760`
  (`process_vm_reset_for_exec` walks the VMA tree, so an orphan survives an
  `exec`), `:1487-1497` (an already-present leaf is adopted without being
  zeroed), `mm/src/vma_region.rs:385-393` (`PagesAxis` is charged only on a VMA
  insert), `mm/src/elf.rs:639-645` (`validate_segment` permits any `p_vaddr`
  below `USER_SPACE_END_VA`).
- **Why it is open rather than fixed:** pre-existing and outside the change that
  found it. The fix is to give the executable the per-segment VMAs
  `place_interpreter` now installs and stop relying on the seeded code/data
  regions, which is a change to the load path of every process on the machine.
  `RegionPurpose::Code`/`Data` are written and never read, so nothing depends on
  the two seeded regions being one entry each.
- **Adjacent, same follow-up:** the eager mapping path chose raw
  `PageFlags::USER_RW`/`USER_RO`, neither of which carries `NO_EXECUTE`, so
  every eagerly mapped user page was executable. `map_segment_pages` now
  derives its flags from the segment's own `p_flags`, but the eager anonymous
  `mmap` (`mm/src/process_vm.rs:2433-2437`), the ring path (`:2196`) and COW
  resolution (`mm/src/cow.rs:96`) still use the raw constants and still drop
  the `NO_EXECUTE` the VMA-driven paths apply.
- **Repro:** build an ELF whose `PT_LOAD[0]` is at `0x400000` (so
  `min_vaddr == code_base` passes) and whose `PT_LOAD[1]` is at
  `0x5_0000_0000`; `execve` it, then `execve` something else in the same
  process. The second image's interpreter is placed by the gap finder at
  `PROCESS_MMAP_START_VA`, finds the first image's leaves still present, and
  adopts them un-zeroed — `map_segment_pages` zeroes only a frame it just
  allocated. The pages are also absent from the `Pages` quota throughout.

## Cadence

1. Sweep after each major milestone and before any release or PR handoff.
2. Re-scan what recent commits touched — at minimum syscall paths, memory
   management, filesystems and drivers.

## Triage workflow (strict order)

1. **List every finding first**, unscored.
2. Score confidence 0-100: evidence quality (0-40, direct code with exact
   `path:line`), exploitability clarity (0-30, a realistic attacker path),
   reproducibility (0-30).
3. Only **confidence >= 80** is a guaranteed issue.
4. Only a guaranteed issue gets a CVSS vector, computed with
   `python3 scripts/cvss_calc.py "CVSS:3.1/..."` so agents agree.
5. A finding below 80 with an attacker-reachable trigger is still recorded, with
   no vector and a note saying what evidence would raise it. One with no
   attacker-reachable trigger belongs in `plans/`, not here.

## Non-negotiable rules

- Never present a speculative issue as a CVSS-scored vulnerability.
- **Verify the claim before fixing it.** An entry can be wrong; read the code.
- A fix lands with a test that **fails without it** — confirmed by reverting the
  fix, not by assertion.

## Entry format

```markdown
### SLOPOS-YYYY-NNNN
- Title: one line, the defect rather than the symptom
- Status: `open` or `needs-retest`
- Confidence: NN — evidence NN, exploitability NN, reproducibility NN, with reasoning
- CVSS vector/score: `CVSS:3.1/...` — **N.N SEVERITY**   (omit when confidence < 80)
- Impact: what an attacker gets, in the tree's own vocabulary
- Evidence: exact `path:line` references, one per claim
- Repro: minimal syscall sequence, malformed artifact, or PoC steps; if none is
  safely possible, say why and give the nearest deterministic validation
- Remediation: the shape of the fix, not "add a check"
```

## Scoring notes

CVSS 3.1 Base Score; `0.1-3.9 Low`, `4.0-6.9 Medium`, `7.0-8.9 High`,
`9.0-10.0 Critical`.

SlopOS has no credential model, so "unprivileged local attacker" means any
process that can execute code: `AV:L/PR:L`.

A panic in a `#![forbid(unsafe_code)]` crate is an availability impact, never a
memory-safety one. Overflow checks are on in the dev and tests kernels and off
in release, so the same arithmetic defect is a panic in one build and a silent
wrong value in another — say which when scoring.
