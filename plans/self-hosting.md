# SlopOS As A Development Machine

## Goal

Develop SlopOS on SlopOS: edit its sources, build them with the native
toolchain, install the result and reboot into it. QEMU first, bare metal after.
The loop closes when a commit to this repository is authored, compiled and
booted with no Linux host in the path.

The compiler must *run* here. Rebuilding LLVM here is Phase 3 and not
committed.

## The loop today

```sh
just boot                                   # host: the development machine
cd /devel/src/slopos                        # guest
shell scripts/selfhost.sh install           # build the kernel, put it in the spare slot
bootctl reboot                              # try it once
bootctl commit                              # keep it; a reboot without this, or a panic, falls back
```

`just boot` boots this build's kernel from an A/B boot disk, with `/` on a
persistent disk and the dev disk (toolchain and a source tree cut from `HEAD`)
at `/devel`. The host owns the boot disk and the binaries it installs on `/`;
the guest owns everything else on both disks. `just reset root|devdisk`
discards one. `just boot-live` boots the live ISO the way bare metal runs it:
from RAM, with no disk.

`selfhost_test` and `install_test` run `scripts/selfhost.sh` the way you type
it, so `just test-selfhost` and `just test-install-guest` grade the command
above: the guest's dev kernel matches the host's build byte for byte, and a
kernel the guest built boots from slot b, commits and rolls back.

**Broken at the default 4G.** Linking the *tests* kernel in the guest hits the
file map's per-process cap, an eighth of usable memory (125306 pages at 4G):
`pinnedbytes` peaks at exactly the cap, `rust-lld` takes a refused file-page
fault and dies of SIGSEGV, and the dev disk remounts read-only with bitmap
blocks leaked. `test-install-guest` fails that way at 4G and passes at
`DEV_QEMU_MEM=8G`; `test-selfhost` builds the same kernel. The release kernel,
which `selfhost.sh install` builds by default, links at 4G. Fix it before
anything below: find which change grew the link's resident file pages or size
the cap for a linker, then make a refused fault in one process stop failing a
whole mount.

The guest builds the dev kernel in about 75 s at four vCPUs under KVM, against
49 s for rustup's dist compiler on the host's same four cores. The gap is the
compiler's build settings (PGO, BOLT, ThinLTO, one codegen unit, jemalloc), not
the kernel: bootstrap's plain stage1 takes 65 s on the host. `just toolchain
--pgo` builds the compiler with release settings and stays opt-in until it
passes `just test-devdisk` and someone times it in the guest. Performance is
off the critical path.

## Phase 1: move development into the guest

In QEMU, you can build and install a kernel in the guest today, but you cannot
work there. Linux became self-hosting at 0.11 when Linus moved his *work* onto
it, and what held that back was tooling around the compiler. Here the tooling
gaps are, in order:

1. **Source control.** The guest has no git. Sources reach it only by
   reseeding the whole volume from `HEAD` (`just reset devdisk`, then a cold
   build), and leave it only through `just devdisk-export`, which reads the
   image with `debugfs` after the guest shuts down. Redox, Asterinas (PR
   #3749) and SerenityOS (#12303) all bridge with git over the network, and so
   should SlopOS. Done when the guest fetches from the host, commits, and the
   host fetches the commit back; that retires reseeding and
   `export_devdisk.sh`. Decide which git first:
   - **C git**, cross-built with the SlopOS clang, with zlib. Complete: `just
     boot` serves the checkout with `git daemon` on the SLIRP host address,
     and `git://` needs neither TLS nor libcurl, which the guest lacks. But
     git is GPL-2.0-only, and a patch series to it in this tree is the
     verbatim GPL-2.0-only code `AGENTS.md` forbids. It needs an explicit
     exemption for a separate program's port, kept apart from SlopOS code.
   - **gitoxide**, MIT OR Apache-2.0, built by the guest's own cargo. It
     fetches and commits, but has no push and no upload-pack server, so the
     host takes commits back over dumb HTTP from a static server in the
     guest.
2. **Scripts run as `shell script.sh`.** Exec has no `#!` dispatch and there is
   no `/bin/sh`. C git runs hooks, aliases and `sh -c`; build scripts and
   ports assume both. Add `#!` handling to exec and install `/bin/sh`.
3. **The userland builds only on the host.** `build_userland.sh` is bash and
   the C++ runtime build is CMake and Ninja, so the guest can rebuild the
   kernel but not `init`, the shell, the coreutils or `libc.so`. Port the
   userland build to POSIX sh, as `build_kernel.sh` was, and give
   `selfhost.sh` a verb that installs into `/`. That forces a decision the
   loop dodges today: the host refreshes every binary it built on each `just
   boot`, so a guest-installed `/bin` must either win or be declared the
   guest's.
