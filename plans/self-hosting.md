# SlopOS As A Development Machine — Task Plan

## Goal

Turn SlopOS from an appliance that demonstrates subsystems into a machine you
can *develop SlopOS on*: boot it (QEMU first, bare metal later), edit its
sources, build the kernel and userland with a native Rust toolchain, install the
result, and reboot into it. The loop closes when a commit to this repository is
authored, compiled and booted without a Linux host in the path.

One word decides the scope: the compiler must *run* here, not be *built* here.
A C++ runtime on SlopOS lets a cross-built LLVM run; a C++ compiler rebuilding
LLVM here is Phase 3, an order of magnitude further out and not committed.

**Scale, measured.** The dev kernel is 49 rustc invocations and 6 build
scripts, a 410 MB target directory, 42 s at `-j20` on the host, and a 1.2 GiB
peak for its largest single compile (`core`). The toolchain that runs it is a
717 MB prefix — `librustc_driver` 140 MB, `libLLVM.so` and `libclang-cpp.so`
79 MB each (X86 only), cargo 31 MB — built from a clean tree in 3 h 30 min at
`-j4`. The same dev kernel builds on the host in 45 s at `-j4`; in the guest,
at four vCPUs, its two passes take about 600 s under KVM and 136 + 90 min
under TCG.

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
| Interleaved writeback never reverts a block | `test_ext2_lagging_pass_never_puts_an_older_copy_home`, `test_ext2_aborted_home_write_does_not_outlive_the_log`, `just check-fs-image` after every capture | `fs/src/ext2/{cache,journal}.rs`, `Ext2Fs::sync_step` |
| A kill never reorders the disk | `test_virtio_blk_killed_write_is_waited_out`, `test_virtio_blk_waits_out_an_abandoned_write`, `test_uninterruptible_wait_outlasts_a_kill` | `drivers/src/virtio_blk.rs`, `slopos-ostd/src/sync/wait_queue.rs` |
| An error is never an absent name | `test_ext2_create_and_link_fail_when_the_lookup_does`, `test_rename_fails_when_a_lookup_does`, `test_removal_fails_when_a_lookup_does`, `test_create_open_keeps_the_mode_of_a_file_it_found`, `test_ext2_directory_hole_is_damage_not_absence`, `test_ext2_orphan_drain_keeps_a_list_it_could_not_read` | `fs/src/{ext2,vfs,fileio}/` |
| Utilities, a POSIX shell, a terminal and an editor | `coreutils_test`, `shell_script_test`, `terminal_grid_test`, `editor_test` | `userland/src/apps/`, `shell-core/`, `terminal-core/`, `editor-core/` |
| The target is a hosted, built-in target | `scripts/check_rustc_target.sh`, `libc_abi_test` | `targets/x86_64-unknown-slopos.json`, `toolchain/{compiler,rust,libc}/` |
| The libc surface LLVM 23.1 and rustc's crates need | `libc_probe`, `heap_allocator_test`, `slibc-core` host tests | `slibc/`, `userland/libctest/` |
| A forked child inherits no libc lock held by a thread it lacks | `heap_allocator_test`'s `fork_while_another_thread_{allocates,holds_the_loader,starts_threads,flushes_streams}` | `slibc/src/pal/slopos.rs` |
| C++ runtime and LLVM ports | `cxx_test`, `scripts/check_{cxx_pin,llvm_port,clang_driver}.sh` | `toolchain/{cxx,llvm,llvm-rustc}/`, `scripts/make_slopos_cxx.sh` |
| The loader searches like glibc and musl | `dl_test` | `slibc/src/ld_so/`, `core/src/exec/` |
| The toolchain is cross-built into one prefix | `scripts/check_bootstrap_config.sh`, `scripts/check_cargo_fork.sh`, `scripts/check_toolchain_pin.sh` | `scripts/bootstrap_slopos_toolchain.sh`, `toolchain/crates/` |
| The dev disk carries it, mounts at boot, and runs it | `just test-devdisk` (inventory, source, remount, the toolchain ladder), `mount_test` | `scripts/build_devdisk.sh`, `boot/src/early_init.rs`, `fs/src/vfs/init.rs` |
| The kernel build needs no host tool | `just build`; `slopos-kallsyms` tests; two checkouts build identical ELFs | `scripts/build_kernel.sh`, `tools/kallsyms/`, `scripts/compare_kernel_elf.sh` |
| The build loop holds | `buildloop_test`, `exit_stress_test`, `test_blocking_populate_outlasts_a_long_reader`, `test_user_copy_retries_a_copy_that_faulted_midway`; the guest builds both kernels | `mm/src/{commit,vma_region,page_fault,user_copy,user_mappings}.rs`, `slibc/src/process/spawn.rs` |
| A mount has one writeback pass | `test_ext2_sync_finishes_the_open_pass_instead_of_opening_one`, `test_ext2_journal_headroom_is_restored_off_the_mount_lock` | `fs/src/ext2_vfs.rs` |
| Code gets in and out | `scripts/check_offline_build.sh`, `transfer_test` | `.cargo/vendor.toml`, `scripts/{make_vendor,export_devdisk}.sh`, `tls-core/` |

