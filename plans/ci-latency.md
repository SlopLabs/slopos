# CI Latency

## The model

Two costs, and both count: the wall clock, which is the longest chain of
jobs, and the runner minutes, which are the sum of every job — setup
included, since each job pays ~20–45 s of checkout, toolchain install and
cache restore before it does any work.

Every job is `.github/actions/setup` followed by one `just ci-*` recipe, so
the YAML carries no logic and a red job reproduces locally with the command
it ran; `just ci` runs every lane in CI's order.

| Job | Recipe | Chain |
|-----|--------|-------|
| `gates` | `ci-gates`, `ci-candidates` | fmt → host tests (Rust and Go) → clippy → dev kernel and its ELF gates → `check-framekernel-gates` → offline build → release kernel and its gates → candidate backends |
| `suite` ×2 | `ci-suite K` | tests system build → boot of shard K of 2 → its image held to e2fsck and the map → the tests kernel's authority gate |
| `grade` | `check-capture` | after both shards: every log gate over both captures |
| `boot` ×3 | `ci-boot LANE` | `persist` (test-persist, test-rude-exit), `usb` (test-usb), `selfhost-root` (test-toolchain), each building what its boots need |
| `toolchain` | `ci-toolchain` | llvm-project, tests userland, rustc source → clang driver, C++ pin, libc exports, recipes, LLVM port, cross-build plan, built-in target |
| `ostd-verify` | `check-miri`, `verify` | KernMiri → Verus |

The boot checks used to queue behind the suite in one job; each is now a
machine of its own, so the wall clock is the build plus the longest single
boot rather than the build plus every boot in a row, and a failing USB boot
no longer skips the self-hosting root check. The price is runner minutes:
five jobs build the tests system where one did, each from a warm sccache.

The rules that keep it there:

- A check joins the lane whose inputs it reads; a boot that needs its own
  image or devices is a lane of its own.
- Compiler output is cached by content, never by mtime, so a run pays only for
  the translation units whose inputs moved: ccache for the C++ builds, sccache
  for the Rust ones. Both restore by prefix and save only from a run that
  missed; sccache saves only from pushes, because nearly every commit misses
  somewhere. The workflow sets `CI_RUSTC_WRAPPER=sccache` and the recipes put
  it on their build steps only — a gate that probes what a compiler does
  (`unsafe-expansion`, the codegen and linker probes, the bootstrap and target
  checks) gets no cache, since a hit would be a cached answer.
- A check whose inputs are ready runs even when an unrelated check before it
  in the same lane failed (`ci-toolchain` collects failures and fails at the
  end), so merging checks into a lane costs no signal.

## Open work

### The suite's shards build the same system twice

Each `suite` job builds the tests kernel, userland and images before booting
its half. Building once and handing the ISO, base and image to the shard jobs
as an artifact would save a warm build per shard, at the cost of moving a few
hundred MB through the artifact store.

### Every job pays its own setup

Each job installs the pinned nightly with all six `rust-toolchain.toml`
components and its apt packages. A runner image with the toolchain baked in
removes both from every job's wall clock.

### The suite's tail is three stress utests

`mm_stress`, `bigprog` and `exit_stress` are ~17 s of the userland phase; the
userland phase splits at test granularity, so a third shard would cut it,
for another runner's setup.
