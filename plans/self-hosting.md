# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

One word decides the scope: the compiler must *run* here, not be *built* here.
A C++ runtime on SlopOS lets a cross-built LLVM run; a C++ compiler rebuilding
LLVM here is Phase 4, an order of magnitude further out and not committed.

**Scale, measured.** The dev kernel is 49 rustc invocations and 6 build
scripts, a 410 MB target directory, 42 s at `-j20` on the host, and a 1.2 GiB
peak for its largest single compile (`core`). The toolchain that has to run it,
cross-built for SlopOS, is `librustc_driver` 122 MB, `libLLVM.so` 83 MB (X86
only) and cargo 25 MB — sizes from a 2026-09-23 probe build linked against
stand-ins for the slibc gaps Phase 1.1 closes.

**Theme.** SlopOS's limits are appliance-sized constants and policies, not
architectural mistakes. The work is widening under proof — quantities derived
from the medium and from RAM instead of frozen at test-fixture values — not
redesign.

## Architectural constraints (do not violate)

- **Unsafe surface.** Only `slopos-ostd` may use `unsafe`; every other kernel
  crate stays `#![forbid(unsafe_code)]`, and `check_unsafe_expansion.sh` sees
  through macros. Nothing in this plan earns an exemption.
- **Allocation discipline.** `KBox`/`KVec`/`KArc`/`KBTreeMap` only. A
  toolchain-sized buffer becomes a chunked or page-list design, never a bigger
  single allocation: `MAX_ALLOC_SIZE` stays 1 MiB (`mm/src/slab/mod.rs:61`).
- **Stack frames ≤ 2 KiB** against a 4 KiB guard page, which is why paths are
  heap-backed (`CanonPath`, `UserPath`) and large structs are built with
  `KBox::try_init`.
- **Task ownership I1–I8, and no `async fn` in a kernel crate.** The sleepable
  fault path is a blocking task on its own kernel stack, not an executor.
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source;
  concepts, ABI numbers and layouts are free to take. Anything linked into a
  shipped binary needs a `NOTICE.md` entry; fonts stay runtime-loaded. libc++ is
  Apache-2.0 WITH LLVM-exception, which GPL-3.0-or-later accepts.
- **Ratchets are measurements, not numbers.** Re-measure with the gate's
  `--emit-allowlist` in the same commit and name the change that moved it.
- **`just boot`'s `verity=require` keeps meaning what it says.** The shipped
  image stays v1-verified and read-only; everything this plan makes writable is
  a different medium.

---

## What has landed

Each row is a piece of the foundation and the test or gate that keeps it true.
The mechanisms and their invariants live in the code and in `AGENTS.md` — read
those before touching a path. The notes after the table keep only what Phase
1.1 or a later phase depends on.

