# KernMiri — dynamic UB detection on `slopos-ostd`

This directory anchors SlopOS's KernMiri harness — dynamic undefined-behaviour
detection for the trusted core (`slopos-ostd`). It's a thin layer of
`cfg(target_os = "none")` host stubs inside `slopos-ostd/` plus a
`just check-miri` recipe; there is no fork of Miri and no separate harness
crate.

## What this is

Asterinas's [KernMiri](https://arxiv.org/abs/2506.03876) (USENIX ATC '25)
is roughly 1,200 lines of shims that let stock Miri interpret the OSTD
kernel core. We follow the same pattern: every hardware-touching primitive
in `slopos-ostd` has a `cfg(not(target_os = "none"))` fallback (or, for
the heaviest naked-asm sites, a `cfg(miri)` / `#[cfg_attr(miri, ignore)]`
escape hatch). The OSTD algorithms — `Frame<M>` ref counting, `VmSpace`
cursor walking, `Slab` / `HeapSlot` lifetimes, spinlock / RCU / wait-queue
protocols — execute under Miri unchanged. Miri then watches every memory
op those algorithms perform and reports UB at the instant it happens.

## What this catches

The slopos-ostd lib unit tests + integration tests cover, among others:

- **Stacked / Tree Borrows violations** — the bug class that's most likely
  to hide inside the `unsafe` blocks in `slopos-ostd`. Code review and
  `cargo test` cannot see them.
- **Provenance violations** — pointers used outside the allocation they
  were derived from.
- **Use-after-free / double-free / OOB** in the heap, slab, and frame
  allocators.
- **Data races** on non-atomic memory across spawned threads.
- **Uninitialized reads** — anywhere `MaybeUninit` is dereferenced
  prematurely.
- **Alignment violations** — exactly the bug Miri caught in
  `slopos-ostd/src/util/ptr_buf.rs::with_at_mut` during this phase.

## How to run

```
just check-miri     # adds the miri component and runs `miri setup` if needed
                    # (first run ~5–10 min, building Miri's std)
```

`just check-miri` runs the full slopos-ostd unit + integration test
suite under Miri with isolation on, so the clock is virtual, runs replay
bit for bit, and the caller's environment never reaches the tests (a
`RUST_BACKTRACE=1` in a shell used to make every expected panic print an
interpreted backtrace). One flag is added:

- `-Zmiri-ignore-leaks` — host scratch allocations in
  `tests/{vm_space,uframe_round_trip,dma,io_mem,ecam,user_mode,virtqueue}.rs`
  are intentionally permanent (the test backing store outlives the
  OSTD references into it); their leakage is not a finding.

It runs as four concurrent `cargo miri test` processes — the two borrow
models crossed with lib-vs-integration — because Miri interprets every
thread of a process on one core, so libtest's thread pool buys nothing
and two back-to-back invocations leave the machine idle. cargo
serialises the four builds on the target-directory lock and releases it
before running the tests, so they share one `builddir/target/miri`.
Each shard's output lands in `builddir/kernmiri-<model>-<shard>.log`;
a failing shard's `failures:` block is echoed to stderr.

Sharding per *test* instead (`cargo miri nextest run`) is measurably
worse: a Miri process costs about a second to start and there are 643
of them, which is more CPU than the whole suite's interpretation.

## Keeping it fast

Profile with `-Zmiri-disable-isolation` (so `--report-time` reports wall
time) plus `-Zmiri-measureme=<dir>`, and read the result with measureme's
`summarize`; `perf` on the interpreter shows which Miri subsystem pays.
Three costs dominated and each has a structural answer:

- **Naming a large array `static` borrows all of it.** Miri prices a
  borrow by the extent and fields it covers, so a one-row lookup into the
  1025-row account arena cost ~220 ms. A `static` indexed per row is a
  `util::static_table::StaticTable`, which borrows one row at a time.
- **Walks over the account arena stop at the highest slot ever created**
  (`ROWS_IN_USE`), which is the peak process count, not the arena.
- **A wildcard access costs a Tree Borrows tree walk.** Host tests back
  physical memory with `mm::phys::init_phys_window`, so `phys_to_virt`
  hands out pointers that carry the arena's provenance.

Measured on a 4-core box, four shards in parallel: 19m16s -> 3m11s end to
end, bounded by the Tree Borrows integration shard.

Miri runs in its **default provenance mode**, which permits the
`expose_provenance()` / `with_exposed_provenance[_mut]()` round-trip
that OSTD's u64-typed phys-to-virt model relies on (see "Why not
strict provenance?" below).

## Provenance discipline (and why not `-Zmiri-strict-provenance`)

Host tests install their scratch arena with `mm::phys::init_phys_window`,
so `phys_to_virt` derives pointers from the arena itself and Miri checks
each access against a tag rather than resolving a wildcard. The window is
still exposed, because a small set of OSTD primitives
(`mm::io_mem::IoMem::{read,write}_volatile`, `boot::handoff::acpi`, the
`Frame` byte views) round-trip an address through an integer with the explicit
`core::ptr::with_exposed_provenance[_mut]` API together with a
matching `.expose_provenance()` call on the backing allocation. That
makes the intent of every integer-to-pointer round-trip auditable
and lets Miri's default-mode provenance model narrow the alias set
the synthesized pointer can reach.

