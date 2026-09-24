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
peak for its largest single compile (`core`). The toolchain that has to run it
is a 716 MB prefix: `librustc_driver` 140 MB, `libLLVM.so` and
`libclang-cpp.so` 79 MB each (X86 only), cargo 31 MB.

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
- **Task ownership I1–I8, and no `async fn` in a kernel crate.**
- **Licensing.** GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source;
  concepts, ABI numbers and layouts are free to take. Anything linked into a
  shipped binary needs a `NOTICE.md` entry; fonts stay runtime-loaded.
- **Ratchets are measurements, not numbers.** Re-measure with the gate's
  `--emit-allowlist` in the same commit and name the change that moved it.
- **`just boot`'s `verity=require` keeps meaning what it says.** Everything this
  plan makes writable is a different medium.

---

## What has landed

Each row is a piece of the foundation and the test or gate that keeps it true.
The mechanisms and their invariants live in the code and in `AGENTS.md`.

| Piece | Standing proof | Where |
|---|---|---|
| A persistent, writable root | `just boot-persist`, `test_ext2_clean_stamp_thaws_before_the_next_write` | `fs/src/ext2/`, `scripts/build_fs_image.sh` |
| A large program runs | `bigprog_test` | `mm/src/{elf,demand,process_vm}.rs`, `core/src/exec/` |
| A build system's floor | `buildctl_test`, `scripts/check_syscall_abi.sh` | `abi/src/syscall/`, `core/src/syscall/`, `fs/src/fileio/flock.rs` |
| Storage holds a tree | `just test-capacity`, `scripts/check_fs_throughput.sh` | `fs/src/ext2/{dirindex,journal}.rs`, `drivers/src/virtio_blk.rs` |
| Utilities, a POSIX shell, a terminal and an editor | `coreutils_test`, `shell_script_test`, `terminal_grid_test`, `editor_test` | `userland/src/apps/`, `shell-core/`, `terminal-core/`, `editor-core/` |
| The target is a hosted, built-in target | `scripts/check_rustc_target.sh`, `libc_abi_test` | `targets/x86_64-unknown-slopos.json`, `toolchain/{compiler,rust,libc}/` |
| The libc surface LLVM 23.1 and rustc's crates need | `libc_probe`, `slibc-core` host tests | `slibc/`, `userland/libctest/` |
| C++ runtime and LLVM ports | `cxx_test`, `scripts/check_{cxx_pin,llvm_port,clang_driver}.sh` | `toolchain/{cxx,llvm,llvm-rustc}/`, `scripts/make_slopos_cxx.sh` |
| The loader searches like glibc and musl | `dl_test` | `slibc/src/ld_so/`, `core/src/exec/` |
| The toolchain is cross-built into one prefix | `scripts/check_bootstrap_config.sh`, `scripts/check_cargo_fork.sh`, `scripts/check_toolchain_pin.sh` | `scripts/bootstrap_slopos_toolchain.sh`, `toolchain/crates/` |
| The dev disk carries it and mounts at boot | `just test-devdisk` (inventory, source, remount), `mount_test` | `scripts/build_devdisk.sh`, `boot/src/early_init.rs`, `fs/src/vfs/init.rs` |
| The kernel build needs no host tool | `just build`; `slopos-kallsyms` tests; two checkouts build identical ELFs | `scripts/build_kernel.sh`, `tools/kallsyms/`, `scripts/compare_kernel_elf.sh` |
| The build loop holds | `buildloop_test`, `exit_stress_test` | `mm/src/{commit,vma_region}.rs`, `slibc/src/process/spawn.rs` |
| Code gets in and out | `scripts/check_offline_build.sh`, `transfer_test` | `.cargo/vendor.toml`, `scripts/{make_vendor,export_devdisk}.sh`, `tls-core/` |

### What Phase 1.1 builds on

- **The toolchain.** `just toolchain` builds rustc, cargo, LLVM, clang and lld
  for `x86_64-unknown-slopos` into `builddir/slopos-toolchain/install`, which is
  also the C sysroot: clang's config names `<CFGDIR>/..`, and `cc`, `c++` and
  `ld.lld` are links in `bin/`. Every binary carries `RUNPATH $ORIGIN/../lib`,
  links `libc++` rather than the host's `libstdc++`, and passes the install
  grader: no host library, no `TLSDESC`, every name bound. Built incrementally
  so far; never yet from a clean tree.
- **The target.** Links through `cc`, whose `toolchains::SlopOS` supplies
  `crt0.o`, the interpreter, `-lc` and compiler-rt, and unwinds. The system's own
  binaries pin `rust-lld` and `panic=abort`.
