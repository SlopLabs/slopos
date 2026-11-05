# CI Latency

## The model

Two costs, and both count: the wall clock, which is the longest job, and the
runner minutes, which are the sum of every job. A job pays its setup — checkout,
toolchain, apt, cache restore, ~1.3 min for the suite's — and whatever it must
build before it does any work, so a check joins a job that already has its
inputs rather than getting a machine of its own.

Every job is `.github/actions/setup` followed by one `just ci-*` recipe, so
the YAML carries no logic and a red job reproduces locally with the command
it ran; `just ci` runs every lane in CI's order.

| Job | Recipe | Chain |
|-----|--------|-------|
| `gates` | `ci-gates`, `ci-candidates` | fmt → host tests (Rust and Go) → clippy → dev kernel and its ELF gates → `check-framekernel-gates` → offline build → release kernel and its gates → candidate backends |
| `suite` | `ci-suite` | tests system build → the suite's boot, captured → test list, lockdep and SMP gates → its image held to e2fsck and the map → the tests kernel's authority gate → test-persist → test-rude-exit → test-usb → test-toolchain |
| `toolchain` | `ci-toolchain` | llvm-project, tests userland, rustc source → clang driver, C++ pin, libc exports, recipes, LLVM port, cross-build plan, built-in target |
| `ostd-verify` | `check-miri`, `verify` | KernMiri → Verus |

Before clippy, the last ten green runs on `develop` took, median: suite 11.3
min, gates 8.3, toolchain 7.7, ostd-verify 5.2 — 32.6 runner-minutes, 11.3 of
wall clock. Of the suite's 11.3, the four boot checks after the suite are 1.6.
Clippy adds ~2.6 min to `gates` (cold on four cores: `builddir/clippy-*` is not
cached), which keeps it under the suite.

The rules that keep it there:

- A check joins the job whose inputs it reads. Another machine costs a setup
  and a build — several minutes — to save a boot of under one.
- Compiler output is cached by content, never by mtime, so a run pays only for
  the translation units whose inputs moved: ccache for the C++ builds, sccache
  for the Rust ones. Both restore by prefix and save only from a run that
  missed; sccache saves only from pushes, because nearly every commit misses
  somewhere. The workflow sets `CI_RUSTC_WRAPPER=sccache` and the recipes put
  it on their build steps only — a gate that probes what a compiler does
  (`unsafe-expansion`, the codegen and linker probes, the bootstrap and target
  checks) gets no cache, since a hit would be a cached answer.
- A check whose inputs are ready runs even when an unrelated check before it
  in the same lane failed (`ci-suite` and `ci-toolchain` collect failures and
  fail at the end), so merging checks into a lane costs no signal.

## Open work

### The suite's time is eleven userland tests

A KVM capture's tests take ~350 s: all 3031 kernel tests ~44 s, the userland
phase ~306 s, and eleven utests ~263 s of it — `priv` 56 s, `privd` 53 s,
`sessions` 31 s, `apps_trees`, `extension_service`, `accounts`,
`extension_action` and `live_base` ~16 s each, `extension` 15 s, `rename` 12 s,
`shell_script` 10 s. Where each waits — a consent or password timeout, a
drain, a backoff — is the next thing to measure; it is most of the suite's
wall clock and none of it is the kernel's.

### Every job pays its own setup

Each job installs the pinned nightly with all six `rust-toolchain.toml`
components and its apt packages. A runner image with the toolchain baked in
removes both from every job's wall clock.