| Piece | Standing proof | Where |
|---|---|---|
| The loop this plan starts from — a persistent, writable root | `just boot-persist`, `test_ext2_clean_stamp_thaws_before_the_next_write` | `fs/src/ext2/`, `scripts/build_fs_image.sh`, `scripts/gen_verity.py` |
| A large program runs | `bigprog_test` | `mm/src/{elf,demand,process_vm}.rs`, `core/src/exec/` |
| A build system's floor is in place | `buildctl_test`, `scripts/check_syscall_abi.sh` | `abi/src/syscall/numbers.rs`, `core/src/syscall/`, `fs/src/fileio/flock.rs` |
| Storage holds a tree, and a write costs what it writes | `just test-capacity`, `scripts/check_fs_throughput.sh` | `fs/src/ext2/{dirindex,journal}.rs`, `fs/src/verity.rs`, `drivers/src/virtio_blk.rs` |
| The utilities are executables | `coreutils_test` | `userland/src/apps/coreutils/`, `userland/src/bin/coreutils.rs` |
| A script can loop, branch and substitute | `shell_script_test`, `shell-core` host tests | `shell-core/src/`, `userland/src/apps/shell/` |
| The terminal is one an editor can be written against | `terminal_grid_test` | `terminal-core/src/`, `vt/src/lib.rs`, `font/src/boxdraw.rs` |
| An editor you can work in | `editor_test`, `editor-core` host tests | `editor-core/src/`, `userland/src/apps/editor/`, `appkit/src/` |
| The target is a host's target | `libc_abi_test`, `scripts/check_toolchain_pin.sh` | `toolchain/{rust,libc}/`, `scripts/make_slopos_sysroot.sh`, `targets/x86_64-unknown-slopos.json` |
| The backend is measured, and it is still LLVM | `scripts/check_codegen_backend.sh`, `scripts/check_linker_script.sh` | `scripts/gates/{codegen,linker}/` |
| A program can be linked at run time | `dl_test` | `slibc/src/ld_so/`, `slibc/cdylib/` |
| A C++ exception crosses an object boundary | `cxx_test`, `cxx_static_probe`, `scripts/check_cxx_pin.sh` | `scripts/make_slopos_cxx.sh`, `toolchain/cxx/PIN`, `vendor/unwinding` |
| The libc surface is complete | `libc_probe` (spawned by `libc_abi_test`), `slibc-core` host tests | `slibc/`, `slibc-core/`, `userland/libctest/` |
| The target is a built-in target | `scripts/check_rustc_target.sh` | `toolchain/compiler/0001-slopos-target.patch`, `scripts/make_rustc_src.sh` |
| LLVM builds for this target | `scripts/check_llvm_port.sh`, `scripts/check_clang_driver.sh` | `toolchain/llvm/`, `toolchain/llvm-rustc/`, `slibc/builtins/` |
| The toolchain is cross-built and lands on a dev disk | `scripts/check_bootstrap_config.sh`, `scripts/check_cargo_fork.sh`, `just test-devdisk` | `scripts/bootstrap_slopos_toolchain.sh`, `toolchain/{compiler,cargo}/`, `scripts/build_devdisk.sh` |
| The build loop holds | `buildloop_test`, `exit_stress_test` | `mm/src/{commit,vma_region,memfd}.rs`, `slibc/src/process/spawn.rs`, `fs/src/{filemap,pipe}.rs` |
| Code gets in and out | `scripts/check_offline_build.sh`, `transfer_test`, `dns_concurrent_test` | `.cargo/vendor.toml`, `scripts/{make_vendor,export_devdisk}.sh`, `tls-core/`, `net/src/{dns.rs,tcp/}` |

### What Phase 1.1 builds on

- **The target.** `x86_64-unknown-slopos` is a unix-family target over slibc.
  std is upstream `sys/pal/unix` plus `target_os` arms (`toolchain/rust/`), and
  `libc` is 0.2.189 plus one `unix/slopos` module whose layouts are Linux's
  (`toolchain/libc/`). It is a built-in rustc target too (`Os::Slopos`,
  `Env::Slibc`); the built-in and JSON specs are one spec in two files, held
  equal by `check_rustc_target.sh`. It is tier 3, so `-Zbuild-std` and
  `-Zjson-target-spec` stay on every build line. `current_exe` is
  `Unsupported`: there is no procfs.
- **The loader.** `libc.so` is the interpreter (`/lib/ld-slopos.so.1`), so a
  process has one allocator, one `errno`, one TLS implementation and one object
  table. Binding is eager with full RELRO. TLS is general-dynamic, and a
  `TPOFF64` into a `dlopen`ed module is refused. `DT_VERSYM` is ignored, the
  object table holds 128, and the search path is `/lib` then `/usr/lib` only
  (`slibc/src/ld_so/mod.rs:57`).
- **The C++ runtime.** libc++ and libc++abi are whole-archived into one
  `libc++.so`, because two copies of libc++abi's caught-exception state in one
  process is a throw nobody can catch. libc++ was chosen over libstdc++ because
  libstdc++ only comes out of a GCC cross-compiler bootstrap — a second
  toolchain — while libc++ is built by the clang that is needed anyway.
  `libc.so` exports the seventeen `_Unwind_*` entry points and is built
  `-C force-unwind-tables`, because the first frame of every unwind is its own.
  `_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` is an ABI flag written down once, in
  `make_slopos_cxx.sh --print-abi-flags`. The runtime ships on the tests image
  and the dev disk, not the appliance root.
- **The C library.** The LLVM 18.1.8 build cost 120 C entry points — the
  POSIX-2008 locale objects and the `_l` family, `<wctype.h>`, wide stdio and a
  set of missing POSIX names — and four headers (`<wctype.h>`, `<inttypes.h>`,
  `<endian.h>`, `<sysexits.h>`). `libbuiltins.a` supplies compiler-rt to shared
  objects. Headers are generated from the same patch hunk the compiler reads,
  so a declaration cannot drift from its export. The rustc-pinned LLVM 23.1
  needs more; that is Phase 1.1.