### What Phase 1 builds on

- **The toolchain.** `just toolchain` builds rustc, cargo, LLVM, clang and lld
  for `x86_64-unknown-slopos` into `builddir/slopos-toolchain/install`, which is
  also the C sysroot: clang's config names `<CFGDIR>/..`, and `cc`, `c++` and
  `ld.lld` are links in `bin/`. Every binary carries `RUNPATH $ORIGIN/../lib`,
  links `libc++` rather than the host's `libstdc++`, and passes the install
  grader: no host library, no `TLSDESC`, every name bound. The prefix's
  `libc.so` is a link input only: `libc.so` is the interpreter, so a program
  runs on the `/lib` copy whatever its search path finds.
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
  version string, `1.100.0-nightly (2e2b193f8 2026-09-02)`, which is hashed into
  every `StableCrateId`; `devdisk_test` holds the two to one string.
- **The guest's build.** `selfhost_test` runs `scripts/build_kernel.sh` under
  `/bin/shell` over `/devel/src/slopos`, with nothing on `PATH` but the staged
  toolchain and the coreutils, and streams cargo's output to the console.
  `just test-selfhost` then holds the volume to `e2fsck -fn`, grades both
  kernels, runs the kernel suite on the guest's tests kernel and compares the
  guest's dev kernel with the host's build of the same commit. Under TCG the
  run takes about eight hours, so `SELFHOST_TIMEOUT_SECS` raises its budget.

### Deliberate gaps carried forward

- **Memory.** No swap and no OOM victim; user pages are 4 KiB only. A shared
  file's page set is charged to whichever process reserved or mapped it last,
  so one process's fault can be refused for another's share.
- **ABI.** Linux's at the numbers and most layouts, except `getdents64`'s
  `d_name` at 24, a truncated `ucontext_t`, `NSIG` 32, a `termios2`-shaped
  `struct termios` and a 16-byte `signalfd_siginfo`.
- **Processes.** The cwd is per-thread. Futexes are private-only, so a
  process-shared semaphore is refused. `si_pid` comes from `kill` only — there
  is no `tgkill` or `sigqueue`. `fork` holds libc's process-wide locks, so a
  `fork` from a signal handler that interrupted a holder deadlocks, as under
  glibc; a stdio stream another thread holds at the `fork` stays held in the
  child.
- **Files.** `posix_fallocate` and `posix_fadvise` are libc-side; no htree;
  mutations serialise per mount; single user, uid 0.
- **Entropy.** The kernel's CSPRNG is seeded from RDRAND and RDSEED; a CPU
  without RDRAND seeds it from four TSC reads, which an observer of a TLS
  ClientHello's random can search.
- **Missing.** `mkfifo`, `vfork`, `trap`, `git` in the guest, network for cargo,
  9p or virtio-fs, `current_exe` (no procfs).

---

## Closing the loop

**Outcome:** one `just test-selfhost` run passes every step on one tree.

**Where it stands.** The guest builds both kernels under KVM at four vCPUs:
the dev kernel in 714 s (cargo passes 6 min 47 s and 4 min 34 s) and the tests
kernel in 756 s. The dev disk then fails `e2fsck -fn`, which is where the run
stops. An earlier run matched the host's dev kernel in both its loadable image
and its symbol table; no run has yet passed all five steps.

1. `just test-selfhost` green end to end:
   - the guest builds both kernels
   - the dev disk passes `e2fsck -fn`
   - `check_kernel_elf_gates.sh` passes on both kernels
   - the kernel suite passes on the guest's tests kernel
   - the dev kernel is identical to the host's build

   The blocker is 16 inodes with three names and a link count of two. Each
   extra name is an `out/*.rcgu.o` that rustc's first pass unlinked after
   archiving: the object's two incremental-session names are right, and the
   `out/` entry is back. Its neighbours in the same directory block stayed
   deleted, and the second pass's later changes to that block persisted. So
   one unlink's entry removal was lost while its link-count decrement was not;
   the whole block was not reverted. Copying only a block's newest record at
   the check point did not fix it. Next: reproduce it without the guest — an
   `Ext2Fs`-level loop of create, link into a session directory, unlink in
   random order, with passes interleaved and clean entries dropped, checked
   against the medium after each full sync.
2. The C allocator's lock: measure what the futex lock bought. Time the dev
   kernel's first pass in the guest with a `libc.so` built on the old spinning
   lock, installed as `/lib/libc.so` on the tests image, against the futex
   lock, from one fresh dev disk each, on one tree. Not yet taken: the only
   spinning-lock timing (5 min 55 s first pass) is from an older tree and an
   older kernel.
