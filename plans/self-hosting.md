# SlopOS As A Development Machine

## Goal

Develop SlopOS on SlopOS: edit its sources, build them with the native
toolchain, install the result and reboot into it. QEMU first, bare metal after.

Every third-party tool in that loop is an upstream project compiled for
SlopOS unmodified. A patch teaches a project the `slopos` target and nothing
else; anything more a port needs is a gap in SlopOS, fixed in SlopOS with
POSIX semantics.

## Where it stands

The loop is closed:

```sh
just boot                                   # host: the development machine (just boot-fast: no wheel)
cd /devel/src/slopos                        # guest
scripts/selfhost.sh install                 # build the kernel, put it in the spare slot
bootctl reboot                              # try it once
bootctl commit                              # keep it; a reboot without this, or a panic, falls back
```

`just boot` boots this build's kernel from an A/B boot disk it rebuilds every
run, with `/` on a persistent disk and the dev disk (toolchain and a source
tree cut from `HEAD`) at `/devel`. The host owns the boot disk and the
binaries it installs on `/`; the guest owns everything else on both disks.
`just test-selfhost` and `just test-install-guest` run the command above in the
guest and grade it: the guest's kernels pass the ELF gates and the kernel
suite, and a kernel the guest built boots, commits and rolls back.

Every patch under `toolchain/` teaches its project the `slopos` target and
nothing else. A library joins as a recipe under `toolchain/recipes/`: a
pinned tarball, its checksum and a build template that
`scripts/build_recipes.sh` compiles against slibc, and that
`scripts/check_recipes.sh` holds to leaving the source as shipped. Cargo builds
with its default features against the zlib, nghttp2, OpenSSL, libcurl, libssh2
and libgit2 recipes; in the guest it resolves a git dependency through libgit2
and fetches a crate over HTTPS through libcurl and OpenSSL.

A port finds the POSIX it expects: one working directory per process, `#!`
scripts and `/bin/sh`, process-shared futexes, 64 signals with queued realtime
ones and `sigqueue`, FIFOs, and `trap` in the shell. A fork owes its copy as
the child writes it, and a write that finds no page makes the OOM killer take
the largest resident process instead of faulting the writer.

The guest builds the dev kernel in about 75 s at four vCPUs under KVM against
49 s for rustup's dist compiler on the same four cores; the gap is the
compiler's build settings, not the kernel.

**Open defect: the tests kernel does not link at the default 4G.** Its link
hits the file map's per-process cap, an eighth of usable memory: `pinnedbytes`
peaks at exactly the cap, `rust-lld` takes a refused file-page fault and dies,
and the dev disk remounts read-only. `test-install-guest` and `test-selfhost`
need `DEV_QEMU_MEM=8G` until the cap is sized for a linker and a refused fault
in one process stops failing a whole mount.

## Phase 1: git in the guest

**Outcome:** `/devel/src/slopos` is a git clone, and code moves between host
and guest by fetch and push, never by copying the tree.

Today a dev disk is seeded once from `git archive HEAD`, records the commit in
`.slopos-base`, gets its edits out through `just devdisk-export` (debugfs after
the guest shuts down), and picks up host commits only through `just reset
devdisk`, which throws away the guest's build cache. Redox, Asterinas (PR
#3749) and SerenityOS (#12303) all bridge with git over the network instead.

1. **Git as a recipe.** C git over the zlib recipe, built unmodified. Its
   licence, GPL-2.0-only, is no obstacle when the tree holds a recipe and no
   git source; it ships as a separate program with a `NOTICE.md` entry.
2. **The host serves its checkout.** `just boot` runs `git daemon` on the
   host so the guest reaches it through SLIRP (`git://10.0.2.2/`), which needs
   neither TLS nor libcurl; pushes land in a host-side bare repository.
3. **The dev disk holds a clone.** A new dev disk clones instead of copying;
   the offline vendor configuration moves into the guest's `CARGO_HOME`, so
   `.cargo/config.toml` stays as committed and `git status` stays clean.
4. **Retire the copy.** Delete `.slopos-base`, the `git archive` seeding,
   `export_devdisk.sh`'s patch mode, `just devdisk-export`, the edit export in
   `just reset devdisk` and the `HEAD` note `just boot` prints.
   `test-selfhost` has the guest fetch the commit under test instead of
   requiring a disk seeded from it.

**Exit:** the guest pulls a host commit, builds and boots it, commits a change,
and the host fetches that commit, with no debugfs and no reseed on the way.

## Phase 2: the whole tree builds in the guest

