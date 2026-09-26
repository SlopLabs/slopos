# SlopOS Vulnerability Audit and CVSS Scoring

**No finding is open.**

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
not an entry. Run on a host without KVM it then found two waits whose only
releaser was the waiting CPU, both pre-existing: a dispatcher that dequeued
the current task after a raced wake spun on that task's own `on_cpu` flag
with every other CPU idle, and a user copy switched out mid-copy pinned a
reference to the address space that an exclusive syscall on a sibling
thread spun for under the process-VM lock, while dispatching the copier
took that same lock; the spin's budget broke the cycle by failing the
syscall, so a four-thread process lost a thread's guard-page `mprotect` as
`EPERM`. Any user reaches both with threads that block and wake under
load, an availability defect at most; the claim hands the current task
back and the copy holds off preemption while it holds the reference, so
they are fixed here and not entries.

Swept 2026-09-23: the network path a self-hosted build fetches through — the
TCP stack's chunked rings, window scaling, persist, retransmission and
keepalive timers and the way a connection's end reaches its socket; the
concurrent resolver; `tls-core`, whose record layer, handshake and certificate
path read what a server chooses; `curl`, `nc` and `getaddrinfo`; and the dev
disk's export, which reads a volume the guest wrote. Five review passes, each
by fresh reviewers. What the change introduced was closed inside it, so none
is an entry, and it fixed one **pre-existing** defect that would have reached
the bar. The resolver's cache was keyed on a 32-bit FNV-1a hash of the name
and compared nothing else, so two names whose hashes collide answered each
other — `glbvs.example` and `yacxa.example` do — and anyone who could make the
machine resolve a name of their choosing (a local process, or a server whose
redirect `curl` follows) could plant an address under any other name for its
TTL, the collision found offline in seconds. Plaintext protocols follow a
planted address; TLS refuses it at the certificate. The cache now compares
whole names, with `dns_tests`' collision case as the record, and a reply must
also echo its question where the ID and port alone were taken before. The rest
were availability defects, most of them a peer's to trigger: a `DataState`
allocation `.expect` that a handshake's last segment under memory pressure
turned into a kernel panic, a FIN sent ahead of queued bytes that the send
map's accounting caught as a panic, a late retransmission timer that left two
running, a shut window nothing probed, a lost FIN never resent, lost bytes
that only a timeout would resend, a retransmission overlapping bytes already
taken dropped whole, FIN_WAIT_1 held for good by a peer that only sent data,
and TIME_WAIT, LAST_ACK and keepalive each ending connections a reader or an
answering peer still needed; each has a test in `slopos_net::tests`. Below the
bar (confidence about 60): the CSPRNG is seeded from four TSC reads on a CPU
without RDRAND, so a ClientHello's random lets an observer
search the seed and with it the key share. Every x86-64 CPU of the last decade
and QEMU's `-cpu max` have RDRAND, and the plan records it as a limit.

Swept 2026-09-24: the toolchain running in the guest and building the kernel
— the user-mode trap stack, `wait4`'s usage report, the executable's segment
VMAs, zero-length user ranges, user copies that read a file-backed page in,
the buddy allocator's free lists, and the KTAP subtest path. Three review
passes, each by a fresh reviewer; the third found nothing above a nit.
What the change introduced was closed inside it, and it fixed four
**pre-existing** defects. A trap from user mode pushed from `TSS.RSP0`, which
sat a fixed 12 KiB above the frames of the round trip that entered user mode,
so a chain deeper than that wrote over those frames and the kernel took a
general protection fault returning through them; `devdisk_test` reached it as
it started, on the unoptimized tests kernel. That is kernel stack corruption
from user mode, fixed here by
starting every trap at the round trip's own RSP, with
`test_deep_user_trap_spares_the_round_trip` as the record. SLOPOS-2026-0056,
the executable's segments mapped with no VMA, is fixed by giving them the
per-segment VMAs the interpreter already had
(`test_a_large_image_owns_its_segments_and_the_heap_clears_it`); its adjacent
half, the paths that drop `NO_EXECUTE`, is carried forward as
SLOPOS-2026-0057. The buddy allocator walked a free list to take a buddy off
it, so freeing a large address space held its interrupt-masking lock for a
time proportional to the list and stalled every CPU behind it; any process
reaches it by exiting after a large allocation, an availability defect at
most, and the lists are doubly linked now. The worst was `fork`: the clone
recorded the parent's frames under the parent's lock but took the child's
references to them only after dropping it, so a sibling thread's `munmap` in
that window freed a frame the child then mapped — and, reallocated in time, a
frame of another process. Any multithreaded process that forks while another
of its threads unmaps reaches it; cargo does on every build, which is how the
guest found it, where the frame's metadata had already been released and the
fork failed. A read or write of another process's memory would have put it
above the bar; the snapshot now holds a reference on every frame it records
(`test_cow_clone_survives_a_sibling_unmap`), so it is fixed here and not an
entry.

Swept 2026-09-26: copy-on-write and the file-backed fault path, which the
guest's kernel build changed to map a page set's frame into a `MAP_PRIVATE`
mapping until the first store. The change leans on the COW marker, and reading
what the marker was trusted with found two **pre-existing** defects, fixed here
with a test each rather than entered. `mprotect` wrote `WRITABLE` onto every
present leaf, COW-marked or not, so a process that forked and then re-applied
`PROT_READ | PROT_WRITE` to its own range stored straight into frames its child
still mapped — one process writing another's memory, confidence 90, which
would have scored above the bar
(`test_mprotect_keeps_a_forked_page_copy_on_write`); the fork also left the
parent's read-only private pages unmarked, which the same `mprotect` reached.
And a write fault on a COW leaf was resolved without asking the region, so a
forked child stored to its `PROT_READ` pages
(`test_cow_write_to_a_read_only_region_is_fatal`). SLOPOS-2026-0057 is fixed
with them: the COW copy, the ring and the shared `memfd` mapping now take their
leaf flags from the region they map (`test_cow_copy_keeps_the_region_no_execute`).

The highest ID issued so far is **SLOPOS-2026-0057**. The next finding is
`SLOPOS-2026-0058`.

## Open findings

None.

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
