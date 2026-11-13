---
name: ratchet-remeasure
description: >
  Re-measure a tracked gate file under `scripts/gates/` after a gate fails on
  a cap, an allowlist or a recorded list. Use when `xtask elf`, `run_tests
  gate`, `xtask base-interface` or `xtask libc-exports` rejects a change you
  meant to make.
---

A ratchet failure is a measurement to re-take, not a number to raise. First
decide whether the change should move it: a frame that grew, a lock order
that inverted or a call path that reached a power primitive may be the bug.

## Read first

- `docs/verification/tcb-discipline`: what each gate holds and why
- `docs/verification/toolchain-gates`: the toolchain and interface gates
- `docs/development/adding-tests`: the log gates and the test list

## Rules a change must keep

- Never edit a file under `scripts/gates/` by hand to make a run pass.
- Re-measure in the commit that causes the delta, and say in its message
  which lock, test, account, frame or call added it.
- An allowlist entry that matches nothing fails the gate: a shrunk frame or a
  removed test hands its exemption back in the same commit.
- `sched` is not a ratchet: `eligible-lag` describes the boot sequence. A
  failure is a CPU that stopped scheduling.
- `lockdep` grades violations and pool capacity, never counts; a cap line in
  its file is refused.
- The `quota` and `fsperf` caps are measured maxima. Measure over several
  captures (`--log` once per boot) so the cap carries the spread.
- An issued `base-interface.txt` row is never rewritten; a changed surface
  takes the next number.
- `libc` exports and layouts only grow; a removal is a hand edit a reviewer
  must see, and is the one exception to the first rule.

## How

- ELF gates (`stack`, `vector`, `authority`): `cargo run -p slopos-xtask --
  elf --variant <dev|release|tests> builddir/kernel-<variant>.elf
  --emit-allowlist <gate>` writes `scripts/gates/<gate>/<variant>.txt`.
- Log gates (`tests`, `quota`, `fsperf`, `lockdep`): `just ci-suite` writes
  `builddir/ci-test.log`; then `builddir/run_tests gate <name> --variant
  tests --emit-allowlist --log builddir/ci-test.log >
  scripts/gates/<name>/tests.txt`.
- Base interface: `just base-interface --record`, with `BASE_INTERFACE` in
  `slopos-abi` raised by one. `--print` shows the surface for a diff.
- libc: `just libc-exports --record`, after a userland build.
- Codegen and linker verdicts (`scripts/gates/{codegen,linker}/`) fail in
  both directions: a `lacks` the probe now finds present is a decision to
  re-take, not a line to flip.

## Checks

- `just check-capture`: every log gate over the last capture
- `just check-framekernel-gates`: the source gates and the dev kernel's

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
