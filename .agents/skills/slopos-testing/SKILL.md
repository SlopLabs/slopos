---
name: slopos-testing
description: >
  The SlopOS build and boot recipes, the in-kernel test harness, the host test
  runner and its log gates, the kernel command line, the diagnostic console
  and the CI lanes. Use when a task touches `justfile`, `ktesting/`,
  `tools/run_tests/`, `scripts/gates/`, `core/src/utests.rs`, `.github/`,
  `slopos-ostd/src/kconsole/`, or mentions `stest!`, `utest!`, `fixture!`,
  KTAP, `just test`, `just ci`, a boot option or a failed gate.
---

## Read first

- `docs/development/adding-tests`: writing, ordering and running tests; CI lanes
- `docs/getting-started/build-test-recipes`: what each `just` recipe does
- `docs/abi/ktap-test-output`: KTAP wire format, runner flags, log gates
- `docs/abi/boot-options`: every kernel command-line option
- `docs/getting-started/qemu-options`: variables, disks and devices QEMU gets
- `docs/development/diagnostics`: hangs, oopses, panics, the diagnostic console
- `docs/getting-started/troubleshooting`: setup, build, boot or test failures

## Rules a change must keep

- A test name never carries an order (no `aaa`/`zz`). Use `order = first|last`
  or `rank = N`; shared setup is a `fixture!`, which runs whatever the filter.
- A test that ends the machine or needs the host is `FLAG_EXPLICIT`
  (`utest!(…, explicit)`); it runs only when `tests.run` is its full name.
- A utest of an unprivileged path declares `caps = none, uid = 1000`; the
  default, uid 0 with every capability, hides the refusal.
- The `just test` filter is positional, and names begin with the crate:
  `just test 'slopos_mm::*'`, never `mm::*`.
- Adding, renaming or removing a test changes `scripts/gates/tests/tests.txt`:
  re-emit it in the same commit.
- A ratchet failure (`tests`, `lockdep`, `quota`, `fsperf`) is a measurement to
  re-take: `builddir/run_tests gate <name> --emit-allowlist --log <capture>`,
  and name the lock, test or account in the message. Never hand-edit
  `scripts/gates/`.
- The `sched` gate is not a ratchet: a failure is a CPU no longer eligible
  for placement. Fix the kernel, not the file.
- No gate reads a cached compiler answer: `CI_RUSTC_WRAPPER` wraps build
  steps only.
- A new CI check joins the lane that has its inputs.
- `kcommand!` goes in a kernel-only crate. Triggers only queue; commands run
  at the bottom half, never in NMI; no program reaches a destructive one.
- Before a commit: `cargo fmt --all`, `just ci` (or the lanes touched; on
  SlopOS `just precommit`), then the `caveman-commit` skill writes the message.

## Checks

- `just test 'slopos_testing::*'`: the harness's own self-tests
- `just check-tests-host`: the Go runner's and log gates' unit tests
- `just ci-suite`: the suite booted once, its capture graded as CI grades it
- `just check-capture`: every log gate, `quota` and `fsperf` too, over that capture
- `just ci`: every CI lane, in CI's order

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