- **Two LLVM ports.** `toolchain/llvm/` ports the 18.1.8 tree the C++ runtime
  is cut from, and `toolchain/llvm-rustc/` ports the 23.1 tree rustc's C++ shim
  is pinned to. Both carry the same `Triple` entry, the same two OS dispatches
  and the same `toolchains::SlopOS`, and they must not diverge.
- **The cross-build.** `just toolchain` is one `x.py` invocation,
  `--build=x86_64-unknown-linux-gnu --host=x86_64-unknown-slopos`. A wrapper
  stands in for the ported clang: it compiles for SlopOS and links under the
  Linux triple with lld and `--sysroot`. cargo is the fifth fork — `network`
  off drops curl, libgit2 and OpenSSL, and SQLite stays. The build has never
  completed; Phase 1.1 finishes it.
- **The dev disk.** `fs/assets/ext2-devdisk.img` is attached as virtio-disk4:
  2 GiB, trailer-less and unattested. It carries the target sysroot, the staged
  toolchain, and `src/slopos` — the committed `HEAD`, the vendored crates, an
  offline cargo config and `.slopos-base`. The tree is seeded once and is the
  guest's from then on; `just devdisk-export` turns the guest's edits into a
  patch. `devdisk_test` mounts the volume at `/tmp/devdisk`.
- **The build loop.** Every private mapping is promised against a commit ledger
  (`mem.commit`, default 100 % of usable frames) and refused at
  `mmap`/`brk`/`mprotect`/`fork` with `ENOMEM`; `MAP_NORESERVE` takes the
  fault-time road. `posix_spawn` is slibc over the kernel's `spawn`, so a
  compiler spawning its linker owes no second copy of itself. The jobserver's
  `R,W` pipes work, and `MAX_PROCESSES` and `MAX_PIPES` are 1024. The file map
  pins populated file pages up to a quarter of usable frames machine-wide, a
  sixteenth per process, and 256 MiB per file (`fs/src/filemap.rs:81-124`).
- **Offline.** `Cargo.lock` is tracked, and `third_party/vendor` holds every
  registry crate it pins plus the ones std's `library/Cargo.lock` pins. The
  guest has no registry to fall back on.

### Deliberate gaps carried forward

Kept because a later phase can trip on them.

- **Memory.** No swap and no OOM victim: a fault that finds no frame is
  `SIGBUS`, and the commit ledger is what makes that rare. User pages are
  4 KiB only.
- **ABI.** Linux's at the numbers, the constants and most layouts. Five
  layouts are not: `getdents64`'s `d_name` at offset 24, a truncated
  `ucontext_t`, `NSIG` 32, a `termios2`-shaped `struct termios`, and a 16-byte
  `signalfd_siginfo`.
- **Processes and locks.** The cwd is per-thread (`CLONE_FS` is ignored).
  Shared futexes are private-only. Advisory locks are 128 rows machine-wide,
  and deadlock detection catches self-conflict only.
- **Identity.** Single user, uid 0: `st_uid`/`st_gid` read 0.
- **Filesystem.** No on-disk htree — an `EXT2_INDEX_FL` directory is
  de-indexed on its first mutation — and mutations serialise per mount.
- **Loader.** No lazy binding, and `dlerror` is process-wide.
- **Missing features.** No `mkfifo`, no `vfork`, no `trap` in the shell, no
  `git` in the guest, no network for cargo, and no 9p or virtio-fs.
- **Terminal.** The kernel vconsole answers no terminal query.

---

## Phase 1 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

The toolchain is **LLVM, cross-built from Linux, with the C++ runtime ported**.
The Rust-hosted road — cranelift plus a Rust linker — was measured against this
kernel and does not reach it: soft-float is a property of cranelift's x64
backend rather than a flag, and two of its four gaps are silent. `rust-lld`
arrives in the same monorepo build as `libLLVM.so`, so `wild` is off the
critical path and clang comes with it. The backend and linker gates re-ask both
questions on every run.

The foundation is the lower half of the landed table, from "The target is a
host's target" down. What remains is Phase 1.1, the last part of Phase 1:
**Phase 1 closes when Phase 1.1's exit criteria hold.**

**Phase 1 exit criteria:** an in-guest `cargo build` of this repository's
kernel produces an ELF identical in behaviour and in its loadable image to the
host build, verified by booting it. Phase 1.1's exit criteria are the precise
form.

---

## Phase 1.1 — The toolchain runs, and builds the kernel

**Outcome:** the cross-build finishes, the guest runs rustc and cargo off the
dev disk, the kernel builds in-guest with no host tool, and the ELF it produces
boots and matches the host's.