3. The slowdown. The guest build is about 22× the host's at the same `-j`,
   and the guest is mostly waiting, not computing: over the last full run the
   four CPUs were idle 27, 54, 73 and 83% of their ticks. Two causes are fixed:
   a page fault used to bounce back to user mode whenever a sibling thread was
   mid-copy, so multi-threaded rustc re-faulted in a loop and a copy's
   populate eventually answered `EFAULT`; and `schedule_internal` read the CPU
   id before masking interrupts, so a task preempted in between could dispatch
   off another CPU's run queue. What is left: compare `cargo --timings`
   per unit, guest against host, to find whether the rest is uniform or
   concentrated, and account for the idle time — the remaining candidates are
   the one global C allocator lock every LLVM thread shares, the per-mount lock
   serialising every filesystem operation, and wakeups stranded until the
   100 ms rescue sweep.

---

## Phase 1 — Install what you built

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

**Phase 1 exit criteria:** `just boot-dev`, build a kernel in-guest, install
it, reboot, and the boot log shows the new build — with rollback if it panics.

---

## Phase 2 — Bare metal (not committed)

Recorded so the cost is known: no NVMe, AHCI or USB; PCI panics without MCFG;
x2APIC is disabled; no real NIC; no ACPI SCI/GPE runtime or frequency
management; COM1 is the only serial, so the KTAP transport vanishes on real
hardware.

---

## Phase 3 — The toolchain rebuilds itself (not committed)

The goal asks that a commit be *compiled* here, not the compiler. Rebuilding
LLVM in-guest needs CMake, Ninja and Python ported, tens of gigabytes and hours
of CPU on real I/O, and `-Zbuild-std` on every build until the target is tier 2.
Neither Redox nor Asterinas rebuilds its own compiler.

---

## Decided

- **Toolchain.** LLVM cross-built from Linux, one shared `libLLVM.so`; cargo a
  pinned fork with `network` off; the compiler's crates ported by PR-shaped
  patches pinned by checksum. A Rust-hosted toolchain — cranelift and wild —
  was decided first, then measured against this kernel and found not to reach
  it; `scripts/gates/{codegen,linker}/` hold that answer, and CI re-asks it
  whenever the candidates install.
- **Linking and panics.** Hosted programs link through `cc` and unwind, as on
  Redox; the system's own binaries pin `rust-lld` and abort. `rustc_llvm` picks
  `libc++` for `slopos`, as it does for FreeBSD, rather than `llvm.use-libcxx`,
  which would force it on the Linux stage1 compiler too.
- **C++ runtime.** LLVM's libc++ and libc++abi in one `libc++.so`, settled by
  cross-building it: `libstdc++` comes out of a GCC cross-compiler's
  bootstrap, a second toolchain to pin and keep, where libc++ is built by the
  clang that has to be here anyway. Localization, wide characters,
  `<filesystem>` and the random device are on because LLVM reaches all four;
  with them on, porting LLVM 18.1.8 cost the C library POSIX-2008's locale
  objects and 53 `_l` functions, wide stdio, and four headers.
- **Library search** follows glibc and musl, all of it ignored under
  `AT_SECURE`.
- **Syscall ABI.** Linux x86-64 numbering, private range at 1024. Linux binary
  compatibility is out of scope.
- **Std platform layer.** Unix family over a real libc.
- **Memory.** A commit ledger, not swap.
- **Block requests.** A request the device holds is waited out, not
  abandoned, because nothing orders two requests in flight to one sector:
  Linux never abandons a submitted bio, whose pages stay with it until it
  completes, and Asterinas and Redox wait on the completion too. The wait is
  bounded, and a write given up on a timeout fences every later write, and a
  read-modify-write's read, until the device returns it.
- **The C allocator's lock** is the futex mutex `pthread_mutex_t` uses: a
  contended `malloc` sleeps, as under glibc and musl, after a short spin, as
  under musl.
- **`fork` and libc's locks.** Held across the `fork`, outermost first — loader,
  TLS layout, `atexit` list, stream list, allocator — as musl does, so both
  sides release them. glibc re-initialises its loader locks in the child
  instead, which leaves the table a concurrent `dlopen` was editing half-done.
- **Writeback.** One pass per mount, driven by every caller that needs one. A
  `sync` that finds a pass open drives it to the end and then the next, as a
  jbd2 commit waiter does, because the open pass's epoch predates the caller's
  writes.
- **Page faults against user copies.** A fault waits, holding the per-process
  lock, for the copies in flight to drop their reference to the address space;
  copies run with preemption off and none can start while that lock is held.
  Linux faults under a shared `mmap_lock` with page-table locks, and Asterinas
  locks page-table nodes; this is the coarse form of the same exclusion.
- **Dev disk.** Mounted from the command line, trailer-less, seeded from `HEAD`
  and carried back out as a patch; no host share on the build path.
- **Kernel build.** One POSIX `sh` driver and one Rust symbol-table tool, on
  both machines; identity means the same loadable image with std at one
  workspace-relative path on both sides.
- **Scope.** The full in-guest loop, through Phase 1, in QEMU.
