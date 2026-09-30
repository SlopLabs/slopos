# SlopOS As A Development Machine

## Goal

Develop SlopOS on SlopOS: edit its sources, build them with the native
toolchain, install the result and reboot into it. QEMU first, bare metal after.

Every third-party tool in that loop is an upstream release compiled for
SlopOS. At most a patch teaches a project the `slopos` target, and nothing
else; anything more a port needs is a gap in SlopOS, fixed in SlopOS with
POSIX semantics.

## Where it stands

The loop is closed, and source moves through it by git:

```sh
just boot                                   # host: the development machine (just boot-fast: no wheel)
cd /src/slopos                              # guest: a clone of the host's checkout
git pull                                    # take the host's commits
scripts/selfhost.sh install                 # build the system, put it in the spare slot
bootctl reboot                              # try it once
bootctl commit                              # keep it; a reboot without this, or a panic, falls back
git commit -am '...' && git push            # hand a change back
```

`just boot` boots this build's system from an A/B boot disk it rebuilds every
run, with `/` on a persistent disk (`fs/assets/ext2-persist.img`). A slot is a
kernel and the base image it boots with, and the base — the programs, the C
library and what they read, under `/bin`, `/sbin`, `/lib`, `/usr/bin`,
`/usr/share` and `/etc/ssl` — is served read-only from the boot module and
mounted over the root, pinned, so the system is the slot's and the disk holds
what the machine wrote. The root carries the toolchain at `/usr/local` —
rustc, cargo, clang, lld, git, bash, CMake and Ninja, with
each project's licence text under `share/licenses`, rustc's and cargo's under
`share/doc` — which the one default
`PATH`, `/bin:/sbin:/usr/local/bin`, reaches after the system's own tools, and
a clone of the host's checkout at `/src/slopos` whose `origin` is that
checkout and whose push remote is a bare repository beside the root
(`fs/assets/guest-push.git`; `git fetch fs/assets/guest-push.git <branch>` on
the host). SLIRP runs one `git daemon --inetd` on the host per connection the
guest opens to `git://10.0.2.4/`, so nothing listens on the host and the
guest reaches exactly two repositories: the checkout read-only, the push
repository with receive-pack. The host owns the boot disk and the toolchain,
each replaced when it changes; the guest owns everything else, the clone
included once it is seeded, and installs what it builds for itself under
`/usr/local` as any Unix does, live. Before a boot the
host grows the root with `resize2fs` whenever its free space is under 6G, the
3.8G a clean dev and tests system build holds with room to spare, and
the root's integrity seal costs the blocks the host wrote rather than the
image. A clone taken from GitHub resolves its crates as the host's does: its
cargo fetches what `Cargo.lock` and std's lockfile for `-Zbuild-std` pin from
crates.io over HTTPS.

The whole tree builds in the guest. `scripts/selfhost.sh build` builds the
kernel, the userland — `init`, the shell, the coreutils, `libc.so`, and for
the tests variant the C++ runtime, built with CMake and Ninja for the SlopOS
platform CMake's own modules describe — and packs the base image, from the one
list of programs `scripts/lib/base.sh` gives the host's justfile too;
`install` writes the kernel and base into the slot that is not the default and
arms one boot, so a system is tried, committed or rolled back whole.

`just test-toolchain`, `just test-selfhost`, `just test-install-guest` and
`just bench-selfhost` boot a root of their own, rebuilt every run as the tests
image is: the toolchain, a clone seeded with the vendored crates and the
llvm-project sources so no build reads the network, and the fixtures the
toolchain ladder fetches from, so `just test` never depends on `just
toolchain`. `test-toolchain` climbs the ladder — rustc, cargo with a build
script and a proc macro, a git dependency through libgit2, a loopback registry
over TLS, clang, git against the host, a clone of
`https://github.com/SlopLabs/slopos`, crates.io over HTTPS, a bash script,
a Ninja graph and a CMake project — and finds a clone made on `/` intact after
a power-off. `test-selfhost` and `test-install-guest` run the loop in the
guest and grade it: the guest fetches the commit under test, its kernels pass
the ELF gates, its tests kernel and base pass the suite, and a system the
guest built boots from the spare slot, pushes a commit the host fetches,
commits and rolls back.

Every patch under `toolchain/` teaches its project the `slopos` target and
nothing else. A library or tool joins as a recipe under `toolchain/recipes/`: a
pinned tarball, its checksum, its licence texts and a build template that
`scripts/build_recipes.sh` compiles against slibc, and that
`scripts/check_recipes.sh` holds to leaving the source as shipped, bar a
patch of that one kind. Cargo builds
with its default features against the zlib, nghttp2, Mbed TLS, OpenSSL,
libcurl, libssh2 and libgit2 recipes. Git 2.55, bash 5.3, CMake 4.4 and Ninja
1.13 are programs beside them, built for `/usr/local`; CMake's recipe carries
its SlopOS platform modules, which the other recipes' builds and the C++
runtime's name too. Git is GPL-2.0-only, so libcurl takes its TLS from
Mbed TLS and not OpenSSL, and `check_recipes.sh` walks the `DT_NEEDED`
closure of everything a GPL-2.0-only recipe installs for a library its licence
does not allow, and its symbols for a static copy of one. For the same reason
the C library every one of them links is `MIT OR Apache-2.0`, and
`scripts/check_libc_license.sh` holds every crate `cargo metadata` resolves
for it to MIT.

