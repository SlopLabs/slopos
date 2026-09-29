# SlopOS As A Development Machine

## Goal

Develop SlopOS on SlopOS: edit its sources, build them with the native
toolchain, install the result and reboot into it. QEMU first, bare metal after.

Every third-party tool in that loop is an upstream project compiled for
SlopOS unmodified. A patch teaches a project the `slopos` target and nothing
else; anything more a port needs is a gap in SlopOS, fixed in SlopOS with
POSIX semantics.

## Where it stands

The loop is closed, and source moves through it by git:

```sh
just boot                                   # host: the development machine (just boot-fast: no wheel)
cd /devel/src/slopos                        # guest: a clone of the host's checkout
export PATH=/devel/src/slopos/third_party/rust-slopos/bin:$PATH
git pull                                    # take the host's commits
scripts/selfhost.sh install                 # build the kernel, put it in the spare slot
bootctl reboot                              # try it once
bootctl commit                              # keep it; a reboot without this, or a panic, falls back
git commit -am '...' && git push            # hand a change back
```

`just boot` boots this build's kernel from an A/B boot disk it rebuilds every
run, with `/` on a persistent disk and the dev disk at `/devel`: the toolchain,
and a clone of the host's checkout whose `origin` is that checkout and whose
push remote is a bare repository beside the dev disk
(`fs/assets/devdisk.git`; `git fetch fs/assets/devdisk.git <branch>` on the
host). SLIRP runs one `git daemon --inetd` on the host per connection the
guest opens to `git://10.0.2.4/`, so nothing listens on the host and the
guest reaches exactly two repositories: the checkout read-only, the push
repository with receive-pack. The host owns the boot disk and the binaries it
installs on `/`; the guest owns everything else on both disks. `just
test-selfhost` and `just test-install-guest` run the loop in the guest and
grade it: the guest fetches the commit under test, its kernels pass the ELF
gates and the kernel suite, and a kernel the guest built boots, pushes a
commit the host fetches, commits and rolls back.

Every patch under `toolchain/` teaches its project the `slopos` target and
nothing else. A library or tool joins as a recipe under `toolchain/recipes/`:
a pinned tarball, its checksum and a build template that
`scripts/build_recipes.sh` compiles against slibc, and that
`scripts/check_recipes.sh` holds to leaving the source as shipped. Cargo builds
with its default features against the zlib, nghttp2, OpenSSL, libcurl, libssh2
and libgit2 recipes; in the guest it resolves a git dependency through libgit2
and fetches a crate over HTTPS through libcurl and OpenSSL. Git 2.55 is a meson
recipe beside them, built for the toolchain's place in the guest.

A port finds the POSIX it expects: one working directory per process, `#!`
scripts and `/bin/sh`, process-shared futexes, 64 signals with queued realtime
ones and `sigqueue`, FIFOs, `trap` in the shell, and the group database,
`utime`, `mkstemp`, `freopen` and `execl` git reached for. A fork owes its copy
as the child writes it, and a write that finds no page makes the OOM killer
take the process holding most of what ran short, among those the writer may
signal, instead of faulting the writer.

The guest builds the dev kernel in about 62 s at four vCPUs and 8G under KVM
against 49 s for rustup's dist compiler on the same four cores; the gap is the
compiler's build settings, not the kernel.

**Open defect: the tests kernel does not link at the default 4G.** Its link
hits the file map's per-process cap, an eighth of usable memory: `pinnedbytes`
peaks at exactly the cap, `rust-lld` takes a refused file-page fault and dies,
and the dev disk remounts read-only. `test-install-guest` and `test-selfhost`
need `DEV_QEMU_MEM=8G` until the cap is sized for a linker and a refused fault
in one process stops failing a whole mount.

**Open: the vendored crates do not travel.** The dev disk is seeded with the
crates `Cargo.lock` names, and the guest builds with no registry, so a commit
that moves `Cargo.lock` builds in the guest only on a fresh dev disk.

## Phase 1: SlopOS ships its own tools (not committed; decisions open)

The target: in `just boot`, `git clone https://github.com/SlopLabs/slopos`
anywhere on `/`, find `git`, `cargo` and `rustc` on the default `PATH`, run
`scripts/selfhost.sh install` from that clone, reboot, and find the clone still
there. The dev disk's first job, carrying source in and patches out, ended
with git; what keeps the tools on it now is below.

What is known:

- **Where the tools are.** Only on the dev disk, at
  `/devel/src/slopos/third_party/rust-slopos` (748M), because
  `selfhost.sh` looks for the toolchain inside the checkout, where the host
  keeps its sysroot, and git's meson prefix is compiled in as that path.
- **`PATH`.** The shell defaults to `/bin:/sbin`
  (`userland/src/apps/shell/env.rs`), slibc's `execvp` fallback and
  `_CS_PATH` to `/bin:/usr/bin` (`slibc/src/process/mod.rs`; no `/usr/bin`
  exists), the coreutils to `/bin:/sbin`. The shell reads no startup file and
  the root has no `/usr/local`. Every system surveyed puts extra tools where
  the default `PATH` already looks: Redox and SerenityOS install into the root
  (`/usr/bin`; `/usr/local`, with `PATH=/bin:/usr/bin:/usr/local/bin`),
  ChromeOS bind-mounts its dev-tools partition at `/usr/local`, NixOS and so
  Asterinas NixOS link a profile at `/run/current-system/sw/bin`.
- **A link is enough.** `exec` resolves symlinks and passes the canonical path
  as `AT_EXECFN` (`core/src/exec/mod.rs`), and slibc's loader takes `$ORIGIN`
  from it, so rustc, cargo, clang and git (`RUNPATH $ORIGIN/../lib`) reached
  through a link load from their real prefix. The initramfs unpacker skips
  symlinks.