**Where it starts (measured 2026-09-23).** Nothing for SlopOS has been produced
yet. `just toolchain` builds the host LLVM, a stage1 rustc and std for both
targets, then stops in the cross LLVM. A probe that replaced each failure with a
throwaway stand-in reached linked `clang-23`, `libLLVM.so`, `lld`,
`librustc_driver` and cargo. So the gaps in 1.1.1 are the whole list the build
shows, not the first of many.

**Exit criteria** (these close Phase 1):

1. `just toolchain` completes from a clean tree. `just test-devdisk` then finds
   rustc, cargo, rust-lld, clang and `ld.lld` on the volume, and no `NEEDED`
   entry outside SlopOS's own libraries.
2. Booted with the dev disk mounted from the kernel command line, the guest
   builds the dev and tests kernels with `scripts/build_kernel.sh`, using
   nothing a Linux host provides.
3. The guest-built tests kernel, exported and booted on the host, passes the
   kernel suite. `check_stack_sizes.sh`, `check_kernel_softfloat.sh` and
   `check_registry_sections.sh` pass on it.
4. The guest-built dev kernel's loadable image and symbol table are
   byte-identical to a host build of the same commit.

The steps below are in the order each unblocks the next measurement.

### 1.1.1 The cross-build finishes

- **Re-materialise the rustc tree first.** Commit 3ce32d67 changed
  `scripts/lib/toolchain_pin.sh`, an input of the tree's stamp. Until
  `just rustc-src` runs, `just toolchain` refuses a stale
  `third_party/slopos-rustc-src`.