A port finds the POSIX it expects: one working directory per process, `#!`
scripts, `/bin/sh` and `/usr/bin/env`, process-shared futexes, 64 signals with
queued realtime ones and `sigqueue`, FIFOs, `trap` in the shell, sockets made
nonblocking and close-on-exec as `socket` and `accept4` create them, which is
how libcurl opens git's connections, the group database, `utime`, `mkstemp`,
`freopen` and `execl` git reached for, and what bash, CMake and Ninja did:
`ppoll`, `pselect` and `sigsuspend` with the mask a wait takes for its
duration, a `select` family that runs on through a stop and hands back the
time it has left, `getopt_long` that leaves the words an option's handler
reads where they were, `sysinfo` and the load average, priorities,
`pthread_atfork`, a `/dev/pts` that lists its terminals so `ttyname` can
name one, and a `umask` the kernel applies to what a process creates, which
LLVM's `config.guess` sets in the shell. `libc.so` exports its data preemptible, so a program's copy of
`optind` is the one the library reads. A fork owes its copy as the child writes it, and a
write that finds no page makes the OOM killer take the process holding most of
what ran short, among those the writer may signal, instead of faulting the
writer.

The guest builds the dev kernel in about 62 s at four vCPUs and 8G under KVM
against 49 s for rustup's dist compiler on the same four cores; the gap is the
compiler's build settings, not the kernel. The whole dev system — kernel,
userland and base — takes about three minutes from a clean tree, and the tests
system, with the C++ runtime built from source by the guest's LLVM, two and a
half more.

**Open defect: the tests kernel does not link at the default 4G.** Its link
hits the file map's per-process cap, an eighth of usable memory: `pinnedbytes`
peaks at exactly the cap, `rust-lld` takes a refused file-page fault and dies,
and the filesystem it was writing remounts read-only. `test-install-guest`
and `test-selfhost` need `DEV_QEMU_MEM=8G` until the cap is sized for a linker
and a refused fault in one process stops failing a whole mount.

## Phase 1: bare metal

`plans/bare-metal.md`: storage, the filesystem, a boot chain that shares a
disk, the installer, a crash record and the network on the first real
machine.

## Phase 2: the toolchain rebuilds itself (not committed)

Rebuilding LLVM and rustc in the guest needs Python beside CMake and Ninja,
tens of gigabytes and hours of CPU, and `-Zbuild-std` until the target is
tier 2. Neither Redox nor Asterinas rebuilds its own compiler.

## Constraints

- Only `slopos-ostd` uses `unsafe`; `check_unsafe_expansion.sh` sees through
  macros. Nothing here earns an exemption.
- `KBox`/`KVec`/`KArc`/`KBTreeMap` only. A toolchain-sized buffer becomes a
  chunked or page-list design; `MAX_ALLOC_SIZE` stays 1 MiB.
- Stack frames stay under 2 KiB, against a 4 KiB guard page.
- Task ownership I1 to I8; no `async fn` in a kernel crate.
- GPL-3.0-or-later, and `MIT OR Apache-2.0` for the C library, which takes
  nothing copyleft. No verbatim GPL-2.0-only or CDDL source in this tree; a
  recipe names a tarball and carries none of it. A third-party program shipped
  on an image needs a `NOTICE.md` entry, and what a GPL-2.0-only one links must
  be GPL-2.0-compatible.
- Ratchets are measurements: re-measure with the gate's `--emit-allowlist` and
  name the change that moved it.
- The verified image stays read-only and attested. Anything writable is a
  different medium.

## Decided

- **A port changes nothing but the target.** A patch teaches a project it; a
  missing function goes into slibc or the kernel once, with POSIX semantics,
  instead of into each program that calls it. A library or tool is a recipe
  (tarball, checksum, template): the shape of Redox's cookbook without its
  patch list — Redox's git carries one, where SlopOS's git brought `<utime.h>`,
  `<grp.h>`, `mkstemp`, `freopen`, `execl`, `getpass` and
  `pthread_setcancelstate` into slibc. A recipe's patch may only add, and
  every hunk names SlopOS: CMake's is its platform modules and the SlopOS
  backend of its bundled libuv, the patch a port upstreams, where Redox
  carries a fork. `check_recipes.sh` holds both rules.
- **CMake knows SlopOS.** `CMAKE_SYSTEM_NAME` is `SlopOS`, from platform
  modules in `toolchain/cmake/Platform` that CMake's recipe carries in its own
  `Modules/` and a cross build names with `CMAKE_MODULE_PATH`, shaped as the
  SerenityOS, Haiku and Fuchsia platforms CMake ships are, and written to be
  contributed as they were. Named `Linux`, every project's checks answer for
  Linux; `Generic` loses `UNIX`, which libc++abi gates `__cxa_thread_atexit`
  on.