4. **POSIX gaps a port hits.** The cwd is per-thread; futexes are private
   only; there is no `mkfifo`, `vfork`, shell `trap`, `tgkill`, `sigqueue`,
   procfs or `current_exe`; directories have no htree; one mount serialises
   its mutations; there is one user, uid 0. Fix each when a port needs it,
   not ahead.

## Phase 2: bare metal

`just iso` builds the bare-metal artifact: kernel and initramfs, running from
RAM, keeping nothing. That lets you *try* SlopOS on hardware. You can only
*develop* there once it keeps what you write:

1. **Storage.** NVMe first, then AHCI. QEMU emulates both (Redox's `make qemu`
   defaults to NVMe), so write and test the driver in QEMU, then let `just
   boot` attach its disks through it and exercise it every session.
2. **An installer.** From the live ISO: partition a disk GPT (ESP, `/`,
   `/devel`), copy the running root, write the ESP with `fat-core` as `bootctl`
   does. That is Linux 0.12's route (boot a RAM root, `mkfs`, copy, set the
   root device) and Redox's installer's.
3. **The rest of a real machine.** PCI without MCFG, x2APIC, a real NIC (git
   needs one), USB HID input (`plans/usb-xhci.md`), ACPI SCI/GPE, and a log
   sink other than COM1, which carries KTAP in QEMU and is absent on most
   hardware.

The verified image (`fs/assets/ext2.img`, `verity=require`) no longer backs any
boot; the suite mounts it to exercise verity. Decide whether it becomes the
bare-metal read-only root or goes.

## Phase 3: the toolchain rebuilds itself (not committed)

Rebuilding LLVM in the guest needs CMake, Ninja and Python ported, tens of
gigabytes and hours of CPU, and `-Zbuild-std` until the target is tier 2.
Neither Redox nor Asterinas rebuilds its own compiler.

## Constraints

- Only `slopos-ostd` uses `unsafe`; `check_unsafe_expansion.sh` sees through
  macros. Nothing here earns an exemption.
- `KBox`/`KVec`/`KArc`/`KBTreeMap` only. A toolchain-sized buffer becomes a
  chunked or page-list design; `MAX_ALLOC_SIZE` stays 1 MiB.
- Stack frames stay under 2 KiB, against a 4 KiB guard page.
- Task ownership I1 to I8; no `async fn` in a kernel crate.
- GPL-3.0-or-later. No verbatim GPL-2.0-only or CDDL source. A third-party
  program shipped on an image needs a `NOTICE.md` entry.
- Ratchets are measurements: re-measure with the gate's `--emit-allowlist` and
  name the change that moved it.
- The verified image stays read-only and attested. Anything writable is a
  different medium.

## Decided

- **Toolchain.** LLVM cross-built from Linux into one prefix; cargo a pinned
  fork with `network` off; the compiler's crates ported by PR-shaped patches
  pinned by checksum. Cranelift and wild were measured against this kernel and
  do not reach it; `scripts/gates/{codegen,linker}/` re-ask whenever they
  install.
- **Linking and panics.** Hosted programs link through `cc` and unwind; the
  system's own binaries pin `rust-lld` and abort. The C++ runtime is libc++.
- **ABI.** Linux x86-64 syscall numbers, no Linux binary compatibility. Unlike
  Asterinas, which runs stock NixOS gcc and git, every tool here is a port.
- **Memory.** A commit ledger, not swap.
- **The dev loop.** One development machine (`just boot`, and `just
  boot-fast` to skip the wheel) and one live artifact (`just iso`); knobs
  (`KERNEL_RELEASE`, `VIDEO`, `ports`, `DEBUG`, `ROULETTE`) rather than more
  recipe variants, as Asterinas, Redox and SerenityOS all do.
  Persistent disks refresh in place and only `just reset` deletes one; Redox
  rebuilds its image and loses what the guest wrote.
- **Install.** A/B slots, a one-shot try and a commit are the one install
  path: the shape of `grub-reboot` and systemd-boot's boot assessment, which
  Redox, Asterinas and SerenityOS do not have. The host rebuilds the boot disk
  on every `just boot`.
- **Tests drive the human entry point.** The guest-side tests call
  `scripts/selfhost.sh`, so the loop you run is the loop `just test-selfhost`
  and `just test-install-guest` grade.
- **Source bridge.** The network, not a shared filesystem: no 9p or virtio-fs
  on the build path.
- **Kernel build.** One POSIX sh driver and one Rust symbol-table tool on both
  machines; identity means the same loadable image.