We do **not** run `-Zmiri-strict-provenance`. Strict mode forbids
`with_exposed_provenance` outright — under strict provenance the
only legal way to change a pointer's address is `ptr.with_addr(...)`,
which requires keeping a live source pointer alongside the address.
OSTD models hardware-derived virtual addresses as bare `u64` (the
HHDM offset, MMIO virt base, ACPI table base) and there is no live
source pointer in the real kernel for those values — strict
provenance would require restructuring `FrameAlloc`, `IoMemMapper`,
and the HHDM contract to carry a `*mut u8` alongside every u64,
which is a Phase 2-scale refactor. Sentinel-token tests in
`tests/lock_graph.rs` and `tests/panic_recovery.rs` already use
`core::ptr::without_provenance(...)` (the strict-provenance-clean
construction for "opaque integer that is never dereferenced").

## CI integration

`.github/workflows/ci.yml` runs `just check-miri` in a dedicated `miri`
job that executes in parallel with the existing `Build, Format & Test`
job. UB caught by Miri blocks merge to `develop` the same way a failed
kernel build or test failure does.

Two cache layers and a trimmed setup keep the CI job short. The job
installs the pinned toolchain with `scripts/ensure_toolchain.sh
--no-sysroot` and no Go: it builds one host-target crate and interprets
it, so the owned `slopos` sysroot and the Go test wrapper are pure cost
there.

| Cache | Path | Key | Why |
|---|---|---|---|
| Miri sysroot | `~/.cache/miri/` | `hashFiles('rust-toolchain.toml')` | The expensive 5–10 min build; only re-runs when the pinned nightly changes. |
| Cargo registry + Miri target dir | `~/.cargo/...` + `builddir/target/` | `Swatinem/rust-cache@v2` with `prefix-key: v0-miri` | Separate from the main `ci` job's cache so they don't conflict on `target/.rustc_info.json`. |

Run locally to mirror what CI does:

```
just check-miri
```

The same recipe runs in CI, so anything that's green locally is green in
CI (subject to runner-side cache misses extending the wall time).

## Integration model

| Layer | Mechanism | Use |
|---|---|---|
| Host-vs-kernel impl pivot | `cfg(target_os = "none")` vs `cfg(not(target_os = "none"))` | Body-level fallbacks for `read_cr3`, `wrmsr`, port I/O, `cli`/`sti`, `invlpg`, etc. Miri uses the host triple, so the not-none branch is automatically chosen. |
| Miri-only impl pivot | `cfg(miri)` (auto-set by cargo-miri) | A handful of test-support fns whose real impl is `unsafe { asm!(...) }` — `read_cs`, `sgdt`, `read_lsr`, etc. |
| Miri-only test skip | `#[cfg_attr(miri, ignore)]` | Tests that exercise the heaviest naked-asm sites (`panic_recovery.rs`, parts of `task_handles.rs`), the `__ostd_usercopy_start/_end` binary layout, and a handful of `extern static` lookups that Miri does not model. |
| Dev wiring | `test-helpers` feature, auto-enabled by `dev-dependencies` | Exposes test-only constructors. `cargo miri test` picks it up automatically; no `--features` flag needed. |

We do **not** fork Miri. We do **not** ship a separate harness crate. The
shim layer lives entirely inside `slopos-ostd/`, follows the existing
`cfg(target_os = "none")` discipline already used by
`slopos-ostd/src/early_console.rs`, and stays out of the way of the real
kernel build.

## What runs vs. what's ignored

Everything runs under Miri except the targets below, whose ignores are
deliberate (counts shift as the suite grows; the *reasons* are the
stable part):

| Test target | Ignored under Miri — why |
|---|---|
| `--lib` (in-tree `#[cfg(test)]`) | `user::copy::fault_range_*` and `task::fpu::tests::xrstor_*` (real fault-path asm and its binary layout) |
| `tests/extern_block.rs` | `unsafe extern static` resolution Miri does not model |
| `tests/kernel_sync.rs` | `RefCell::borrow()` counter race demo |

### About the doctests

The Miri run names its targets (`--lib`, `--test '*'`), which excludes
the doctest target. Nothing is lost: OSTD's doctests are either
` ```ignore ` snippets that show the *syntax* of a macro against
placeholder types, or `compile_fail` snippets asserting that a
deliberate misuse is rejected. Both are claims about the compiler, not
about what the machine does at run time, and `just test-host` runs them
natively in well under a second.

## Where `MIRI_FINDINGS.md` is

There isn't one. By project convention, any UB Miri surfaces is **fixed
inline in `slopos-ostd/` source** and reported in the PR description rather
than accumulated in a findings file. The findings recorded when the harness
was first built were:

1. **Real UB**: `slopos-ostd/src/util/ptr_buf.rs::with_at_mut` could
   construct an unaligned `&mut [T]` if a caller passed a misaligned
   byte offset. Fix: `debug_assert!` on alignment + the in-tree test
   now uses a `#[repr(align(4))]` backing buffer.
2. **Soundness gap in a test (not in OSTD itself)**:
   `tests/kernel_sync.rs::refcell_u64_round_trips_across_threads` had
   four threads concurrently call `RefCell::borrow()`, racing on the
   non-atomic borrow counter. Benign on x86_64 but UB per the Rust
   memory model. Test ignored under Miri with an explanatory comment.
3. **Miri limitations** (not bugs, ignored under Miri): a handful of
   tests that depend on the real binary layout of `global_asm!`
   blocks or on `unsafe extern static` resolution.

## Background

- Asterinas KernMiri paper: <https://arxiv.org/abs/2506.03876>
- Miri repo: <https://github.com/rust-lang/miri>