- **Bash runs scripts.** It is built without readline, whose terminal layer
  wants a termcap library this system does not carry; line editing is the
  SlopOS shell's.
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
- **Licensing.** The tree is GPL-3.0-or-later and the C library `MIT OR
  Apache-2.0`: slibc, `slibc-core` and `slopos-abi`, which the kernel shares
  and which is permissive whole rather than split, as Redox's `redox_syscall`
  is. The pair is the Rust ecosystem's and the `libc` fork's, and its MIT side
  is what a GPL-2.0-only program takes. relibc and musl are MIT, and Linux's
  UAPI headers carry the syscall note, for the same reason.
- **Git never links OpenSSL.** OpenSSL 3 is Apache-2.0, which GPL-2.0-only
  code cannot be combined with, so the one libcurl, git's HTTP transport and
  cargo's registry client, takes its TLS from Mbed TLS 4.1, the long-term
  branch, and OpenSSL stays for libgit2 and libssh2, which only cargo loads.
  `check_recipes.sh` holds that from the built objects. Debian and Ubuntu give
  git a GnuTLS libcurl, but libldap brings OpenSSL back, and they, Fedora and
  Arch rely on reading OpenSSL as a system library under GPLv2, which a git
  copyright holder disputes (Debian #1094969); Asterinas runs NixOS's git on
  an OpenSSL libcurl.
- **Distribution.** Local while SlopOS is pre-alpha: no package host. The ISO
  carries what a distribution's installer ISO does — the system, not a
  compiler or git — and the toolchain reaches a disk from the machine that
  built it, as the install payload of `plans/bare-metal.md`.
- **Crates.** crates.io over HTTPS in the guest, as on the host; the tests and
  CI build from the vendored crates with no registry.
- **Disks.** One persistent root, grown on demand the way SerenityOS grows its
  image, with the tools on it; a separate volume is the user's to make, and
  `mount(2)` and `mount=` attach any ext2 device. The heavy checks boot a root
  of their own, rebuilt every run, so nothing a boot left decides the next
  verdict.
- **Where the tools live.** A prefix of their own at `/usr/local`, found
  through one default `PATH` that names it after `/bin` and `/sbin`, so a
  writable disk cannot shadow the system's tools. Redox and SerenityOS install
  ports into the root and search `/usr/local/bin` after the system, and
  ChromeOS bind-mounts its dev-tools partition there; a link on the default
  `PATH` would do as well, since `exec` passes the canonical path the loader
  takes `$ORIGIN` from.
- **The dev loop.** One development machine (`just boot`, and `just
  boot-fast` to skip the wheel) and one live artifact (`just iso`); knobs
  (`KERNEL_RELEASE`, `VIDEO`, `ports`, `DEBUG`, `ROULETTE`) rather than more
  recipe variants, as Asterinas, Redox and SerenityOS all do. Persistent disks
  refresh in place and only `just reset` deletes one.
- **Install.** A/B slots, a one-shot try and a commit are the one install
  path for the system: the shape of `grub-reboot` and systemd-boot's boot
  assessment. The host rebuilds the boot disk on every `just boot`, so a stale
  system never boots by accident.
- **The system is the boot slot's.** A slot holds a kernel and the base it
  was built with, and the kernel mounts the base over the root read-only and
  pinned, so an install is tried, committed or rolled back whole and no boot
  runs a kernel beside another build's `libc.so`. ChromeOS and Android update
  the same way, as A/B system partitions; Fuchsia's system image, macOS's
  sealed system volume and FreeBSD's boot environments make the same split.
  The base is served from the boot module, not copied: an index of the
  archive, as a read-only system image is mounted rather than unpacked. What a
  user builds for themselves goes to `/usr/local` live, as on any Unix, and
  `/usr/bin` and `/usr/share` are the system's, as FHS has `/usr`.
- **Tests drive the human entry point.** The guest-side tests call
  `scripts/selfhost.sh` and take the host's commits with `git fetch`, so the
  loop you run is the loop `just test-selfhost` and `just test-install-guest`
  grade.
- **Source bridge.** Git over the network, not a shared filesystem: no 9p or
  virtio-fs on the build path, and no copying the tree. The tree under test
  comes from the host, where Asterinas's self-hosting demo and SerenityOS
  clone from the internet over HTTPS: `git://` over SLIRP needs neither TLS
  nor libcurl, and a `guestfwd` per repository runs the daemon only when the
  guest connects. The guest pushes into a bare repository, never into the
  checkout. GitHub is where HTTPS itself is graded.
- **Kernel build.** One POSIX sh driver and one Rust symbol-table tool on both
  machines, rebuilt until the embedded table is the kernel's own. The guest's
  kernel is graded by the gates and the suite, not by identity with the
  host's.
- **No patches of our own for speed or identity.** A toolchain patch that only
  makes something faster, or makes two machines build the same bytes, is not
  carried.