- **The loader.** `LD_LIBRARY_PATH`, `DT_RPATH`, `DT_RUNPATH` and `$ORIGIN`, all
  refused under `AT_SECURE`; `AT_EXECFN` is the canonical executable path, so
  `dladdr` on the executable names it (clang's `getMainExecutable`).
  `LD_DEBUG=statistics` prints the relocations bound. Binding is eager.
- **The dev disk.** 4 GiB, labelled `slopos-dev`, mounted at `/devel` by
  `mount=LABEL=slopos-dev:/devel`. A new volume carries the toolchain at
  `src/slopos/third_party/rust-slopos` — where the host keeps its owned
  sysroot, so std's crates hash alike on both machines — and the source cut from
  `HEAD`. `just boot-dev` boots it with 4G of RAM.
- **The kernel build.** `scripts/build_kernel.sh` is POSIX sh for `/bin/shell`
  and the coreutils; the ELF gates run from the justfile. Host builds use
  `cargo +slopos` with `trim-paths`. The cross-built rustc reports the host's
  version string, `1.100.0-nightly (2e2b193f8 2026-09-02)`, on which crate
  hashes depend.

### Deliberate gaps carried forward

- **Memory.** No swap and no OOM victim; user pages are 4 KiB only.
- **ABI.** Linux's at the numbers and most layouts, except `getdents64`'s
  `d_name` at 24, a truncated `ucontext_t`, `NSIG` 32, a `termios2`-shaped
  `struct termios` and a 16-byte `signalfd_siginfo`.
- **Processes.** The cwd is per-thread. Futexes are private-only, so a
  process-shared semaphore is refused. `si_pid` comes from `kill` only — there
  is no `tgkill` or `sigqueue`.
- **Files.** `posix_fallocate` and `posix_fadvise` are libc-side; no htree;
  mutations serialise per mount; single user, uid 0.
- **Missing.** `mkfifo`, `vfork`, `trap`, `git` in the guest, network for cargo,
  9p or virtio-fs, `current_exe` (no procfs).

---

## Phase 1 — The toolchain

**Outcome:** `cargo build` runs on SlopOS and produces `kernel.elf`.

The toolchain is **LLVM, cross-built from Linux, with the C++ runtime ported**;
the cranelift road was measured and does not reach this kernel, and the backend
and linker gates re-ask on every run. Everything but running it in the guest
has landed. **Phase 1 closes when Phase 1.1's exit criteria hold.**

---

## Phase 1.1 — The toolchain runs, and builds the kernel

**Where it stands.** Under KVM the ladder passes and the guest builds both
kernels from the dev disk: the dev kernel in about 600 s and the tests kernel
in about 600 s, at four vCPUs against 20 s + 9 s for the dev kernel's two
passes on the host at `-j4`. The guest image then fails `e2fsck -fn` (open
item 1), which is where the run stops.

**Exit criteria** (these close Phase 1):

1. `just toolchain` completes from a clean tree, and `just test-devdisk` finds
   rustc, cargo, rust-lld, clang and `ld.lld` on the volume.
2. `just test-selfhost`: the guest builds the dev and tests kernels with
   `scripts/build_kernel.sh`, using nothing a Linux host provides.
3. The guest-built tests kernel passes the kernel suite, and
   `check_kernel_elf_gates.sh` passes on both.
4. The guest-built dev kernel's loadable image and symbol table are
   byte-identical to a host build of the same commit.

### Done so far

- The GPF in `UserContext::rax` was the trap stack overwriting the round
  trip's frames. `test_deep_user_trap_spares_the_round_trip` covers it; that it
  fails without the fix is not yet shown.
- The ladder: `rustc --version` 1.48 s, 206 012 relocations bound in 5
  objects; `rustc hello.rs` 6.7 s; the cargo rung 21 s. Rung 1 also runs
  `ld.lld` and `rust-lld`, and holds the guest `rustc --version` to the host's
  — bootstrap appended "(built from a source tarball)", and the version string
  is hashed into every `StableCrateId`.
- `stacker`: the pinned nightly no longer uses it; the compiler runs on a
  17 MiB thread, and `libc_abi_test` proves a requested stack reaches the
  thread.
- The file map at 4G: 256 sets per process refused the archive step of a
  256-codegen-unit crate, and a sixteenth of memory refused the kernel link's
  pages. The registry is 4096 sets, 1024 per principal, and a principal may pin
  an eighth of usable memory.
- ext2: a block logged by a small write and then written home by a large one
  kept its log mapping, so a miss, a check point and a replay all brought the
  older copy back (every rlib member began with zeroes). A home write now
  supersedes the log's copies with a `REVOKE`.
- A killed task's abandoned block request read as device damage and latched
  `/devel` read-only; it is now `Interrupted`.
- `utest_selfhost` streams klog live; both clang driver ports take rustc's
  `-no-pie`.

### Open, in order

1. **Link counts after a guest build.** `e2fsck -fn` finds 16–20 `.rcgu.o`
   objects with three names and a link count of two: the name rustc unlinked
   from a crate's `out/` after archiving is back on disk in the directory block
   as it was before that unlink, while the link count kept the decrement.
   Instrumented runs ruled out a home write ahead of the newest log copy, a
   rollback dropping committed content, and hash-chain order in the log index;
   a four-process link/unlink churn on the tests image does not reproduce it.
   Left to test: a killed requester's quarantined write landing after a newer
   write of the same block, and two cache slots for one block. Until it is
   fixed `export_devdisk.sh` refuses the image, which blocks criteria 3 and 4.
2. Criterion 4, once the image exports: the recipe's host reference build
   succeeds; the comparison has not run.
3. Criterion 3.
4. Criterion 1 from a clean tree: the `-no-pie` claim changed the LLVM port,
   so the toolchain has to be rebuilt anyway, and the dev disk restaged.
5. The global `malloc` lock (`slibc/src/mem/dlmalloc.rs:77`) only spins; the
   guest build is about 22× the host's at the same `-j`. Measure its share.
6. One full-speed run failed `exec` and an archive `write` with `EFAULT`; not
   seen again in three runs.

`just test-selfhost` needs a clean working tree and a dev disk seeded from
`HEAD`. A difference between the host rustc and the cross-built one is a
finding to explain, not a tolerance to set.

---

## Phase 2 — Install what you built

**Outcome:** the guest writes a bootable medium and reboots into its own kernel.

- **Current state.** `write` on a `/dev` block node returns `ReadOnly`
  (`fs/src/devfs/mod.rs`), and reading one needs `TASK_FLAG_SYSTEM`. There is no
  FAT support, so an ESP cannot be written; partition tables are parse-only.
  Limine is installed by host scripts, and QEMU boots `order=d` with throwaway
  OVMF vars.
- **Needed:** a writable block path, FAT32 write, a bootloader installer or an
  EFI stub, a `limine.conf` editor, `SYSCALL_REBOOT` landing on the new image,
  and A/B slots with rollback.
- **Policy change needed:** `AGENTS.md`'s QEMU-only execution boundary forbids
  exactly this and needs a scoped exception for the guest's own ESP.

**Phase 2 exit criteria:** `just boot-dev`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 3 — Bare metal (not committed)

Recorded so the cost is known: no NVMe, AHCI or USB; PCI panics without MCFG;
x2APIC is disabled; no real NIC; no ACPI SCI/GPE runtime or frequency
management; COM1 is the only serial, so the KTAP transport vanishes on real
hardware.

---

## Phase 4 — The toolchain rebuilds itself (not committed)

The goal asks that a commit be *compiled* here, not the compiler. Rebuilding
LLVM in-guest needs CMake, Ninja and Python ported, tens of gigabytes and hours
of CPU on real I/O, and `-Zbuild-std` on every build until the target is tier 2.
Neither Redox nor Asterinas rebuilds its own compiler.

---

## Decided

- **Toolchain.** LLVM cross-built from Linux, one shared `libLLVM.so`; cargo a
  pinned fork with `network` off; the compiler's crates ported by PR-shaped
  patches pinned by checksum.
- **Linking and panics.** Hosted programs link through `cc` and unwind, as on
  Redox; the system's own binaries pin `rust-lld` and abort. `rustc_llvm` picks
  `libc++` for `slopos`, as it does for FreeBSD, rather than `llvm.use-libcxx`,
  which would force it on the Linux stage1 compiler too.
- **C++ runtime.** libc++ and libc++abi in one `libc++.so`.
- **Library search** follows glibc and musl, all of it ignored under
  `AT_SECURE`.
- **Syscall ABI.** Linux x86-64 numbering, private range at 1024. Linux binary
  compatibility is out of scope.
- **Std platform layer.** Unix family over a real libc.
- **Memory.** A commit ledger, not swap.
- **Dev disk.** Mounted from the command line, trailer-less, seeded from `HEAD`
  and carried back out as a patch; no host share on the build path.
- **Kernel build.** One POSIX `sh` driver and one Rust symbol-table tool, on
  both machines; identity means the same loadable image with std at one
  workspace-relative path on both sides.
- **Scope.** The full in-guest loop, Phases 1–2, in QEMU.