- **Licence.** Git is GPL-2.0-only and links slibc, which is GPL-3.0-or-later,
  so `NOTICE.md` keeps git off every distributed image. rustc, cargo, clang and
  lld are shippable as they are; cargo's libgit2 carries the GCC linking
  exception.
- **HTTPS.** Git's recipe disables curl, so it clones only from the host's
  `git://` daemon, and GitHub turned `git://` off in March 2022. The curl and
  OpenSSL recipes exist and cargo already fetches over HTTPS in the guest, from
  a loopback registry. The guest takes a nameserver from DHCP and trusts
  `/etc/ssl/certs/ca-certificates.crt`; no test reaches GitHub or crates.io
  over the real internet, and `dns_resolve_test` is known-failing in a
  full-suite boot.
- **Persistence.** Under `just boot`, `/` is `fs/assets/ext2-persist.img`:
  writable, preserved across builds, grown with `resize2fs`, 512M by default.
  The host refreshes its binaries on it on every boot. `just test-persist`
  already grades a write surviving a power-off.
- **Crates.** A clone carries no `third_party/vendor`; the guest builds from
  the crates seeded onto the dev disk.
- **Build graph.** `just toolchain` is hours cold and needs the host's clang,
  CMake and Ninja; `just build`, `just test` and CI's boot lane do not depend
  on it and must not start to.
- **The live ISO.** 28M, of which the initramfs is 13M, unpacked into RAM and
  keeping nothing; the toolchain alone is 748M.

The work, once the decisions below are made:

1. **Relicense slibc** so git may link it on a distributed image. The closure
   of `libc.so` is slibc, `slibc-core`, `slopos-abi`, crt0 and the builtins
   beside vendored `libm` and `unwinding` (`MIT OR Apache-2.0`).
2. **Git over HTTPS:** enable curl in the recipe, and whatever slibc and the
   network stack the transport reaches for, graded against a real remote.
3. **One default `PATH`** for the shell, slibc and the coreutils, naming
   `/usr/local/bin` after `/bin` and `/sbin`, so a writable disk cannot shadow
   the system's tools.
4. **The toolchain as its own prefix at `/usr/local`**, installed onto the
   persistent root in QEMU; `selfhost.sh` takes the toolchain on `PATH`
   instead of `<checkout>/third_party/rust-slopos`. A guest kernel's `core`
   panic paths then differ from a host build's, which nothing grades.
5. **A clone on `/` survives a reboot**, graded like `test-persist`, with a
   root large enough for a checkout and its target directory.

Open, to discuss:

- **slibc's licence.** `MIT`, or `MIT OR Apache-2.0` as the Rust ecosystem
  does. Not Apache-2.0 alone: it is incompatible with GPL-2.0-only, which is
  the case this is for. `slopos-abi` is shared with the kernel, so it is either
  relicensed with slibc or split. Does the rest of the tree stay
  GPL-3.0-or-later?
- **What ships in RAM and what is fetched later.** Installers keep the live
  image small and download the rest: Asterinas NixOS's installer downloads
  its packages, Redox's `pkg` fetches from `static.redox-os.org`, ChromeOS
  `dev_install` fetches the dev tools into `/usr/local`. For SlopOS that means
  choosing among the toolchain as a package fetched over HTTPS after boot
  (a format, a host, verification), a second ISO flavour carrying it, and a
  compressed image; and deciding what the base ISO carries for the long term.
- **Crates in the guest:** crates.io over HTTPS, or a vendored mirror that
  travels with the toolchain.
- **The dev disk:** kept as a workspace that survives `just reset root`, as
  `/home` does, or dropped. Phase 3's installer assumes a `/devel` partition.
- **Size of the persistent root:** raise the 512M default, or grow it on
  demand.

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
  on an image needs a `NOTICE.md` entry. Git is GPL-2.0-only and links slibc,
  so until Phase 1 relicenses slibc it goes onto nothing but a disk built where
  it is used.
- Ratchets are measurements: re-measure with the gate's `--emit-allowlist` and
  name the change that moved it.
- The verified image stays read-only and attested. Anything writable is a
  different medium.

## Decided

- **Ports compile unmodified.** A patch teaches a project the target; a
  missing function goes into slibc or the kernel once, with POSIX semantics,
  instead of into each program that calls it. A library or tool is a recipe
  (tarball, checksum, template): the shape of Redox's cookbook without its
  patch list — Redox's git carries one, where SlopOS's git brought `<utime.h>`,
  `<grp.h>`, `mkstemp`, `freopen`, `execl`, `getpass` and
  `pthread_setcancelstate` into slibc.
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
  `scripts/selfhost.sh` and take the host's commits with `git fetch`, so the
  loop you run is the loop `just test-selfhost` and `just test-install-guest`
  grade.
- **Source bridge.** Git over the network, not a shared filesystem: no 9p or
  virtio-fs on the build path, and no copying the tree. The host is the
  remote, where Asterinas's self-hosting demo and SerenityOS clone from the
  internet over HTTPS: `git://` over SLIRP needs neither TLS nor libcurl, and a
  `guestfwd` per repository runs the daemon only when the guest connects. The
  guest pushes into a bare repository, never into the checkout.
- **Kernel build.** One POSIX sh driver and one Rust symbol-table tool on both
  machines, rebuilt until the embedded table is the kernel's own. The guest's
  kernel is graded by the gates and the suite, not by identity with the
  host's.
- **No patches of our own for speed or identity.** A toolchain patch that only
  makes something faster, or makes two machines build the same bytes, is not
  carried.
