---
name: slopos-gates
description: >
  The kernel's unsafe-code surface, its style rules, and the source, ELF and
  clippy gates that hold them. Use when a task touches `tools/xtask`,
  `slopos-ostd`, `kernel/clippy.toml`, `scripts/gates/`, or mentions `unsafe`,
  `# Safety`, an allowlist, `xtask lint`, `xtask elf`, `xtask clippy`,
  `tcb-ratio` or a failing gate.
---

## Read first

- `docs/verification/tcb-discipline`: every gate, what fails it, how to run it
- `docs/verification/framekernel`: why only OSTD may hold `unsafe`
- `docs/verification/ostd-trusted-core`: how OSTD turns safety rules into types
- `docs/development/unsafe-ffi-boundaries`: adding an OSTD primitive or registry; style rules
- `docs/verification/toolchain-gates`: the `scripts/check_*.sh` toolchain and pin gates
- `docs/verification/kernmiri`: running OSTD's tests under Miri
- `docs/verification/verus-status`: what is proven and what a gate holds instead

## Rules a change must keep

- Only `slopos-ostd` writes `unsafe`. Every other crate the kernel links keeps
  `#![forbid(unsafe_code)]`; the one exempt file is `kernel/src/main.rs`.
- Never answer a gate by widening it: no allowlist entry (`SOURCE_ALLOWLIST`,
  `SECTION_ALLOWLIST`, `SYMBOL_ALLOWLIST`) without an audited reason, no
  hand-raised cap; re-measure with `--emit-allowlist`. An entry that matches
  nothing fails its gate, so remove it with the exemption.
- No `# Safety` section on a safe fn. Express the obligation as a type
  (witness, validated newtype, one-shot handle, sealed trait, slice) or make
  it `unsafe fn` inside OSTD.
- No `mem::forget` or `ManuallyDrop::new` in kernel crates; a clippy exception
  is a site-local `#[expect(lint, reason = "...")]`, never a blanket `allow`.
- Kernel crates use OSTD's containers, never `alloc`, and contain no `async`.
- A kernel frame stays under 2 KiB; nothing may exceed the 4 KiB guard page,
  which no allowlist covers. Build large values in place.
- No x87, MMX, SSE or AVX instruction in the kernel (no target-feature
  `RUSTFLAGS`, no vector `asm!`): kernel entry does not save that state.
- A new check is a Rust module in `tools/xtask` (or a Go log gate in
  `tools/run_tests/gates/`) with planted-violation tests, never a new
  `scripts/check_*.sh`. Keep its `//!` invariant current.
- Default to no comment; `// SAFETY:`, `# Safety` on `unsafe fn`, `///` API
  docs and asm register contracts are the exceptions.

## Checks

- `cargo run -p slopos-xtask -- lint <gate>`: one source gate (`xtask list` names them)
- `cargo run -p slopos-xtask -- elf --variant dev builddir/kernel-dev.elf`: the per-build ELF gates
- `just clippy kernel`: clippy with the kernel's bans
- `just tcb-ratio`: the unsafe-line ratio against its 1.0 % cap
- `just test-host`: every gate's own fixtures
- `just check-framekernel-gates`: all source, ELF and script gates (after `just build`)

## Not yet in the docs

Read `pending.md` beside this file when it exists. A change that adds or alters an invariant the docs do not state appends it there in the same commit.