`build_userland.sh` is bash and the C++ runtime build is CMake and Ninja, so
the guest can rebuild the kernel but not `init`, the shell, the coreutils or
`libc.so`. Bash, CMake and Ninja join as recipes like any other, and
`selfhost.sh` gains a verb that installs the userland into `/`, which
forces the decision the loop avoids today: the host refreshes every binary it
built on each `just boot`, so a guest-installed `/bin` must either win or be
declared the guest's.

## Phase 3: bare metal (not committed)

`just iso` builds the bare-metal artifact: kernel and initramfs, running from
RAM, keeping nothing. You can *try* SlopOS on hardware; you can *develop* there
once it keeps what you write:

1. **Storage.** NVMe first, then AHCI. QEMU emulates both, so write and test
   the driver in QEMU and let `just boot` attach its disks through it.
2. **An installer.** From the live ISO: partition a disk GPT (ESP, `/`,
   `/devel`), copy the running root, write the ESP with `fat-core` as `bootctl`
   does. That is Linux 0.12's route and Redox's installer's.
3. **The rest of a real machine.** PCI without MCFG, x2APIC, a real NIC,
   USB HID input (`plans/usb-xhci.md`), ACPI SCI/GPE, and a log sink other than
   COM1.

The verified image (`fs/assets/ext2.img`, `verity=require`) backs no boot; the
suite mounts it to exercise verity. Decide whether it becomes the bare-metal
read-only root or goes.

## Phase 4: the toolchain rebuilds itself (not committed)

Rebuilding LLVM and rustc in the guest needs Python beside Phase 2's CMake and
Ninja, tens of gigabytes and hours of CPU, and `-Zbuild-std` until the target is
tier 2. Neither Redox nor Asterinas rebuilds its own compiler.

## Constraints

- Only `slopos-ostd` uses `unsafe`; `check_unsafe_expansion.sh` sees through
  macros. Nothing here earns an exemption.
- `KBox`/`KVec`/`KArc`/`KBTreeMap` only. A toolchain-sized buffer becomes a
  chunked or page-list design; `MAX_ALLOC_SIZE` stays 1 MiB.
- Stack frames stay under 2 KiB, against a 4 KiB guard page.
- Task ownership I1 to I8; no `async fn` in a kernel crate.
- GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source in this tree; a
  recipe names a tarball and carries none of it. A third-party program shipped
  on an image needs a `NOTICE.md` entry.
- Ratchets are measurements: re-measure with the gate's `--emit-allowlist` and
  name the change that moved it.
- The verified image stays read-only and attested. Anything writable is a
  different medium.

## Decided

- **Ports compile unmodified.** A patch teaches a project the target; a
  missing function goes into slibc or the kernel once, with POSIX semantics,
  instead of into each program that calls it. A library or tool is a recipe
  (tarball, checksum, template): the shape of Redox's cookbook without its
  patch list.
- **Toolchain.** LLVM cross-built from Linux into one prefix; the compiler's
  crates ported by PR-shaped patches pinned by checksum. Cranelift and wild
  were measured against this kernel and do not reach it;
  `scripts/gates/{codegen,linker}/` re-ask whenever they install.
- **Linking and panics.** Hosted programs link through `cc` and unwind; the
  system's own binaries pin `rust-lld` and abort. The C++ runtime is libc++.
- **ABI.** Linux x86-64 syscall numbers, no Linux binary compatibility. Unlike
  Asterinas, which runs stock NixOS gcc and git, every tool here is compiled
  for SlopOS; unlike a fork, it is compiled unchanged.
- **Memory.** A commit ledger, not swap; a forked copy is charged as written,
  with an OOM killer behind it.
- **The dev loop.** One development machine (`just boot`, and `just
  boot-fast` to skip the wheel) and one live artifact (`just iso`); knobs
  (`KERNEL_RELEASE`, `VIDEO`, `ports`, `DEBUG`, `ROULETTE`) rather than more
  recipe variants, as Asterinas, Redox and SerenityOS all do. Persistent disks
  refresh in place and only `just reset` deletes one.
- **Install.** A/B slots, a one-shot try and a commit are the one install
  path: the shape of `grub-reboot` and systemd-boot's boot assessment. The host
  rebuilds the boot disk on every `just boot`, so a stale kernel never boots by
  accident.
- **Tests drive the human entry point.** The guest-side tests call
  `scripts/selfhost.sh`, so the loop you run is the loop `just test-selfhost`
  and `just test-install-guest` grade.
- **Source bridge.** Git over the network, not a shared filesystem: no 9p or
  virtio-fs on the build path.
- **Kernel build.** One POSIX sh driver and one Rust symbol-table tool on both
  machines, rebuilt until the embedded table is the kernel's own. The guest's
  kernel is graded by the gates and the suite, not by identity with the
  host's.
- **No patches of our own for speed or identity.** A toolchain patch that only
  makes something faster, or makes two machines build the same bytes, is not
  carried.