- **slibc grows what LLVM 23.1 names.** Every item below was a compile or link
  failure:
  - **`<fenv.h>` in full.** `fenv_t`, `fexcept_t`, the `FE_*` constants, and
    the exception, rounding and environment functions over the x87 control word
    and MXCSR (musl's shape). LLVM-libc's `FEnvImpl.h`, which `APFloat.cpp`
    includes, needs the header. Nothing references the functions at link time,
    but a C library does not ship the header without them.
  - **Constants, structs and a `siginfo_t` field:**
    - `FD_CLOEXEC` (`Unix/Jobserver.inc:50`).
    - `AT_PAGESZ` with `<sys/auxv.h>`, and `struct winsize`
      (`Unix/Process.inc:73,324`).
    - `MADV_WILLNEED`, `MADV_RANDOM` and `MADV_SEQUENTIAL`, accepted by
      `madvise` as hints (`Unix/Path.inc:947,960`).
    - `si_pid` in `siginfo_t`, filled by the kernel for a signal a process
      sent (`Unix/Signals.inc:480`).
  - **Functions `libLLVM.so` links against.** It links `-z defs`, so these are
    link errors: `strpbrk`, `strtok`, `perror`, and `shm_open`/`shm_unlink` the
    way glibc has them — `open` and `unlink` under `/dev/shm`, a ramfs the
    kernel mounts at boot.
  - **For the Rust side:** `__errno_location`, `posix_fadvise`, `getpwnam`
    (answering the uid-0 entry), and `lutimes`.
- **The libc fork reaches the compiler.** Only `library/` is patched today, so
  the compiler and cargo resolve crates.io `libc`, which has no `slopos`
  module (`error[E0432]: unresolved import unistd`).
  - `[patch.crates-io] libc` goes into the root workspace and into
    `src/tools/cargo`, whose lock moves from 0.2.186 to 0.2.189.
  - The fork grows what rustix, memmap2, filetime and rustc name:
    - the 42 Linux errnos it lacks;
    - the termios `B*`/`CS*`/`TAB*`/`V*`/`TIOC*` names;
    - `POSIX_FADV_*`, `FALLOC_FL_*` and `O_ASYNC`;
    - `seekdir`, `mknodat`, `posix_fadvise`, `posix_fallocate` and `lutimes`;
    - `sigevent`, `_SC_ARG_MAX` and the `MADV_*` above.
  - `stat`'s time fields take Linux's spelling (`st_mtime` plus
    `st_mtime_nsec` — the same bytes), which is what `gix-index` reads.
  - Every function the fork declares is one slibc implements for real. The
    header generator already turns a missing one into a build failure.
- **Crate ports, pinned like the other forks.** These fail to compile for a
  SlopOS host even with the fork patched in:

  | Crate | Failure |
  |---|---|
  | `getrandom` 0.2.17, 0.3.3, 0.4.3 | `compile_error!`; its `libc` dependency is target-gated, so no `--cfg` backend helps |
  | `libloading` 0.9.0 | no `RTLD_*` values |
  | `nix` 0.30.1, via `ctrlc` | no errno table, no `sigevent`, and a signal-count const that fails to evaluate |
  | `rustix` 1.1.2 and 1.1.4 | libc-backend arms missing, and the ioctl `_Opcode` type undefined |
  | `errno` 0.3.14 | no `__errno_location` link name |

  `stacker` 0.1.21 compiles but falls back to "limit unknown", so it maps a
  fresh 1 MiB stack on most calls made from a thread's own stack. It gets the
  `pthread_getattr_np` arm, which slibc already provides. Each port is a
  PR-shaped patch under `toolchain/crates/`, pinned by checksum, applied by
  `make_rustc_src.sh` and wired in with `[patch.crates-io]`. That is the shape
  the `redox` arms already have upstream in getrandom, rustix, nix and errno.
- **The compiler links libc++, not the host's libstdc++.**
  `rustc_llvm/build.rs` picks `-lstdc++` unless `LLVM_USE_LIBCXX` is set, and
  the wrapper resolved that to the host GCC's library despite `--sysroot`. The
  fix is `llvm.use-libcxx = true` in the generated config and a wrapper that
  disables GCC detection. `check_bootstrap_config.sh` gains the check that
  would have caught it: no `NEEDED` outside `libc.so`, `libc++.so`, `libLLVM`,
  `librustc_driver` and `libstd`.
- **The install stages what the guest runs.** `x.py install` has no clang step,
  so the script:
  - copies `clang`, `libclang-cpp.so` and clang's resource directory;
  - installs a clang config file next to the binary, with a `<CFGDIR>`-relative
    `--sysroot`;
  - links `cc` to clang and `ld.lld` to `rust-lld`;
  - leaves the Linux rust-std (123 MB) off the volume.

  The toolchain is staged at `src/slopos/third_party/rust-slopos` — where the
  host keeps its owned sysroot — for 1.1.4's identity check. The dev disk grows
  to 4G: about 0.9 GB of toolchain including clang, 0.05 GB of source, and
  0.41 GB per kernel variant.

**Done when:** exit criterion 1 holds, and `readelf` over the staged objects
finds no `R_X86_64_TLSDESC`. The loader does not implement it. Everything built
so far uses `DTPMOD64`/`DTPOFF64`, and `TPOFF64` only in startup objects.

### 1.1.2 The toolchain runs in the guest

- **The loader finds libraries the way glibc and musl do.**
  - Search order: `LD_LIBRARY_PATH`, then `DT_RPATH` (only when the object has
    no `DT_RUNPATH`), then `DT_RUNPATH`, then `/lib` and `/usr/lib`, with
    `$ORIGIN` expanded.
  - A program whose `exec` conferred a grant finds `AT_SECURE` in its auxiliary
    vector, and the loader then ignores `LD_LIBRARY_PATH` and `$ORIGIN`, as
    Linux loaders do for setuid programs.
  - `rust.rpath = true` goes into the generated config and `has-rpath: true`
    into both specs, so rustc, cargo and rust-lld carry `$ORIGIN/../lib`.
  - `dl_test` gains cases for `$ORIGIN`, the precedence order and the
    secure-exec refusal.
- **The dev disk mounts from the kernel command line.**
  - Syntax: `mount=<source>:<path>`, repeatable. The source is a device name or
    `LABEL=<label>`; the guest's disk letters are positional, so the label is
    the stable name.
  - `build_devdisk.sh` labels the volume `slopos-dev`, and every root image
    carries an empty `/devel`.
  - A mount that fails is a klog line, just as an absent `root=` device is.
  - No shipped program gains `TASK_FLAG_MOUNT`.
  - `just boot-dev` boots the persistent root with the dev disk at `/devel` and
    4G of RAM.
- **Host programs link through the C compiler driver.** Both specs become
  `LinkerFlavor::Gnu(Cc::Yes, Lld::No)` with linker `cc`, as on Linux, the BSDs,
  illumos and Redox. `toolchains::SlopOS` already owns `crt0.o`,
  `--dynamic-linker=/lib/ld-slopos.so.1`, `-z now`, `--eh-frame-hdr`, `-lc` and
  `libbuiltins.a`, so build scripts and proc macros link in the guest with no
  per-project configuration. The system's own binaries keep pinning their link
  line in `build_userland.sh`: `-C linker=rust-lld -C linker-flavor=gnu-lld`
  beside the `-C relocation-model=static` already there.
- **The toolchain unwinds.** Both specs become `panic-strategy: unwind`, as
  every hosted target is. `FatalError::raise` is a `resume_unwind`, and a
  panicking proc macro should end as a diagnostic, not a SIGABRT. std's
  `unwind` crate takes `_Unwind_*` from libc. The system's own binaries keep
  `panic = abort` by pinning `-C panic=abort` on their build line, and
  `userland/userland.ld` keeps discarding their `.eh_frame`.
- **The session is sized for a compiler.** A dev session gets 4G of RAM, and
  `just test-devdisk` runs at 4G too. The measurements behind that:
  - The `core` compile peaks at 1.15 GiB of anonymous memory, and `-j4` peaked
    at 1.45 GiB across the process tree.
  - Each compiler process holds 90–105 MiB of file-mapped pages. The
    per-process cap is usable memory ÷ 16: 256 MiB at 4G, and under what one
    process needs below 2G.

  `CARGO_HOME` points into `/devel`, because `HOME` is `/` on a read-only root
  (`userland/src/apps/shell/env.rs:108`).
- **Smoke ladder, each step a `devdisk_test` case:**
  1. `rustc --version`. Records startup time and the number of relocations
     bound — the figure "no lazy binding" said to re-measure at this scale.
  2. `rustc` compiles and runs a hello-world, which proves the link policy.
  3. `cargo build --offline` of a crate with a build script and a proc macro,
     then run it. This exercises `dlopen` of the proc macro (which must carry
     no `TPOFF64`), cargo's spawn and jobserver road, and cargo finding its own
     executable with `current_exe` unsupported.
  4. clang compiles and links a C file. `getMainExecutable` must answer the
     binary's path through `dladdr` on the main executable, or clang finds
     neither its resource directory nor its config file.
- **Measured and recorded; fixed if they make the build impractical:**
  - `malloc` is one global spinlock (`slibc/src/mem/dlmalloc.rs:77`), shared
    by LLVM's codegen threads;
  - the `stacker` fix above;
  - the file map's per-process cap.

**Done when:** the ladder passes in `just test-devdisk`.

### 1.1.3 The kernel builds without host tools

- **One build driver for both machines.** `scripts/build_kernel.sh` becomes
  POSIX `sh`. The guest has `/bin/shell` and the coreutils; it has no bash,
  just, python, awk or rustup. rustup's `+<channel>` stays in the justfile,
  passed in as `CARGO`. `RUSTFLAGS` stays explicit: the
  `[target."targets/x86_64-slos.json"]` rustflags in `.cargo/config.toml` never
  apply (measured with `cargo build -v`).
- **The symbol table comes from a Rust tool in the workspace.** The kernel
  embeds a table generated from its own first-pass ELF: 40 207 symbols, a fixed
  point after the second pass. A dependency-free ELF64 `.symtab` reader
  replaces `gen_kernel_symbols.py` and its `llvm-nm`, producing the same
  `kallsyms-<variant>.rs`: types `t`/`T`/`w`/`W`, addresses from
  `0xffff800000000000`, deduplicated and escaped. It is built for whichever
  machine runs the build, and the Python script is deleted.
- **The safestack stub is written, not archived.** The driver writes the empty
  `librustc-nightly_rt.safestack.a` (`!<arch>\n`) into the sysroot itself;
  `safestack_stub.sh`'s dependence on rustup and `llvm-ar` goes.
- **Both sides build with the owned sysroot:** `cargo +slopos` on the host, the
  staged toolchain in the guest. 1.1.4's identity check depends on it.

**Done when:** exit criterion 2 holds, and the host build through the new
driver passes `just test` and the ELF gates.

### 1.1.4 The ELF comes out, boots and matches

- **Out.** `export_devdisk.sh` emits only a patch, and `builddir/` is ignored.
  `just devdisk-export-file PATH=… OUT=…` dumps one file with `debugfs`, behind
  the refusals `devdisk-export` already has: a volume not marked clean, or one
  `e2fsck -fn` rejects.
- **Boot.** `just boot-elf ELF=…` and `just test-elf ELF=…` stage a given ELF
  through `build_iso.sh`'s `KERNEL_ELF` without rebuilding. Every recipe today
  depends on `build`, which deletes `builddir/kernel-<variant>.elf` first.
- **Identity.**
  - **What is already deterministic.** Three `-j` settings and two target
    directories gave identical loadable images; only `.debug_line` differs.
  - **What moves code.** Where std's sources live. With the same sources at
    another absolute path, `core`'s crate hash changed and the change cascaded:
    `.text` came out 1 288 bytes smaller and 85 symbols were renamed.
  - **Why the layout fixes it.** Cargo hashes a path source outside the
    workspace by its absolute path, and one inside by its path relative to the
    workspace root. With the guest's toolchain at
    `src/slopos/third_party/rust-slopos` (1.1.1), both builds see std at the
    same workspace-relative path.
  - **Embedded paths** are normalised with cargo's `trim-paths`.
  - **First step, host only:** two checkouts at different absolute paths must
    produce identical loadable images.
  - **The comparison:** `llvm-objcopy -O binary` of both ELFs plus their symbol
    tables, with debug sections excluded. A remaining difference between the
    host rustc and the cross-built one is a finding to explain, not a tolerance
    to set.

**Done when:** exit criteria 3 and 4 hold.

### Prior art

- **Redox** (January 2026) runs natively a rustc and cargo that were
  cross-built on Linux. It has built relibc, ripgrep, cbindgen and its own test
  suite in-guest, and submitted its first merge request from inside Redox.
  - **Fixes that map onto this phase:**
    - an `mremap` panic cargo triggered;
    - spurious `futex` wake-ups read as timeouts;
    - `TPOFF` relocations with an undefined symbol index;
    - an allocator mismatch between relibc and its loader;
    - thread-destructor and thread-creation races.
  - SlopOS already handles the `TPOFF` case (`slibc/src/ld_so/reloc.rs:150`),
    and its one-libc design rules out the allocator one.
  - Redox's loader reads `LD_LIBRARY_PATH` and expands `$ORIGIN` in
    `DT_RPATH`/`DT_RUNPATH`.
  - Its rustc unwinds (upstream `base/redox.rs` sets no panic strategy) and
    links through `cc` with `-lgcc`.
  - Source: <https://www.redox-os.org/news/this-month-260131/>
- **Asterinas** reached this phase's exit shape by the other road.
  [asterinas#3749](https://github.com/asterinas/asterinas/pull/3749) (open)
  builds Asterinas inside AsterNixOS with the unmodified Linux rustc and cargo
  from Nix, then boots the result as a nested TCG guest. It uses 32G of RAM, 8
  CPUs and a 64 GB disk, takes about 90 minutes, and needs the network for Nix
  and cargo inputs. Linux binary compatibility is out of scope here (see
  Decided).
- **Motor OS**'s fork ships a statically linked rustc (`rustc_driver` built as
  `["dylib", "rlib"]`) that keeps `panic = abort`.

---

## Phase 2 — Install what you built

**Outcome:** the guest writes a bootable medium and reboots into its own kernel.

Nothing here exists yet.

- **Current state.** `write` on a `/dev` block node returns `ReadOnly`
  (`fs/src/devfs/mod.rs`), and reading one needs `TASK_FLAG_SYSTEM`. There is
  no FAT/vfat support anywhere, so an ESP cannot be written. Partition tables
  are parse-only (`fs/src/partition.rs`). Limine is fetched and installed by
  host scripts, and QEMU boots `order=d` (CD only) with throwaway OVMF vars.
- **Needed:**
  - a writable block path;
  - FAT32 write;
  - a bootloader installer or a direct EFI stub;
  - a `limine.conf` editor;
  - `SYSCALL_REBOOT` (exists) landing on the new image;
  - A/B slots with rollback.
- **Already in place:** a second disk can be mounted at an arbitrary path, so
  the installer has somewhere to read from and write to.
- **Policy change needed:** `AGENTS.md`'s QEMU-only execution boundary
  currently forbids exactly this operation and needs a scoped exception for the
  guest's own ESP.

**Phase 2 exit criteria:** `just boot-persist`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 3 — Bare metal (not committed)

Out of scope for the current goal, which ends at Phase 2 in QEMU. Recorded so
the cost is known:

- **Storage.** No NVMe and no AHCI; virtio-blk is the only storage driver, so
  a real machine has no disk.
- **Input.** No USB at all, so a laptop without PS/2 has no keyboard
  (`plans/usb-xhci.md`).
- **Platform discovery.** PCI is ECAM-only and *panics* without MCFG. x2APIC
  is forcibly disabled, so machines with APIC IDs above 254 do not boot.
- **Network.** No real NIC.
- **Power and thermal.** No ACPI SCI/GPE runtime — no power button, no lid, no
  thermal events during a multi-hour build — and no CPU frequency management.
  EFI runtime services are `ResetSystem` only.
- **Debugging.** COM1 port I/O is the only serial, so the debug channel and
  the KTAP transport vanish exactly when bare-metal debugging starts.

---

## Phase 4 — The toolchain rebuilds itself (not committed)

Out of scope for the goal, which asks that a commit be *authored, compiled and
booted* without a Linux host in the path — not that the compiler was itself
compiled here. It is recorded so nobody widens the goal by accident: it is the
difference between a C++ *runtime* and a C++ *compiler*, and that is where the
order of magnitude lives.

- **The build drivers are C++ too.** LLVM builds with CMake and Ninja, both C++
  programs, and its build also runs Python: a second C++ port plus an
  interpreter, on no other phase's path.
- **clang running in-guest is not the hard part.** It arrives with
  `libLLVM.so` in Phase 1.1. Having the compiler is not having the build
  system, the disk or the hours.
- **Disk and time.** A release LLVM build is tens of gigabytes of objects and
  hours of CPU on a machine with real I/O; both are an order of magnitude past
  Phase 1's, and Phase 3's list is where that I/O would come from.
- **`-Zbuild-std` does not end.** Until `x86_64-unknown-slopos` is tier 2 and
  ships artifacts, every in-guest build builds std.

Phase 1 makes SlopOS a machine that develops SlopOS. This phase would make it
one that develops its own toolchain, and **nobody has done that**. Redox runs a
rustc and LLVM cross-built on Linux: its `HOSTED_REDOX=1` prefix branch fetches
`rust.pkgar` and `llvm21.pkgar` rather than building them, and its LLVM recipes
hand CMake a host toolchain file for tablegen. Asterinas's in-guest build runs
Nix's prebuilt Linux rustc. Neither rebuilds its own compiler.

---

## Decided

**Toolchain**

- **Rust toolchain.** LLVM, cross-built from Linux, with the C++ runtime
  ported. The Rust-hosted road was measured against this kernel and does not
  reach it; the backend and linker gates re-ask on every run.
- **libLLVM is shared.** Upstream's dist and the major distributions link
  rustc, clang and lld against one `libLLVM.so`. That is also one set of pages
  in the file map instead of three.
- **cargo** is a pinned fork with the `network` feature off, dropping curl,
  libgit2 and OpenSSL. SQLite stays, because three libc names were cheaper than
  a build-system port.
- **Crates the compiler needs** get PR-shaped patches, pinned by checksum and
  applied to the rustc tree (Phase 1.1).
- **Host linking** goes through `cc`, i.e. clang with `toolchains::SlopOS`. The
  system's own binaries pin their own link line.
- **Panic strategy.** The target unwinds; the system's own binaries pin abort.

**C and C++ runtime**

- **C++ runtime.** LLVM's libc++ and libc++abi, linked into one `libc++.so`.
- **Library search** follows glibc and musl: `LD_LIBRARY_PATH`, `DT_RPATH`,
  `DT_RUNPATH` and `$ORIGIN`, all ignored under `AT_SECURE`.

**Platform and ABI**

- **Syscall ABI.** Linux x86-64 numbering in one table, with a private range at
  1024; using a Linux number obliges the Linux signature.
- **Linux binary compatibility** is out of scope. Asterinas #3749 is recorded
  as prior art only.
- **Std platform layer.** Unix family over a real libc. The rejected
  alternative was a bespoke platform layer over a crates.io ABI crate — Motor
  OS's shape — which would have cost six hard-breaking third-party crates,
  `libloading` among them.
- **Memory.** A commit ledger, not swap. `MAP_NORESERVE` is honoured, and
  `posix_spawn` runs over the kernel's `spawn`.
- **Directory scaling.** An in-memory name index, not an on-disk htree, so
  `e2fsck` stays the oracle.
- **Identity.** Single-user, uid 0, permanently.

**Dev disk and workflow**

- **The dev disk** is mounted from the kernel command line, stays trailer-less
  and unattested, and `verity=require` keeps meaning the shipped image.
- **Source** is seeded onto the dev disk from git `HEAD` with its vendored
  crates and carried back out as a patch; no host share is on the build path.
- **Kernel build.** One POSIX `sh` driver and one Rust symbol-table tool, run
  the same way on host and guest.
- **Exit identity.** Same behaviour plus a byte-identical loadable image, with
  std's sources at one workspace-relative path on both sides.
- **Scope.** The full in-guest loop, Phases 1–2, in QEMU.
