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
shell scripts/selfhost.sh install           # build the kernel, put it in the spare slot
bootctl reboot                              # try it once
bootctl commit                              # keep it; a reboot without this, or a panic, falls back
```

`just boot` boots this build's kernel from an A/B boot disk it rebuilds every
run, with `/` on a persistent disk and the dev disk (toolchain and a source
tree cut from `HEAD`) at `/devel`. The host owns the boot disk and the
binaries it installs on `/`; the guest owns everything else on both disks.
`just test-selfhost` and `just test-install-guest` run the command above in the
guest and grade it: the guest's dev kernel matches the host's build byte for
byte, and a kernel the guest built boots, commits and rolls back.

The guest builds the dev kernel in about 75 s at four vCPUs under KVM against
49 s for rustup's dist compiler on the same four cores; the gap is the
compiler's build settings, not the kernel.

**Open defect: the tests kernel does not link at the default 4G.** Its link
hits the file map's per-process cap, an eighth of usable memory: `pinnedbytes`
peaks at exactly the cap, `rust-lld` takes a refused file-page fault and dies,
and the dev disk remounts read-only. `test-install-guest` and `test-selfhost`
need `DEV_QEMU_MEM=8G` until the cap is sized for a linker and a refused fault
in one process stops failing a whole mount.

## Phase 1: the toolchain adds a target and nothing else

**Outcome:** every patch under `toolchain/` only teaches its project the
`slopos` target, is shaped as the upstream PR it wants to be, and a program or
library joins SlopOS as a recipe (a pinned tarball, its checksum and a build
command) with no patch at all.

The patches today, by what they do:

| Kind | Patches | Fate |
|---|---|---|
| Teach the target | `compiler/0001,0003,0004`, `rust/0001`, `libc/0001`, `llvm/*`, `llvm-rustc/0001`, `crates/*` but jobserver, `crates/wiring` | stay until upstream takes them |
| Stand in for missing libraries | `cargo/0001` (the `network` cut), `compiler/0002` (exists only to drop that default feature) | remove |
| Work around SlopOS's memory policy | `crates/jobserver` (no `pre_exec`, so a large process never forks) | remove |
| Speed LLVM up | `llvm-rustc/0002,0003` | upstream to LLVM or drop |
| Make the host's and the guest's builds identical | `cargo/0002` (no host triple in `-C metadata`) | upstream to cargo, or decide identity differently |

1. **Recipes.** One place and one driver that fetch a pinned upstream
   tarball, check it, and build it with the SlopOS clang against slibc into a
   prefix the dev disk carries. A recipe that needs a source patch is a
   finding against slibc or the kernel, not a patch to carry.
2. **Cargo's network, unpatched.** zlib, nghttp2, libcurl with a TLS library,
   libssh2 and libgit2 as recipes; cargo built with its default features;
   `cargo/0001` and `compiler/0002` deleted. Decide the TLS library when
   libcurl is ported: OpenSSL is the stock choice and a second TLS stack beside
   `tls-core`; libcurl over rustls keeps TLS in Rust.
3. **POSIX where SlopOS approximates it.** Each deviation is either fixed or a
   port breaks on it:
   - the working directory is per-thread, not per-process;
   - `execve` has no `#!` dispatch and there is no `/bin/sh`, so every script
     runs as `shell script.sh`;
   - futexes are private only, so process-shared mutexes and semaphores are
     refused;
   - `NSIG` is 32: no realtime signals, no `sigqueue`, and `si_pid` only from
     `kill`;
   - no `mkfifo`, and the shell has no `trap`.

   One user, uid 0, stays until a port needs more.
4. **A fork a large process can afford.** A `fork` charges the child's copy
   of every private region to the commit ledger up front, so a gigabyte
   compiler forking owes a second gigabyte, and the jobserver port exists so
   cargo's children never fork. Charge a forked copy as it is written, as
   Linux's default overcommit does, and delete the port. The price is that a
   refusal lands as `SIGBUS` at the write that needed the page, not as
   `ENOMEM` from `fork`.
5. **No speed patches of our own.** Upstream `llvm-rustc/0002,0003` or drop
   them and take the cost in build time.

**Exit:** `toolchain/` holds only target patches, cargo builds with default
features from recipes that carry no patch, and the kernel build still matches
the host's byte for byte.

## Phase 2: git in the guest

**Outcome:** `/devel/src/slopos` is a git clone, and code moves between host
and guest by fetch and push, never by copying the tree.

Today a dev disk is seeded once from `git archive HEAD`, records the commit in
`.slopos-base`, gets its edits out through `just devdisk-export` (debugfs after
the guest shuts down), and picks up host commits only through `just reset
devdisk`, which throws away the guest's build cache. Redox, Asterinas (PR
#3749) and SerenityOS (#12303) all bridge with git over the network instead.

1. **Git as the first recipe.** C git with zlib, built unmodified under
   Phase 1. Its licence, GPL-2.0-only, is no obstacle when the tree holds a
   recipe and no git source; it ships as a separate program with a
   `NOTICE.md` entry.
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

## Phase 3: the whole tree builds in the guest

`build_userland.sh` is bash and the C++ runtime build is CMake and Ninja, so
the guest can rebuild the kernel but not `init`, the shell, the coreutils or
`libc.so`. With Phase 1, bash, CMake and Ninja are recipes like any other.
`selfhost.sh` then gains a verb that installs the userland into `/`, which
forces the decision the loop avoids today: the host refreshes every binary it
built on each `just boot`, so a guest-installed `/bin` must either win or be
declared the guest's.

## Phase 4: bare metal (not committed)

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

## Phase 5: the toolchain rebuilds itself (not committed)

Rebuilding LLVM and rustc in the guest needs Python beside Phase 3's CMake and
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
  instead of into each program that calls it.
- **Toolchain.** LLVM cross-built from Linux into one prefix; the compiler's
  crates ported by PR-shaped patches pinned by checksum. Cranelift and wild
  were measured against this kernel and do not reach it;
  `scripts/gates/{codegen,linker}/` re-ask whenever they install.
- **Linking and panics.** Hosted programs link through `cc` and unwind; the
  system's own binaries pin `rust-lld` and abort. The C++ runtime is libc++.
- **ABI.** Linux x86-64 syscall numbers, no Linux binary compatibility. Unlike
  Asterinas, which runs stock NixOS gcc and git, every tool here is compiled
  for SlopOS; unlike a fork, it is compiled unchanged.
- **Memory.** A commit ledger, not swap.
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
  machines, rebuilt until the embedded table is the kernel's own; identity
  means the same loadable image.
