# CI Latency

## The model

Two costs, and both count: the wall clock, which is the longest job, and the
runner minutes, which are the sum of every job — setup included, since each
job pays ~20–45 s of checkout, toolchain install and cache restore before it
does any work. So the tree runs four jobs, each a serial chain over one set of
inputs, sized to finish near the boot lane:

| Job | Serial chain | Warm cost |
|-----|--------------|-----------|
| `ci` | tests build → boot → graders of that capture → persistence and dev-disk boots | ~2:50, most of it the 65 s boot and the two short ones after it |
| `gates` | host tests → dev kernel → framekernel gates → offline build → release kernel → candidate backends | ~3:50, half of it `check-framekernel-gates` |
| `toolchain` | llvm-project, tests userland, rustc source → clang driver, C++ pin, LLVM port, cross-build plan, built-in target, cargo fork | ~3:10; ~9 min when the clang build's compiler cache is cold |
| `ostd-verify` | KernMiri → Verus | ~2:20, most of it Miri |

The rules that keep it there:

- A check joins the job whose inputs it reads. It goes in the boot lane only if
  it reads the boot's capture, and in a new job only if no existing job has its
  inputs and it would not fit beside the boot lane in one that does.
- Compiler output is cached by content, never by mtime, so a run pays only for
  the translation units whose inputs moved: ccache for the C++ builds, sccache
  as `RUSTC_WRAPPER` for the Rust ones. Both restore by prefix and save only
  from a run that missed; sccache saves only from pushes, because nearly every
  commit misses somewhere. sccache is on build steps only — a gate that probes
  what a compiler does gets no cache, since a hit would be a cached answer —
  and it is trusted because its key covers the target spec: adding one feature
  to `targets/x86_64-slos.json` misses on every kernel crate and hits only the
  host-built proc-macro dependencies.
- A check whose inputs are ready runs even when an unrelated check before it
  in the same job failed (`!cancelled()` plus its prerequisites' outcomes), so
  merging jobs costs no signal.

## Open work

### The gates job is the longest, and its builds are uncached

`check-framekernel-gates` runs without `RUSTC_WRAPPER` because one recipe
holds both kinds of step: `check_unsafe_expansion.sh`, whose ~50 s is compiling
feature-set variants of the kernel crates and could take the cache, and the
codegen and linker probes, which exist to re-ask a compiler and must not. Split
the recipe so the expansion builds run under sccache and the probes do not, and
the job drops below the boot lane.

### Every job pays its own setup

Each job installs the pinned nightly with all six `rust-toolchain.toml`
components and its apt packages. That is runner minutes four times over and a
floor under every job's wall clock; a runner image with the toolchain baked in
removes both.

### The boot is one capture on purpose

The ratchets grade one boot's counts, so the boot is not sharded. Its tail is
three stress utests (`mm_stress`, `bigprog`, `exit_stress`, ~17 s of ~56 s of
test time); making them cheaper is a test change, not a CI change.
