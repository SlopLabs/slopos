# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
[`just`](https://github.com/casey/just) is the command runner; the `justfile` drives cargo + `rust-lld` via `scripts/`. Run `just --list` for all recipes. No git submodules — `scripts/ensure_limine.sh` fetches pinned Limine v12.9.3 into `third_party/limine` on first ISO build.

- `just setup` — install pinned nightly from `rust-toolchain.toml`; materialize the owned `slopos` sysroot (`scripts/make_slopos_sysroot.sh`, see below); verifies Go >= 1.22 on PATH (for `tools/run_tests/`)
- `just build` — emits `builddir/kernel-dev.elf`; `just iso` regenerates `builddir/slop.iso`, the live ISO bare metal boots from RAM
- `just boot` (the development machine: a persistent `/` carrying the toolchain at `/usr/local` and a clone of this checkout at `/src/slopos`, an A/B boot disk; spins the Wheel of Fate) / `just boot-fast` (the same without the wheel, `ROULETTE=0`) / `just boot-live` (the live ISO from RAM, no disk; `ROULETTE=0` skips the wheel) / `just boot-log` (the live ISO headless, 15 s timeout, fails unless `/sbin/init` launched)
- `just test` — the CI/agent entry point (see Testing Guidelines)

**Both targets build on an owned toolchain.** The kernel
(`targets/x86_64-slos.json`) and the userland
(`targets/x86_64-unknown-slopos.json`) are both built by `cargo +slopos`
against an *owned* sysroot at `third_party/rust-slopos` — the kernel too, so
one toolchain builds everything, and `trim-paths` makes a kernel's panic
locations read the same wherever the checkout is. That sysroot is a hardlink clone of the
pinned rustup toolchain whose `lib/rustlib/src` is a
real copy carrying two pinned forks — `rust-lang/rust`'s `library/` and
`rust-lang/libc` — applied from the patches under `toolchain/{rust,libc}`,
because `-Zbuild-std` reads std from the invoking sysroot's source tree and
nothing in this workspace can reach it. `scripts/make_slopos_sysroot.sh`
builds and registers it (idempotent: a stamp over those two patch directories
and `toolchain/PIN` makes a warm run ~20 ms), `scripts/ensure_toolchain.sh`
calls it, and `scripts/check_toolchain_pin.sh`
fails the build when the materialized sysroot has drifted from the pin.
Nothing on this path writes inside the rustup toolchain directory; the only
thing it adds to `$RUSTUP_HOME` is the `slopos` symlink that registration is.

The fork is cut against a *pristine* `rust-src`, and the retired std patcher
this replaces mutated that component in place, so a machine that ever ran it
needs a one-time reset. `make_slopos_sysroot.sh` refuses to run while the
residue is there and prints the fix; both halves are required, because
`rustup component remove` deletes only what its own manifest lists, so the
files the patcher *added* survive a reinstall (measured — a remove/add left
six of them behind):

```sh
ch="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml)"
rustup component remove rust-src --toolchain "$ch" && rustup component add rust-src --toolchain "$ch"
find "$(rustc +"$ch" --print sysroot)/lib/rustlib/src" -name '*slopos*' -prune -exec rm -rf -- {} +
```

**The compiler itself is forked too, and it is a different tree.** A JSON
target is enough to build *for*; bootstrap's `--host` resolves a triple
through rustc's own built-in list, so hosting a compiler needs
`x86_64-unknown-slopos` to be a built-in spec. That is
`toolchain/compiler/0001-slopos-target.patch` over the pinned nightly's
sources, with `0003`, which maps the tuple to a `CMAKE_SYSTEM_NAME` — an
unrecognised one falls back to `Generic`, which loses `LLVM_ON_UNIX` and with
it every `Unix/*.inc` file the LLVM port patches — and `0004`, which links the
C++ runtime statically (below). All three, and
`toolchain/llvm-rustc/0001-slopos-support.patch` (the LLVM port in rustc's
bundled llvm-project), are pinned by `toolchain/compiler/PIN` and materialised by
`scripts/make_rustc_src.sh` into `third_party/slopos-rustc-src` (265 MB
fetched, 656 MiB on disk, ~17 s; `just rustc-src`, removed by `just
distclean`). The sysroot above cannot carry it — it is a clone of a *built*
toolchain — so the two trees stamp their own inputs separately and neither
re-materialises for the other's edits. The built-in spec and the JSON one
must not drift: `scripts/check_rustc_target.sh` holds them equal.

**A build can read no registry.** `Cargo.lock` is tracked, and
`scripts/make_vendor.sh` (`just vendor`) fills `third_party/vendor` with every
crates.io package it pins *and* the ones `-Zbuild-std` resolves from the
sysroot's own `library/Cargo.lock`, which nothing in the workspace can see.
`.cargo/vendor.toml` points `crates-io` at that directory with `net.offline`
set, and is passed with `--config` rather than folded into
`.cargo/config.toml` because the same checkout drives cargo inside
`third_party/`'s materialised toolchain trees, whose dependencies are not
vendored here — so a host build still reads crates.io as it always did, and
the vendored road is the one the self-hosting root's clone and the offline
gate take.
`scripts/check_offline_build.sh` holds it: `--pins-only`, from
`check-framekernel-gates`, holds every registry package in both lockfiles to
the checksum its vendored copy records wherever `third_party/vendor` exists,
which is a developer's tree and not CI's gates step; `just
check-offline-build`, in CI, checks the kernel and the userland from an empty
`CARGO_HOME`, `--locked --offline`, where a warm registry cache cannot answer
for a missing crate. A new dependency is a `Cargo.lock` diff, a `just vendor`
and a `NOTICE.md` entry.

**The C++ runtime is test-only.** `x86_64-unknown-slopos` has
a C++ standard library: LLVM's `libc++` and `libc++abi`, built from the
llvm-project release `toolchain/cxx/PIN` names, linked whole-archived into a
single `third_party/slopos-cxx/lib/libc++.so`. `scripts/make_slopos_cxx.sh`
builds it on the host or in the guest, for the SlopOS platform
`toolchain/cmake` describes to CMake (idempotent: a stamp over the pin, the
compiler's version, the build script, the platform module and `slibc/include`
makes a warm run milliseconds),
`scripts/check_cxx_pin.sh`
gates it, and `scripts/build_userland.sh --test` is the only caller — the
shipped appliance root runs no C++ program, so the runtime is on the tests
image only, beside `cxx_probe` and `libcxxtest.so`, whose `cxx_test` proves a
C++ exception crosses a `dlopen` boundary and then exercises the localized
half — iostreams, `<iomanip>`, `std::locale`, `std::to_wstring` and an
`<fstream>` read — and `cxx_static_probe`, which links
`libc++.a` and `libc.a` instead and so is the only thing that exercises either
archive or the frame finder's `AT_PHDR` road. One artifact rather than two because
libc++abi's caught-exception stack and the `type_info` a `catch` matches on are
process-wide: two instances of it in one process is a throw that cannot be
caught across the boundary between them.

**The runtime is configured for a compiler.** Localization, wide characters,
`<filesystem>` and the random device are on, because LLVM's own sources reach
all four; the time-zone database is off, because nothing does and there is no
zone data here. `_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` takes libc++'s own
character table instead of the glibc `__ctype_b_loc()` road, and it is an ABI
flag — it decides the width and bits of `ctype_base::mask`, a type passed by
value — so it is written down once, in `make_slopos_cxx.sh --print-abi-flags`,
and every consumer asks for it there. `libbuiltins.a` is a third artifact
beside `libc.a` and `libc.so`: the same compiler-rt routines built
`relocation-model=pic`, because x86-64 codegen calls out for 128-bit
arithmetic, there is no libgcc here, and a cdylib's version script localises
everything but its own exports.

**The llvm-project sources are patched too.** `toolchain/llvm/` is the SlopOS
port — the two places LLVM dispatches on the OS with no default a new one can
take, the `Triple` entry and clang target that make `__slopos__` a macro a
compiler predefines, `toolchains::SlopOS`, the clang driver that turns
`cc a.o -o a` into the link line `build_userland.sh` writes by hand, and the
`config.guess` entry that names a SlopOS machine to the runtimes' configure,
which `make_slopos_cxx.sh` applies to the part of the tree it unpacks — pinned
by checksum in `toolchain/cxx/PIN` beside the tarball it applies to,
materialised by `scripts/make_slopos_llvm_src.sh` into the same
`third_party/llvm-project-<version>.src` the runtime is built from (1.5 GB on
disk, 11 s), and held by `scripts/check_llvm_port.sh` and
`scripts/check_clang_driver.sh`.

That build is the one place SlopOS needs host tools beyond rust and QEMU:
`clang`, `clang++`, `ld.lld` and `llvm-ar` of **one** LLVM major at or above
`clang_major_min`, plus `cmake` and `ninja`. The floor is a floor and not an
equality because no Linux ships LLVM 18 by default any more — Arch is at 22
and packages no versioned `llvm18` in either the repositories or the AUR,
Fedora is at 20 with an `llvm18` compat tree, Debian 13 at 19, Ubuntu 24.04 at
18 — and the two pins protect different things: the *sources* fix the C++ ABI
and the libc symbols `libc++.so` ends up needing, which is slibc's side of the
contract, while the host compiler only codegens them. `clang_major_tested`
records the majors that have built a green `just test` (18 in CI, 22 on a
rolling host, 23 in the guest, whose suite `just test-selfhost` runs); a major outside it builds with a note on stderr, because
`cxx_test` and `cxx_static_probe` on the tests image are the functional gate,
not a version string.

`scripts/cxx_host_tools.sh` resolves the four tools and is the only thing that
decides: the sources' own major wherever it is installed
(`clang-18`, apt.llvm.org's `/usr/lib/llvm-18/bin`, Fedora's
`/usr/lib64/llvm18/bin`) first, since that is the pairing upstream tests, then
the unsuffixed default, then any other installed major above the floor. All
four must report the same major — a host with clang 18 and lld 22 on PATH
would otherwise link the runtime with a mismatched linker — and
`build_userland.sh` compiles the C++ probes with the toolchain that answered,
not with whatever `clang++` resolves to. `CLANG`/`CLANGXX`/`LD_LLD`/`LLVM_AR`
override the names and never fall back, which is how CI pins itself to a
distribution's `-18` suffixes. A host compiler upgrade changes the stamp and
rebuilds the runtime rather than leaving one built by a compiler that is no
longer installed.

The sources are the pinned `llvm-project` release tarball, fetched into
`third_party/` on the first
build; an offline checkout pre-populates that file or points `LLVM_URL` at a
local copy, exactly as `LIMINE_URL` works for Limine. `just distclean` removes
the extracted 1.5 GB source tree — the C++ runtime and the LLVM port share
it, so it carries `llvm/` and `clang/` as well as the two runtime
directories; nothing removes `third_party/slopos-cxx`, because the build is
minutes and its inputs are pinned.

**The toolchain that runs on SlopOS is cross-built from here.**
`scripts/bootstrap_slopos_toolchain.sh` (`just toolchain`) assembles a target
sysroot out of the staged libraries, slibc's headers and the C++ runtime,
writes a `bootstrap.toml` and a compiler wrapper into
`builddir/slopos-toolchain/`, and runs `x.py install` with
`--build=x86_64-unknown-linux-gnu --host=x86_64-unknown-slopos`. Bootstrap's
build directory is `builddir/slopos-rustc-build`, outside the source tree, so
re-materialising the tree for a patch keeps the host LLVM and every stage
already built. The wrapper names two triples on purpose: SlopOS to compile,
because otherwise the preprocessor defines `__linux__` and LLVM takes
`/proc/self/exe` paths this system has not got, and Linux to link, because the
host clang has no SlopOS toolchain and for that triple hands the link to
`gcc`; `--gcc-toolchain` points it at the sysroot so no host GCC library is
found. It is `toolchains::SlopOS` written in shell and it goes away the day a
clang built from `toolchain/llvm/` runs the build. `x.py install` ships no
clang and no sysroot, so the script completes `builddir/slopos-toolchain/install`
into one prefix: clang, `libclang-cpp.so` and its resource directory, the
`cc`, `c++` and `ld.lld` links, clang config files that name `<CFGDIR>/..` as
the sysroot, and the target sysroot itself in `lib/` and `include/`; the Linux
std is dropped. `just check-bootstrap-config` is the affordable half —
bootstrap's own dry run plus a compile and a link through the wrapper — and,
once a prefix is installed, it grades that too.

**The target is a host's target.** `x86_64-unknown-slopos` links through `cc`
(`gnu-cc`, and `no-default-libraries: false`, because the clang driver's
`toolchains::SlopOS` owns `crt0.o`, the interpreter, `-lc` and compiler-rt), has
rpath, and unwinds: a build script, a proc macro or a panicking rustc needs
nothing a SlopOS program cannot have. The system's own artifacts keep their
hand-written link line and `panic = abort` — `build_userland.sh` puts
`SYSTEM_RUSTFLAGS` (`-C linker=rust-lld -C linker-flavor=ld.lld -C
panic=abort`) on every build line — which is why `userland.ld` still discards
`.eh_frame`. The compiler links LLVM against `libc++`: `rustc_llvm` picks it
for `slopos` as it does for FreeBSD, and `llvm.use-libcxx` is not set because it
would do the same to the Linux stage1 compiler. `llvm.static-libstdcpp` links
it statically into libLLVM, which exports it to libclang-cpp and the LLVM
executables (one copy, because libc++'s error categories are compared by
address), and into librustc_driver (`toolchain/compiler/0004` has bootstrap
find `libc++.a`), with `-Bsymbolic` for LLVM's shared objects and
`-Bsymbolic-functions` for the Rust ones, and every SlopOS object of the
toolchain is linked with `-z pack-relative-relocs` (`DT_RELR`, which slibc's
loader applies).

**The C libraries cargo's and git's network transports link are recipes, and
so is git.** `toolchain/recipes/<name>/recipe` pins an upstream release
tarball (URL, SHA-256, licence), a build template (`cmake`, `meson`,
`openssl` or `autotools`), its configure arguments and its dependencies: zlib,
nghttp2, Mbed TLS, OpenSSL, curl, libssh2, libgit2, git and OpenSSH.
`scripts/build_recipes.sh` (`just recipes`) builds them shared and `-z defs`
into `builddir/slopos-recipes/prefix`, with the
target sysroot and `x86_64-unknown-slopos-clang{,++}` wrappers that
`scripts/make_slopos_cross.sh` assembles for bootstrap too; tarballs are
cached in `third_party/recipes/` (`--fetch-source` prints them), and each
template adds what it runs to the C++ runtime's host tools — `make` and
`pkg-config` for autotools, `meson`, `cmake`, `ninja` and `perl` — whose
versions join that template's stamps, and a recipe with a patch needs `git`,
which only applies it. A library is
`$ORIGIN`-rpathed; a program (`program=`) is built with `--prefix` the guest
path of the toolchain, because git finds its exec path, templates and system
config through that compiled-in prefix, and its run path reaches `lib/`. The
programs are git (`meson`), CMake and Ninja (`cmake`), and bash, e2fsprogs
and OpenSSH (`autotools`: out of tree, `--host` the target, the prefix's
headers and libraries on the search paths, and the link flags — `-z defs`
and the run path — in a response file beside the wrappers so neither make nor
a shell expands `$ORIGIN`, and the answers configure cannot find out without
running a SlopOS program given as `*_cv_*` cache values); a recipe that
declares no library installs nothing under `include/` or `lib/`, so
e2fsprogs's static libraries reach neither another recipe nor the target
sysroot. Bootstrap copies the libraries into the target sysroot and
`build_recipes.sh --install-programs` copies everything else a recipe
installed into the install, so all of it reaches a root: OpenSSH's `ssh` and
`ssh-keygen`, which git and the ladder run, arrive with its server, `scp`,
`sftp`, the agent and the helpers, which nothing runs. Only the libraries'
stamps clear cargo. The `cmake` template builds for `CMAKE_SYSTEM_NAME=SlopOS`,
the platform `toolchain/cmake/Platform` describes. **A patch only teaches the
target:** a build that would need any other edit to upstream is a slibc or
kernel finding, fixed there — git's build is what brought `<utime.h>`,
`<grp.h>`, `mkstemp`, `freopen`, `execl`, `getpass` and
`pthread_setcancelstate` into slibc, bash's, CMake's and Ninja's brought
`ppoll`, `pselect`, `getloadavg`, `getopt_long` and `ttyname`, and
e2fsprogs's brought `system`, `popen`, `daemon`, `random`, `<libgen.h>`,
`secure_getenv`, `PR_GET_DUMPABLE` (not dumpable under `AT_SECURE`, as a
setuid program is not), `si_addr`, the block requests in `<sys/ioctl.h>` and
`<net/if.h>`'s interface requests, which slibc answers from the kernel's net
query, and OpenSSH's brought `<resolv.h>`'s `res_init`, `res_query` (any
record type, asked of the kernel resolver's nameservers over UDP from a
random source port and over TCP when truncated, each step bounded by the
resolver's timeout) and `dn_expand` with `<arpa/nameser.h>`, `initgroups`,
`getitimer` and `setitimer` (which disarm and refuse to arm, as `alarm`
does), `<paths.h>`, `<netinet/in_systm.h>`, `<netinet/ip.h>`'s
type-of-service values, the BSD `timeval` macros, `caddr_t`,
`IPPORT_RESERVED`, `IN_LOOPBACKNET` and `scanf`'s `%[` scansets, without
which ssh refuses every server's banner. A
recipe's `patch=` (`NNNN-slopos-<what>.patch`, hashed into its stamp) has the
shape of one that teaches the target, which review holds it to: it only adds —
a run of removed lines is replaced where it stood, line for line, by added
lines that start with them — patches each file in one section, names SlopOS in
a line every hunk adds or the path of a file it creates, and under CMake's
`Modules/Platform` only creates `toolchain/cmake/Platform`'s files, byte for
byte, which with the SlopOS backend of its bundled libuv is all CMake's
carries. A diff carries upstream's own lines as context, so a recipe under a
licence this tree may not hold the text of (GPL-2.0-only, CDDL) carries none.
`scripts/check_recipes.sh` holds the
shape and holds every `arg` and the OpenSSL target definition to a grammar
that can carry no code (a flag, CMake script, launcher or search root edits
what is built without touching a file); the driver fails any build that
changes the tree the patches leave. **Git links no OpenSSL:** git is GPL-2.0-only and
OpenSSL 3 is Apache-2.0, so libcurl, git's HTTP transport and cargo's
registry client, takes its TLS from Mbed TLS (`Apache-2.0 OR
GPL-2.0-or-later`), and OpenSSL stays for libgit2 and libssh2 alone.
`check_recipes.sh` walks the `DT_NEEDED` closure of everything a built
GPL-2.0-only recipe installs and fails on a library under a licence that code
cannot be combined with, or on a static copy of one, found by the symbols that
library exports among every definition in the closure's symbol tables, local
ones included.

**`just toolchain --pgo` builds the compiler as a Rust release is.** ThinLTO
and one codegen unit for rustc's crates, ThinLTO for LLVM, and
profile-guided optimisation of both, the settings the two configurations
below share living in `scripts/lib/rustc_build_settings.sh`. It is opt-in
(`--pgo` or `SLOPOS_TOOLCHAIN_PGO=1`): without it the configuration is the
plain one, and no SlopOS-hosted compiler has yet been built with it. The profiles
come from `scripts/make_toolchain_profile.sh` (`just toolchain-profile`),
rust-lang's opt-dist with this repository's kernel build as the workload: in
`builddir/slopos-pgo-build`, a Linux-hosted build of the same sources with
the same settings builds an instrumented LLVM under a stage1 compiler and runs
`scripts/build_kernel.sh` for the dev and tests kernels on it (the guest's
`selfhost_test` build, with the stage's own cargo and the vendored sources),
then an
instrumented stage2 compiler over the optimised LLVM and runs it again. The
merged profiles land in `builddir/slopos-pgo`: LLVM's merged by the host's
`llvm-profdata`, because the host clang compiled the instrumented objects and
compiles the SlopOS LLVM with the result, rustc's by the build's own. `just
toolchain --pgo` makes them first when they are missing or stale — a stamp
over the compiler tree's source stamps, the host clang, the shared settings,
the wrapper and the script's `PROFILE_FLOW`, not over the kernel — and `just
toolchain-profile --optimized-host` builds the Linux-hosted twin with them.
Timed on the dev kernel from an empty target directory (four P-cores):
rustup's dist rustc 50.4 s wall / 117 s user, the plain stage1
65.6 s / 137 s, the PGO twin 49.8 s / 102 s. A profile is keyed by symbol,
and cargo hashes the target triple into `-C metadata`, rustc hashes that into
every crate's `StableCrateId`, and every v0 symbol carries it: out of one tree
and one compiler, the Linux std's `core` is `CsgRlzlzJNmri_4core` and the
SlopOS one's `Cs61faTTiSLg5_4core`. Both builds therefore compile every crate
through `scripts/rustc_neutral_metadata.sh`, a `RUSTC_WRAPPER` that replaces
cargo's value with a hash of package, version, crate name, crate types,
host-or-target and bootstrap's per-mode `__CARGO_DEFAULT_LIB_METADATA`.
Cargo does not fingerprint a wrapper and bootstrap keys LLVM on a commit a
tarball lacks, so both scripts clear a build directory's Rust stages when the
wrapper changes and a triple's LLVM and lld when its settings, profile or
host clang do; a plain build clears only what a `--pgo` build left. cc-rs
forwards `-Cprofile-generate`/`-Cprofile-use` to a clang, so the profile
build's C compiler drops the rustc profile flags: the host clang's LLVM is not
rustc's, and mixed records crashed the instrumented compiler at exit.

**The result lands on the root.** `just boot` installs the prefix `just
toolchain` built, or the one `TOOLCHAIN_STAGE` names, at `/usr/local` on the
persistent root, where the one default search path finds it:
`/bin:/sbin:/usr/local/bin:/opt/bin` (`slopos_abi::fs::DEFAULT_PATH`), which
the shell (its exported default and `command -p`), slibc's `execvp`, the
coreutils and remoted's commands all take, puts it after the system's own
tools, so nothing a writable disk holds shadows them, and the commands the
owner granted extensions after it, so they shadow neither; a person's own
`PATH` (`slopos_userland::root_layout::search_path`) puts `~/.local/bin`
between the system's tools and `/usr/local/bin`. `scripts/build_fs_image.sh`
takes two kinds of tree, both installed through one debugfs session
(`scripts/fs_tree.py`), which gives each the owner of its destination under
the ownership map — the first person's, uid 1000, within a tree the image's
owners record says they hold, uid 0's elsewhere — writable by that owner
alone whatever the host's modes say. A host tree
(`FS_HOST_TREES`, `<host dir>:<guest dir>[:<name>]`) is the host's: what it
installed is recorded beside the image in `<image>.host/trees/<name>`, where the
guest cannot edit it, with a copy under `/var/lib/slopos/trees` for the guest to
read, owned with the tree, and whenever the host directory changes the tree is replaced — exactly
what the manifest names removed, whatever the guest put beside it kept. The
image is read only through `ls -l` listings of directories bound to the
inodes their parents gave them, so no symlink the guest makes redirects a
request, and an install that would overwrite something the guest left, or
something that is no longer the kind the manifest names, is refused before
anything is written: the tree stays as it was, the build warns and goes on,
and the next build tries again. A seed tree (`FS_SEED_TREES`) is the guest's
once it is there and is copied only onto a root without its directory. The
root is grown with `resize2fs` to hold both and keep `FS_FREE_FLOOR` free
(`ROOT_FREE_FLOOR`, 6G: a clean dev and tests system build — kernels,
userlands, bases and the C++ runtime — holds 3.8G of it) on every build, not only one that changed something, and
carries a 64M log, the size its grown volume would get. `gen_verity.py`
streams the image and reads only the blocks a seal attests, so a grown root
costs what the host wrote rather than its size. A seal keeps what it attested,
the bitmap and each block's hash, in `<image>.host/verity`, since the guest can
write every byte of the image, trailer included; before a preserved v2 image
is grown or refreshed, the blocks the guest owns are the allocated ones not
attested there with the recorded content still in place, and those stay
un-attested through what follows. A trailer is one only past the filesystem's
extent and describing exactly it, so bytes the guest writes that look like one
are only its data. The guest's cargo reads std's sources from
`/usr/local/lib/rustlib/src`, so a guest kernel's `core` panic locations
differ from a host build's; nothing grades them. `just boot` gives the guest
6G of RAM (`DEV_QEMU_MEM`), because the file map pins at most an eighth of
memory per process and the debug tests kernel's link maps more than 4G's
eighth, and the optimized kernel, because a dev-profile kernel spends ten
times as long in every syscall and page fault a compiler makes.

**The source moves by git.** A root without `/src` is seeded with
`/src/slopos`, which `scripts/stage_workspace.sh` makes: a clone of the
checkout with `HEAD`'s branch checked out (`git clone --no-local`, so the
history is what the branches and tags reach and nothing else). On the
developer's root it resolves its crates from crates.io, as the host does;
`--vendored` adds the vendored crates in its ignored `third_party/vendor` and
the offline vendor configuration in `/src/.cargo/config.toml`, above the tree,
where cargo reads it for any directory below while the clone's own
`.cargo/config.toml` stays as committed, and the llvm-project tarball the
C++ runtime is built from, which the developer's clone fetches. The clone's `origin` is
`git://10.0.2.4/slopos` and its `host` remote, the push default, is
`git://10.0.2.4:9419/slopos`: with `GIT_PUSH_REPO` set, `qemu_run.sh` adds two
SLIRP `guestfwd` rules, as for the echo peer, each running one `git daemon
--inetd` per connection — one serving this checkout read-only, one serving
the bare repository `GIT_PUSH_REPO` names with receive-pack — so nothing
listens on the host. Each daemon is pinned to its git directory twice: its
`--interpolated-path` answers a request that names a host, which every git
client sends, and `--strict-paths` with that directory as the whole allowlist
refuses a hand-made request that names none and so bypasses the template.
With `GIT_SSH_PEER` naming the host's half of the ladder's ssh fixture too,
a third rule runs `sshd -i -E <it>/sshd.log -f <it>/sshd_config` per
connection to `10.0.2.4:22`, as the host's own user: publickey only, for the
one guest key in its `authorized_keys`, with a host key the guest's
`known_hosts` names, no forwarding, no pty and no PAM, and a `ForceCommand`
that runs `git-upload-pack` on this checkout for `ssh://10.0.2.4/checkout`
and `git-receive-pack` on `GIT_PUSH_REPO` for `ssh://10.0.2.4/push` and
refuses anything else. libslirp gives the command the connection as stdin,
stdout and stderr, so sshd logs to that file and never to stderr.
`just boot` names `fs/assets/guest-push.git`, which `just reset root` keeps;
the tests name a scratch repository under `builddir/`, `test-toolchain`'s
with this checkout's objects as an alternate, so receive-pack offers its refs
as haves and a push sends only what is new. In the guest, `git
pull` takes the host's commits and `git push` hands the guest's back (`git
fetch fs/assets/guest-push.git <branch>` on the host); `just export-file`
copies one file, a kernel the guest built, off a root whose guest has shut
down.

**The heavy checks boot a root of their own.** `fs/assets/ext2-selfhost.img`
(`just _fs-image-selfhost`), rebuilt every run as the tests image is, is the
persistent root's shape, booted with the tests base: the toolchain at
`/usr/local`, the clone seeded `--vendored`, so no build reads a registry,
and at `/home/user/ladder` the fixtures `scripts/stage_ladder_fixtures.sh` makes, a
bare repository of one crate and a sparse registry of one crate with a fresh
test root and server identity, and the guest's half of a fresh ssh key pair
and host key, whose host half stays in `builddir/ladder-sshd` for both boots
of `test-toolchain`. Staging them needs `git`, `openssl` and `ssh-keygen` on
the host, and `test-toolchain` with a toolchain installed needs `sshd` (on
PATH or at `/usr/sbin/sshd`) as well. `just test-toolchain`, `test-selfhost`,
`test-install-guest` and `bench-selfhost` boot it as `/`, so `just test`
never depends on `just toolchain`. `toolchain_test`, `selfhost_test` and
`reboot_clone_test` run as the session does, uid 1000 holding nothing gated,
since git refuses a clone another uid owns; a root test runs git and
`selfhost.sh` in the clone as the session (`slopos_userland::selfhost`).
`toolchain_test` holds the toolchain to
the manifest the host installed it with and the clone to its vendored crates,
and then climbs the toolchain ladder: `rustc --version` under
`LD_DEBUG=statistics`, rustc linking a program through `cc`, cargo building a
crate with a build script and a proc macro, cargo fetching a `git =
"file:///home/user/ladder/git/greeting.git"` dependency through libgit2, cargo
fetching a crate through libcurl and Mbed TLS from the fixture registry,
served over loopback TLS and verified against its test root (the image's own
CA bundle must refuse it first), clang compiling C and C++, git reading the
clone and reaching the host's checkout, `ssh -V`, `ssh-keygen` deriving the
fixture key's public half, and git over ssh to the host's sshd — `ls-remote`
and a fetch of the checkout, a commit on the fetched `HEAD` pushed into the
run's scratch repository, which the host holds to the commit the guest notes
and to its own `HEAD` as that commit's parent, and a command the forced
command refuses — git cloning
`https://github.com/SlopLabs/slopos` at depth 1, trusting the image's CA
bundle once a root minted in the guest has been refused, cargo resolving
that clone's lockfile and std's from crates.io over HTTPS — the two rungs that
need the host's internet, as `dns_resolve_test` does — and then bash running a
script with a pipeline, a background job and a here-document, Ninja running a graph and finding nothing left
to do, and CMake configuring a C project for the SlopOS platform, generating
for Ninja and building a program against a shared library it reaches by its
run path. `reboot_clone_test`
clones the workspace to `/home/user/clone` and commits there on one boot and, on
the next boot of the same image, holds the clone to `git fsck --strict` and to a
clean status over an index without stat data, which rehashes every file.

**The whole tree builds in the guest.** `scripts/build_kernel.sh` is POSIX sh
that runs under `/bin/sh` with the coreutils and nothing else;
`scripts/build_userland.sh`, `make_slopos_cxx.sh` and `cxx_host_tools.sh`
are bash, CMake, Ninja and clang, which the toolchain carries; the base image
is packed by `tools/initramfs`. What only the host has —
`ensure_toolchain.sh` before, the ELF gates after
(`scripts/check_kernel_elf_gates.sh`) — lives in the justfile's `_kernel`
and `_build-userland` recipes, which pass `cargo +slopos`; in the guest
`build_userland.sh` finds std's fork by the `library/.slopos-std-stamp` the
installed toolchain's rust-src carries, as the owned sysroot's does. Both
build `compiler_builtins` one intrinsic per object, as std's own manifest
does, because `-Zbuild-std` takes this workspace's profile: in ten objects,
linking one intrinsic drags in the weak hidden libm routines beside it, and
their visibility wins over the `floor` `libc.so` exports. Which programs a
base carries is `scripts/lib/base.sh`, which the justfile and
`scripts/selfhost.sh` both read; the recipe programs among them come from the
recipes' prefix on the host and from `/usr/local` in the guest, where the
toolchain installs them. The embedded symbol table comes from
`tools/kallsyms`, an ELF reader
built for whichever machine runs the build, byte-identical to what `llvm-nm`
gave less LLVM's `.llvm.<hash>` promotion suffix, which would keep a release
table from ever reaching a fixed point; the driver writes the empty safestack runtime archive itself; and
`trim-paths` keeps every absolute path out of the image. `just test-selfhost`
is the whole loop: the guest builds the dev and tests systems — kernel,
userland and base (`selfhost_test`) — the host holds the root to `e2fsck`,
exports both kernels and the tests base, grades the kernels with the ELF
gates and runs the suite on the guest's tests kernel and base (`just test-elf
ELF=… BASE=…`). The guest's artifacts are not held to a host build's bytes:
the gates and the suite are the grade.

**A SlopOS disk shares its ESP.** Every disk SlopOS is installed on, the QEMU
boot disk `scripts/build_bootdisk.sh` makes included, carries Limine at
`\EFI\SlopOS\BOOTX64.EFI` with its configuration beside it, which Limine reads
ahead of any other loader's on the same ESP, and its `LICENSE` and
`3RDPARTY.md`, the notices its binaries must carry, as `LICENSE.limine` and
`3RDPARTY.limine.md`, as on the ISO and the install medium; a FAT32 boot
partition holding the slots; the ext4 root, unless the root is a disk of its
own; and a 4 MiB crash partition. SlopOS's partitions carry
type GUIDs of its own, never the Discoverable Partitions Specification's root
or XBOOTLDR types, which a Linux on the same disk would mount as its own.
`boot-core` is that disk as data: GUIDs and the GPT, which the kernel's
partition probe and `bootctl` both read through it, load options and device
paths, the boot manager variables and what a write to them must look like, the
Boot Loader Interface's strings, the layout and the Limine configuration, and
an install's half: the table a disk is given, written with each piece behind
a flush and the copy it was not read from first, and where a plan puts
SlopOS's partitions
(`boot-core::install`). It
is `no_std`, free of I/O and host-tested under `just test-host`, against
tables `fdisk` wrote and `sfdisk --verify` reads among them, and
`tools/bootdisk` is how the host's disk builder asks it for the layout and the
configuration, and holds a base and a kernel to what `bootctl` takes. The configuration names
each slot's kernel and the two bases by the boot partition's GUID and names no `default_entry`,
because Limine 12.9 consults `LoaderEntryDefault` only while that is unset; it
is written at install and never by a commit. The removable-media path
`\EFI\BOOT\BOOTX64.EFI`, with a copy of the configuration beside it, is written
only on an ESP SlopOS created: on a shared one it belongs to whoever put a
loader there. The firmware reaches SlopOS's own copy through a `Boot####`
entry, which only a `BootEntry` holder — the installer, init and the utests —
may register: `efivar_get` and `efivar_set` are gated on `Power` and reach the
Boot Loader Interface's namespace and SlopOS's own, and `BootEntry` adds
`Boot####`, `BootOrder`, `BootNext` and `BootCurrent` in the
global one and nothing else there, with every write held to non-volatile boot
and runtime access and to the format the firmware parses. An entry is matched
by the partition GUID and file it names, wherever its device path puts them,
so registering again changes nothing. In QEMU the persistent and
self-hosting roots stay disks of their own, since the host grows and refreshes
them as image files; `just test-install` builds its disk around the tests
root, so its A/B loop runs on the bare-metal shape, root partition and all.

**The base is the boot partition's, and every slot boots it.** Each slot of
the boot partition holds a kernel, `/boot/<slot>/kernel.elf`, and beside the
slots `/boot/base/` holds the base every slot boots, `current.img`, and the
one that ran before it, `previous.img`, which every Limine entry loads as the
modules `initramfs` and `initramfs-previous` (`layout::BASE_*`). Limine fails
an entry whose module it cannot open, so a disk always holds both, and an
install writes both with one image; no slot is named `base`. In the loader
handoff, before memory is laid out, the kernel reads each one's
`/usr/share/slopos/base-interface` (`boot/src/base.rs`, through
`boot-core::newc` and `boot-core::base`, the reading `bootctl` gives a base)
and boots current when it serves that interface, previous when it serves
previous's, and otherwise fails its `initramfs root` step naming both. It
logs `BOOT: base current|previous (<n> bytes), interface <i>`, and the pages
of the base it left unused — every whole 4 KiB page the module covers —
join usable memory before the frame allocator
is seeded, so nothing can read them after. The base leaves the slots so a
damaged root still boots to a shell and booting the other kernel never
changes the userland. `fs/src/basefs/` serves the chosen `newc` archive where it lies: an index of its
paths built at boot, a file's bytes a slice of the module, every inode sealed
and nothing writable, so the booted base costs no copy. That is generation 1;
`base_activate(archive, len)` (`BaseUpdate`, which privd's base actions
hold) makes another the base while the
system runs, a copy in pieces it owns, each a page short of 64 KiB so that
with the heap's header it fills a 16-page block, refused when its interface is
outside the kernel's range, past a quarter of memory (`EFBIG`), or while all
sixteen slots are held or the copies still resident would take more than that
quarter with it (`EBUSY`), and moves every base mount to it in one pass under
the mount table's lock, all or none (`vfs::mount::switch_roots`), logging
`BASE: generation <n> is the base, interface <i>`. A generation's number is
never reused within a boot and is the high half of each inode it serves, so a
descriptor, a mapping or a program of one never reads another's bytes; an open
whose walk began before a switch keeps the generation it found, and exec holds
the files it reads open. The booted generation stays for the boot: it costs
no copy, and what init made at boot, the consent pair among it, names its
programs. Any other no longer current is freed by the `basefs-reap` thread
once two passes a second apart find nothing holding it — an open file, a page
set, or a live process whose program it is — which reads those tables rather
than counting references beside them; a socket's captured identity holds
nothing, and one whose generation has gone reads as no program.
`base_monitor()`, close-on-exec, turns readable at each switch and reads the
current number, which `slopos_userland::base_watch` pairs with the manifest
it read before. Each of
`slopos_abi::fs::BASE_DIRS` — `/bin`, `/sbin`, `/lib`, `/usr/bin`,
`/usr/share`, `/etc/ssl` — is mounted from it over whatever `/` is, the disk
once `fs init` has made it the root or the RAM root, which holds only what the
archive carries outside them: read-only and `MOUNT_PINNED`, so `umount(2)`
refuses it and `mount(2)` puts nothing at or beneath it (`EPERM`). A directory
on the way to one, `/usr` or `/etc`, is held by identity rather than by path:
no mount of its filesystem may rename or remove it (`EBUSY`), and a root that
has a symlink there gets no base and fails its boot. The disk keeps what the
machine wrote — `/etc`, `/var`, `/home`, `/src` — and `/usr/local`, where the
toolchain and whatever the guest installs for itself live, live and without a
reboot. `just boot`'s and the self-hosting root are built `FS_BASE=boot`,
carrying the base directories only as sealed mount points; the tests and
verified images carry a system of their own, which a disk root's base
covers.

**An extension is shown beside the base.** `ext_activate(name, fd, digest)`
(`ExtensionUpdate`, which init, privd, the utests and the mechanisms of
privd's extension actions hold) serves the image in the file `fd` reads — an
ext4 volume followed by a SHA-256 tree in dm-verity's layout
(`slopos-verity-core`), `digest` the tree's root — read-only and
`MOUNT_PINNED` at `/opt/<name>`, in place of the activation shown there in one
step under the mount table's lock (`vfs::mount::mount_replacing`), keeping the
mount's id; `ext_deactivate(name)` takes it away (`fs/src/extension/`).
Nothing is copied: a read-only block device over the file
(`extension::device`), which holds it open, checks every block it reads
against the tree, keeping the sixteen hash blocks it checked last, so a block
the tree refuses fails the read that meets it with `EIO`. The volume must say
it holds exactly the tree's data blocks, carry ext4's `read-only` feature and
need no recovery; it is mounted through an `Ext2Mount` of its own that keeps
no name cache, looks for no CRC trailer and caches from 64 frames up to the
volume's size (`READ_ENTRIES_MIN`), and walked once at activation into the path index
`SO_PEERPROGRAM` answers from (`extension::index`), which refuses an inode at
two names, a name no path holds or held twice in a directory, a `.` or `..`
that lies, an entry whose type is not its inode's, anything but a directory,
regular file or symlink, directories larger together than the volume and
more than `MAX_ENTRIES` entries. Its locks nest as the layering does: an activation's,
then its device's (`EXT_VERITY`), then the filesystem the file is on. A name
is lowercase letters, digits and `-`, at most 32 bytes, none of
`slopos_abi::extension::RESERVED`, and `extctl` installs none of
`slopos_extension_core::TAKEN` either, the names whose `/var/log`, `/run` or
`/etc` directory the system's own paths hold, though it still shows, stops
and removes one installed before its name was taken. Each activation is an instance
of its own from a pool of `MAX_ACTIVATIONS` (32), numbered across every
activation and never within a boot again; the number is the high half of
each inode it serves, and `statfs` names it `EXTENSION_MAGIC`. It carries the
name and the root its caller vouched for, its label, which `SO_PEEREXTENSION`
answers. The kernel checks no signature: the caller holds `ExtensionUpdate`
and has checked the image, and the kernel serves only the bytes that root
vouches for. One replaced or deactivated is freed by the `basefs-reap` thread
once nothing holds it, as a retired base generation is. `/opt` is made where
the root lacks it and held by identity, as `/usr` is for the base; a mount
there that is no activation is neither replaced nor taken away (`EBUSY`,
`ENOENT`), and the mount table grew by the pool's 32 mounts. Interface 5's
`ext_activate`, which copied an archive from the caller's memory, is retired:
its slot answers `ENOSYS` and is never given to another call. Under
`extensions=off` (`BOOT_FLAG_EXTENSIONS_OFF` in `sys_info`) `ext_activate`
answers `EPERM`. Its log lines are `EXT: activation <n> is <name>`, `EXT:
<name> deactivated`, `EXT: activation <n> freed` and `EXT: image block <n>
fails its tree`.

**An extension goes in by its owner's word.** `/bin/extctl` (privd's
`org.slopos.extension.*` actions, run as uid 0 with `ExtensionUpdate` and
`Seal`) installs an image `slopos_extension_core::image` holds to a base's
shape: a volume sealed by its `/.extension/manifest`, its paths those they
have below `/opt/<name>`, under `bin/`, `libexec/`, `lib/` and `share/`
alone, naming itself in `/.extension/release` and what it asks for in
`/.extension/declare` (`slopos_extension_core::declare`). The volume is the
ext4 `slopos_extension_core::volume` writes — `ext4-core`'s profile less
the journal, plus `read-only`, every file an extent tree, inodes numbered
breadth first in name order, its UUID from the manifest's digest — and its
reader believes no other: a feature or inode flag that changes what a block
means, an inode type, generation or hard link it does not write, a block the
kernel would not serve, a checksum that fails or a block the tree refuses is
no image, so what extctl reads is what the kernel's index serves. An image of the archive form extensions took before
is refused, saying to install it again, and a floor such an image set binds
its version and not its bytes while the store holds it, so the same version
goes back in rebuilt. A signed install whose sidecar names the bytes its
record runs, or one its record refuses whatever the image holds, is answered
before the image is read. What it is
granted is the capabilities its services and actions hold, the actions a
person reaches unasked (`allow user`, `user alone`, `admin` or a `program`)
and those the paired agent does (`paired`),
its users and its commands, none of which another extension declares. A
first install's command line names the image's root and that grant, which
`extctl install <image>` writes out in the requester and the person approving
it reads, and the mechanism runs only that form, refusing an image that asks
for other than the line; the requester first holds every request to the
store as it reads it, so the person is asked only about what would go in.
`install-signed`, which the session runs unasked, takes an image whose
sidecar — name, version, root — a key in the store's keys signed with
`ssh-keygen -Y sign -n extension@slopos`, and holds the grant to that key's
ceiling and the unasked actions to its `user` ceiling; it leaves an extension
the person disabled disabled, and puts back one the person removed only as a
first install. Every grant sits under the base's ceiling,
`/usr/share/slopos/privd/extensions` (`assets/privd/extensions`), which never
names what `ceiling::NEVER` lists. A program the declaration names lives
outside the image's `lib/`, and every ELF file outside `lib/` is a program:
linked through `/lib/ld-slopos.so.1` against `libc.so` and libraries in the
image's `lib/`, found through soname links there, binding only what they
export (`slopos_extension_core::symbols`), so a static PIE is refused. A
symlink's target is relative and never climbs through `..`, so it stays in
the image, and `tools/extension` packs git's modes — 0755 for a directory or
an executable, 0644 otherwise — so an image is its tree's and not its
umask's. A record keeps a version floor — an older version is refused, the
same one with other bytes too — and the key whose updates go in unasked; any
other update is a first install. `rollback` makes the previous image current
once it still fits, lowers the floor to it and refuses the version it left
to a signed install until a newer one goes in, and `remove` keeps the record.
Every activation (`enable`, `rollback`, `activate-all`, and a key's change,
which holds what it signed to the keys anew) holds the stored image to its
record, its key and both ceilings again, and stops one that no longer fits,
saying why in its record; an update or rollback that will not activate
leaves the record as it was. The store is `slopos_abi::fs::EXTENSION_STORE`:
`keys.<n>`, and per extension `record.<n>` and its images named by root,
each copied with its holes kept, sealed and then held whole to its root —
a copy that is not the image is taken away — and one the store holds
already held whole to its root again before it is reused; an
`ExtensionUpdate` holder stores whole images, so no per-process `DiskBlocks`
ceiling binds it.
Only a sealed file is believed, and a writer seals each through the
descriptor it wrote and reads it back, held to one name, so `DacOverride`
without `Seal` neither rewrites a record nor plants a key; a record or the
keys are held to their owner and mode too, and the older generations go only
once the new one is so held, so one a `DacOverride` holder spoilt before its
seal is passed over for the last one a writer finished. Nothing is
renamed: a record or the keys go in as the next generation, the newest sealed
one is believed, and a writer then takes away the older ones and what the
record does not name, so a writer cut off anywhere leaves either. Writers
lock `EXTENSION_STORE.lock` (mode 0600) and ignore the termination signals
privd relays; readers take nothing. The kernel keeps it where it is
(`slopos_abi::fs::KEPT_DIRS`): once the root and the `mount=` volumes are
mounted, so a `/var` of its own holds the store, it makes the store and each
directory on the way to it and holds them by identity, so no rename or
removal moves one (`EBUSY`), and `mount(2)` and `umount(2)` refuse a target
at, above or beneath it (`EPERM`) — `/var` itself among them. The mechanism reads the
files a request names as uid 0 without `DacOverride`, so only what uid 0 may
read by mode goes in. A RAM root keeps a seal for good, so extensions need a
disk root. `just extension DIR` packs one on the host (`tools/extension`),
signing its sidecar with `EXTENSION_KEY`, and `scripts/remote.py install
--extension` installs it on a paired machine, refusing a sidecar that names
another image. Init runs `extctl activate-all` at every boot, holding
`ExtensionUpdate` and `Seal`, before privd and unless `extensions=off`; every
other change a mechanism makes, it tells init through `/run/init`'s
`extensions`.

**An extension's services run as its own users.** A record keeps a uid for
its extension's own name and for each user its images are granted, the lowest
of 100 to 999 no record holds, given at install and kept for good, its
removal included, so what a user left keeps its owner
(`slopos_extension_core:: accounts`); the uid of the name is the extension's
number. The user database lists each installed extension's users after the
people, each with a group of its own, written after every change through a
file sealed and renamed over the old one unsealed for the rename (*The
machine knows its people*); slibc's readers (`slibc/src/pwd.rs`) believe
`/etc/passwd` or `/etc/group` only while it is a sealed regular file and
answer the base's entries otherwise, so `DacOverride` without `Seal`
changes no name, and init and privd take uids from records, never from a
lookup. Init (`userland/src/apps/init/extensions.rs`) runs each service of
every enabled, running extension, as the activation shown at `/opt/<name>`
declares it and as long as that is an activation's mount, opened without
following a link, whose release and grant are the record's: its program once
the file opened there just before the start, which then names it by path, is
of the activation its declaration was read from, as its inode's high half
names it, as its user (a session service as the person, while the desktop session
runs), holding what it declares within the grant and the base's ceiling, with
`/dev/null` for input, the readiness notifier on fd 3, the sockets it
`listen`s on from fd 4 (`LISTEN_FDS`, `LISTEN_FDNAMES`), cwd and `HOME` its
state directory when it declares one (the person's home for a session
service, `/` otherwise), and `PATH` `/bin:/sbin:/opt/<name>/bin`. Each
carries a login tag of its own, `init_control::service_tag` of its
extension's number and its place among at most 16 services: when its process
exits, `SIGTERM`, `SIGCONT` and then `SIGKILL` end what else carries the tag
before it starts again as `restart` says (`on-failure` unless it exited 0),
within the five-a-minute budget, a start that fails waiting a second and five
in a row waiting for a change, which the next `extensions` asked of it counts
as, across a re-execution too; a session's end ends the session services'
tags with the desktop's. A service starts once each service it is `after` has
written its notifier, closed it while running, run thirty seconds or will not
start, and once nothing being ended carries its tag or holds its sockets.
Init binds a TCP port on every address with `SO_REUSEADDR`, which the kernel
lets only another listener refuse (`tcp::listen_reusing`, as Linux does), so
connections an earlier start left in `TIME_WAIT` do not; a UDP port without
it, which then shares with no socket (a UDP address, or a wildcard and an
address beneath it, is shared only among sockets that all set `SO_REUSEADDR`,
the newest receiving, as on Linux, and a bind to port 0 takes an ephemeral
port at once); or `/run/<name>/<socket>` mode 0666; before the first start,
and keeps it while the service is to start again, across restarts and
re-executions, so no other process takes it then, giving it up when the
service exits not to start again or gives up. Two services of one extension
never `listen` on one socket, and of two extensions that would, the one of
the higher number runs no service; nor does an extension whose services would
take init past the 128 of its descriptors it gives services, four each and
one per socket. A connector to a service's socket reads init's identity,
which recorded it at `listen`, through `SO_PEERCRED` and `SO_PEERPROGRAM`,
not the service's. Its output goes to `/var/log/<name>/<service>.log` through
a pipe to a copy of init (`/sbin/init --log <service>`, login tag
`init_control::LOGGER`) that runs as the extension's first user, holding
nothing, writes only to a regular file and turns it into `<service>.log.1` at
256 KiB, writing nothing past that when the rename fails. `/var/log/<name>`,
and `/var/lib/<name>` and `/run/<name>` as declared, are made for that user,
mode 0755, and one there that is anyone else's stops the services. On
`extensions` init reads the store and the activations again and answers once
no service is ending: it leaves an extension whose record or activation it
cannot read as it stands, stops what is gone, starts what is new, and
replaces a service whose declaration, user, grant or tag changed, whose
program, `reads` or the image's `lib/` reads differently in the new
activation's manifest (`boot_core::base::changed`), or whose `reads` of
`/etc/<name>` hold other bytes than it started on, handing on the sockets
both listen on and the log, and leaves the rest running on the activation
they started from; a new base whose ceiling changed does the same. What it
could not read it reads again a second on, then twice as long each time, up
to a minute. A re-execution hands the services on in as many pipes as they
fill, named after the state's, `--resume <state> <units>,…`, to an image
whose text holds their header line (`extensions::CARRIED`); before one that
does not, init ends every service, and an image that cannot read them closes
the descriptors their first line names: each service's tag, whether it was
up, starting, due, meant to stay down or given up, the pid it was started as,
its starts, a digest of what it was started as, and its sockets, notifier and
log pipe. The new image takes over a service whose pid still carries its tag,
or exited unreaped, as it stood when the digest still matches what the
records say, starts again one whose does not or that it knows nothing of, and
ends what carries a service's tag no service claims, those of an extension it
cannot read among them, until it reads them again. The loader searches
`/opt/<name>/lib` after `/lib` and `/usr/lib` for a program the kernel
attests is a file of `<name>`'s activation, which it asks through a socket
pair's `SO_PEEREXTENSION` the first time a search gets there, so an
`AT_SECURE` service finds its own libraries and nothing a path names.

**An extension's actions run as its first user.** privd reads, at its start
and again on `SIGHUP`, the actions of every extension that runs (enabled,
stopped by no check, installed) as the activation shown at `/opt/<name>`
declares them and its record grants them (`slopos_userland::activation`),
beside the base's policy, as `ext.<name>.<action>`, and each request takes
them as they stand when it comes; an action of one that clashes with the
base's, or names a dynamically linked base program as a principal, is left out
alone. Its mechanism is the extension's program, run as the extension's first
user with no supplementary group, holding what it declares within the grant
and the base's ceiling, with `PATH` `/bin:/sbin:/opt/<name>/bin`, once the
file opened just before the start is of the activation its action was read
from; init hands privd `SetId` beside `DELEGABLE` for that, and `ProcSignal`
to relay a requester's signals to a mechanism running as another uid
(`slopos_priv_core::PRIVD_CAPS`); no action names either, and privd signals
only the mechanisms it started. A mechanism carries its extension's login tag
(`init_control::action_tag`) rather than privd's, which a privd started again
counts with what its predecessor started, and init ends whatever carries the
tag of an extension whose actions no longer run, as it ends a service, but not
one whose record or activation it cannot read, nor one running again. What it
holds is also within `DELEGABLE`, all privd can give. Its `allow` lines,
levels and `keep` are privd's own (`user`, `admin`, below); `paired` is
privd's, and `program` a base program's path or one of the extension's own,
which matches
only a requester running that file of the activation whose label
(`SO_PEEREXTENSION`) names the extension and the digest its record installed,
and never `root`. privd reads a requester's label before its program, so one
that lapses between the two reads is no program; uid 0 running an extension's
program is no `root`. The question names the extension after the action's
`describe`. Init remembers what the actions privd read were read from — each
extension's name, image digest and activation — and on `extensions`, and
whenever it reads the store again, sends privd `SIGHUP` when that changes,
once a privd it started has said it is ready, so privd never leaves its socket
for an extension and an exclusive group stays its own.

**An extension's commands, apps and configuration are the owner's grant.**
`/opt/bin` holds a link to `/opt/<name>/bin/<command>` for each command of
every extension shown, which `extctl` keeps after every change (`extctl::
commands`), touching nothing there but its links into `/opt`; it is last on
`DEFAULT_PATH`. The dock reads each shown extension's `share` before the
base's, so an extension's app entry replaces none, and shows of it only the
apps it declares that no extension before it in name order showed.
`/etc/<name>`, a sealed uid-0 directory of sealed files read by everyone,
holds what the machine changed of an extension's configuration
(`slopos_extension_core::config`), for an extension that declares `config`,
one file deep and at most 64 files, laid out empty and sealed before its first
activation runs, mode 0755; at each activation and install anything unsealed
in a sealed one — left by a write cut short, or put there while a write held
it open — is taken away, as is a write's temporary file, while anything else
unsealed, which may be the owner's, is refused, and a file another process
changes as it is written is taken away rather than left unsealed: `extctl
config edit <name> <file>` edits a copy as sudoedit does, as uid 0 with an
editor of `/bin` or `/sbin`, and installs what changed through
`org.slopos.extension.config` (`config install <name> <file> <sha256>`, the
file the requester's standard input, so the mechanism installs only what the
requester could read; `Seal` alone, the person asked), and `extctl config
apply <from>` installs, unasked through `org.slopos.extension.config-signed`,
a file whose sidecar `<from>.meta` — name, file, serial, SHA-256 — the key
that installed the extension signed with `ssh-keygen -Y sign -n
extension-config@slopos`, while the owner trusts that key and its serial is
past the last, which the record keeps (`config <n>`) before the file goes in.
Init restarts each service whose `reads` of `/etc/<name>` hold other sealed
bytes, and holds it to no file no `Seal` holder put there. `extctl --root
<dir> status|enable|disable` acts on the store of a root mounted at `<dir>`,
as the rescue shell runs it holding everything; no privd action takes
`--root`.

**A base names the interface it needs; a kernel names the ones it serves.**
`slopos_abi::BASE_INTERFACE` numbers the private call surface: every private
syscall's number and the signature its doc states, and every structure and
constant those calls take. `scripts/check_base_interface.sh` holds that
surface, as rustdoc's JSON describes `slopos-abi`, to the digest recorded for
the number in `scripts/gates/base-interface.txt`, so the surface cannot change
while the number stays. A kernel serves `BASE_INTERFACE_MIN` through
`BASE_INTERFACE` and says so twice: in `sys_info`, which the shell's `info`
prints, and in the ELF note `.note.slopos.base-interface` (owner `SlopOS`,
type `NT_SLOPOS_BASE_INTERFACE`, `slopos_abi::base_interface`), readable from
the file without booting it; `link.ld` fails a link that lost it, and
`boot-core::kernel` finds it by name through the section headers. A base
carries `/usr/share/slopos/base-interface`, the number it was built against,
and `/usr/share/slopos/base-manifest`, `<sha256> <mode> <path>` for every
record but itself in archive order, so two bases of one manifest hold one
tree, which `tools/initramfs` writes; the packer holds what it writes to
`boot-core::base::check` — every name canonical (`/` and its components) and
once, none beneath a file, each base directory and every directory on the
way to one a directory, `/tmp`, `/run` and `/dev` directories with nothing
in devfs, no symlink the kernel refuses, at most 2048 records, an interface,
and the manifest — which `bootctl base install` and `bootdisk base` hold an
image to as well, so a base the kernel would refuse to index, mount or
unpack after freeing the other never reaches the boot partition. The minimum
rises only when a change reshapes something an older base calls: a call
taken away keeps its slot and answers `ENOSYS`, as interface 5's
`ext_activate` does, so an older base still boots, without what that call
did — an interface-5 base on a newer kernel activates no extension, and its
`extctl` stops each at boot, which `extctl enable` under a newer base brings
back; rolled back to while the system runs, its init and privd read the
newer activations as nothing shown and wait for them. A program outside the base reaches SlopOS through `libc.so` alone,
which only grows: `scripts/check_libc_exports.sh` holds its dynamic symbols
to `scripts/gates/libc/exports.txt` and the layout of every struct and union
`slibc/include` defines to `scripts/gates/libc/layouts.txt`. The compositor
serves `PROTOCOL_VERSION_MIN` through `PROTOCOL_VERSION`: its `Hello` names
the newest, a client's first request is a `Hello` naming the version it
speaks, which the compositor answers in or refuses with `VersionMismatch`
before closing, and a client accepts any compositor at its version or newer.
The two `Hello`s never change encoding, and raising the minimum is a
decision with a stated reason.

**The apps live outside the base.** The editor, the file manager and the
image viewer are `slopos-apps` (`apps/`), built as `cargo build` builds a
program for this target — the target's std and panic strategy, linked
dynamically against `libc.so` by the line the clang driver writes — and staged
by `build_userland.sh` with their app entries and icons as `builddir/apps`:
the tree `apps`, which every root holds at `/usr/local` beside the toolchain's
`usr_local`. `scripts/check_loose_programs.sh`, after every host userland
build, fails one that is static, names another interpreter, lacks `libc.so`
in `DT_NEEDED` or holds a `syscall` instruction. The toolkit under them
(`appkit`, `windowing`, `slop-protocol`) reaches the system only through
`slopos-cabi`, safe wrappers over functions `libc.so` exports — never slibc's
raw calls or the ring — so the same code links into a static base program
against slibc's rlib. A tree's manifest is keyed by its name
(`tree-core::{TOOLCHAIN, APPS}`), names where the tree went (`at`), so an
install elsewhere is refused, and belongs to whoever installs the tree again:
the toolchain's to uid 0, the apps' to whoever holds `/usr/local`, which the
first person rewrites in place in uid 0's directory while they do, and where
init makes it for them on a root without one. A manifest neither uid 0 nor
the installing uid wrote is not read, and
one that does not parse — an in-place rewrite cut short — is not believed:
either stands for the new tree unfinished, whose paths the install takes back,
so the installer never acts on what the session wrote. An install skips a tree
whose believed manifest records its identity at that destination with every
entry still of the kind and size it names; no content is checked, so a
reinstall keeping the root leaves what the session changed in place inside
its own trees. Two trees at one destination stay
disjoint: a file one holds refuses the other's install of it. On a host-built
root the host keeps the record that counts, and the guest's copy takes its
tree's owner.
The apps reach a root four ways: the image builders install them as a host
tree; the install medium carries them at `apps/` (`layout::MEDIUM_APPS`) and
the installer installs them; init installs them into a live boot's RAM root;
and `scripts/selfhost.sh install` installs them with `/bin/tree-install`, a
base program over `tree-core` that writes as whoever runs it and refuses a
tree whose manifest another uid keeps (*Software is installed for everyone in
`/usr/local`*, below). The apps' manifest is computed from the tree where
it is installed (`slopos_userland::tree::scan`), its identity a digest of every
entry; the toolchain's comes with the payload, written by the host. The
dock pins the app entries — `share/slopos/apps/<id>.app`, `key=value` lines
naming the `id`, a `name`, the program it `exec`s and an `icon` PNG in that
share's `slopos/icons/` (`slopos_userland::app_entry`) — read at session start
from `/usr/share`, `/usr/local/share` and `~/.local/share`, a later one
replacing an earlier with its id as the XDG data directories rank, in name
order. The shell's program registry names no app; `PATH` finds them. The
loader searches `$HOME/.local/lib`, then `/usr/local/lib`, after `/lib`,
`/usr/lib` and an extension program's own `lib`, only for a process that is
neither `AT_SECURE` nor uid 0's, so nothing a person puts in either reaches a
program holding more than they do.

**The guest installs what it builds.** `/bin/bootctl` (privd's
`org.slopos.boot.*` and `org.slopos.base.*` actions, run as uid 0 with
`RawBlock` for the raw disk and partition and `Power` for the loader's
variables; `collect` is init's) finds
the boot
partition by its type GUID on the disk whose GPT lists `LoaderDevicePartUUID`,
the ESP Limine was started from, and writes a kernel into a slot
that is not the default, clearing a one-shot boot armed for that slot first,
once the kernel's note says it serves current's or previous's interface,
through `fat-core`, a FAT32 implementation whose every file write is
copy-on-write — the new contents go into free clusters and a single
directory-entry store commits them, so no kernel or base is ever half
replaced; each file is read back. `bootctl base install <image>` takes an
image that checks out and makes it current, moving current to previous first
only when current is the running base — its manifest is the one at
`/usr/share/slopos/base-manifest` and the system booted from that disk — so
previous always holds the base that last ran, as FreeBSD's `installkernel`
moves `kernel.old` only off the running kernel; it refuses a base that would
leave the kernel of the default slot, the running one or the one armed to
boot once serving neither base,
or that with the base kept beside it takes more than half the machine's
memory, since every boot loads both. `bootctl base rollback` makes previous
current and current previous; writing both, either writes first over the
image the default kernel does not need, and ignores the termination signals
privd relays once the writes begin, so no request cuts the pair. `bootctl
install` and `base install` take the image's SHA-256 after its path and then
install only those bytes, which `remote.py install` passes for what it
pushed, since the session may replace the file; `set-default` takes only a
slot whose kernel serves a base on the partition. A base installed or rolled
back to runs at once through `BaseUpdate` when the system booted from that
partition, the running kernel serves its interface and that interface is 3
or newer, whose init takes over a running system's, and otherwise at the
next boot, and says which (a session's install or rollback asks the person,
since either replaces the running base); `bootctl status` names
each base's interface, build tag and whether it runs. It then sets
`LoaderEntryOneShot` for a kernel, which Limine consumes on the next boot, and after that
boot `bootctl commit` writes the entry Limine reports in `LoaderEntrySelected`
to `LoaderEntryDefault`. Nothing it does writes the ESP. The default a boot
with nothing armed takes is the entry that variable names, or Limine's first
when it is unset; a value naming no entry Limine offers (`LoaderEntries`)
leaves the default unknown, since Limine also resolves menu paths there, and
`bootctl install` refuses until `bootctl set-default` names one. `bootctl
spare` names the slot that is not the default. A slot that
panics resets under `panic=reboot`, and the reset lands on the old default.
Block device nodes accept reads and writes from a `RawBlock` holder, each write
through the claim on that disk or partition, so a mounted one answers `EBUSY`
while its siblings stay writable. UEFI variables are read and written on a
kernel thread — the firmware is mapped only into the kernel master address
space and may use the vector registers. In the guest, `scripts/selfhost.sh install` builds the
userland and the base with the toolchain on `PATH` and installs the base,
which runs at once when the running kernel serves it and which `bootctl base
rollback` takes back; `scripts/selfhost.sh install-kernel` builds the kernel,
puts it into `bootctl spare`'s slot and arms the one-shot boot, so the kernel
is tried and committed or rolled back. A base that needs a new kernel goes in
first and runs from the boot into it. The apps are no slot's: they go to
`/usr/local` at once when the running `libc.so` is the one they were built
against, through privd once it is the system's, and otherwise by the command
the script prints for the new boot, and a rollback keeps them.
`selfhost_test` and `install_test` run those as a person at the shell does
(`slopos_userland::selfhost::selfhost_install` runs both), so the loop a
developer types is the loop the tests grade; a root test
hands the script a privd connection of its own, so its `bootctl` calls are
the test's and wait for nobody at the machine.

**An install medium installs SlopOS.** The live ISO is the medium: `just iso`
builds `builddir/install.cpio` (`scripts/build_install_medium.sh`), which
Limine loads as the module `install` beside the base, holding Limine for the
ESP and the source tarball of every recipe the base takes programs from. The
kernel serves it read-only and pinned at `/media/install`
(`layout::MEDIUM_DIR`), a second `basefs` instance where Limine put it, with
the kernel and base it booted beside it at `boot/kernel.elf` and
`boot/base/current.img`, as the ISO itself stages the base. The medium records the GPT disk GUID its image was built with
(`boot/disk-guid`, `layout::MEDIUM_DISK_GUID`), which `build_install_medium.sh`
chooses per build and `build_iso.sh` hands xorriso as `--gpt_disk_guid`; read
from the header alone (`gpt::disk_guid`), it names the medium the system
booted from however the firmware started it, whatever array the header
describes.
`PAYLOAD=1` adds the payload, which Limine never loads: an ext4 volume in the
profile holding the toolchain at `usr/local` with the manifest an install
records for it, and at `src/` a `--vendored` clone of `HEAD` whose origin is
GitHub, shrunk with `resize2fs -M` and given ext4's `read-only` feature, which
`mke2fs` refuses at creation, so no system that mounts it writes it.
`build_iso.sh` appends it as a GPT partition of SlopOS's payload type
(`layout::PAYLOAD_TYPE`) and holds the image, through `bootdisk medium`, to
`boot-core`'s GPT reader finding it there byte for byte; that reader stages
xorriso's 176-entry table (`gpt::MAX_ENTRIES` is xorriso's most, 248). The
`install medium` boot step mounts it read-only and pinned at `/media/payload`
(`layout::PAYLOAD_DIR`): the one partition of that type on the one disk whose
GPT carries the medium's disk GUID (`block::locate_partition`), chosen once
USB has settled so a second disk carrying it is seen, under a read claim held
for the boot that keeps any table re-read off the stick, on one of the four
pooled ext4 instances. It is the one disk `root=initramfs` mounts that no
`mount=` names. A medium the kernel cannot read — a CD, an image copied as a
file — has no payload, and the installer installs without the toolchain,
saying so when the medium's disk names a payload the kernel did not mount; it
takes a payload only from the root of a read-only ext4 volume at
`/media/payload`, since any program can make a directory there.
`/bin/installer` (privd's `org.slopos.install`: `RawBlock`, `Mount`, `Power`,
`BootEntry`, `Seal` and `DacOverride`, for the session only once the person
allows it and never for a paired host; a `RawBlock` holder may write any disk
raw, so no per-process `DiskBlocks` ceiling binds it, and it fills a whole root
in one process; its questions are asked through the requester's terminal) asks
for a whole disk —
never one that answers `BLKROGET` with 1, nor the one carrying the medium's
GPT disk GUID, which `installer_test` excludes by the same rule
(`boot_disk::not_installable`) — and a mode — erase it, install
into free space beside what it holds, or reuse a partition as the root — or
takes every answer as a flag. Its engine (`installer::engine`) probes the
medium and every disk, proposes what a set of choices installs and installs
it, printing nothing: each step, each copy's bytes against its total and every
line e2fsprogs prints are events, which the CLI (`installer::cli`) prints. An
install reads the chosen disk again and writes nothing when what it would do
differs from the proposal confirmed, and keeps its steps and log lines on the
root as `/var/log/installer.log`: once the root is laid out, again before the
check, which covers it, and after a failure between the two. `installer
--serve`, which runs only as privd's `org.slopos.install` and is what a
window asks privd for, drives the same engine over the frames
`installer_core::protocol` states on its standard input and output — probe,
open, propose, install — answering each request in turn: it installs only
the proposal it made last, for the same choices, writes nothing when its
input ends before an install, serves on after a failed install once probed
again, and exits after one that succeeds. `installer-core`, host-tested
under `just test-host`, is what the engine and a driver agree on: the
machine and each opened disk as views, every choice on an opened disk
proposed with the engine's defaults, the proposal — the disk before and
after, an action per partition tagged erase, new, format, share, keep or
reuse, the root's size range — its issues worded as the CLI says them, the
events, the protocol's codec, and the `Wizard` a window's pages follow.
`boot-core::install` plans where the partitions go, on 1 MiB boundaries,
sharing the disk's ESP and keeping SlopOS's own on a reinstall, a new root as
large as `--root-size` asks clamped between the medium's least and what the
region gives, the crash partition after it and the rest left free;
`BLKRRPART` re-reads the table it writes. It formats new FAT32
volumes with `fat-core`, which creates long names, the root with
`/sbin/mke2fs` in the profile, and zeroes a new crash partition; seals the
root's base mount points; installs the medium's toolchain and apps at
`/usr/local` by the rule host trees follow (`tree-core`: what the manifest
under `/var/lib/slopos/trees` names is replaced, what the user added kept), as
whoever a kept root's owners record says holds it, and
seeds `/src` once, its origin
where the user says; writes a kernel into both slots, the base as current and
previous, removing a slot's `base.img` a disk laid out before kept, and `limine.conf` — `root=PARTUUID=`, a
five-second menu, an `efi_boot_entry` for each other system `BootOrder` lists
that Limine can boot and the install leaves in place — makes `slopos-a` the
default, holds the root to
`e2fsck -fn` and last registers the firmware entry, first in `BootOrder` only
when asked. It writes nothing outside the partitions it made or was given, nor
anything on a shared ESP outside `\EFI\SlopOS\`. e2fsprogs is the recipe
`toolchain/recipes/e2fsprogs`, built unpatched; its six programs are in every
base at `/sbin`, holding nothing a spawn does not delegate: the installer hands
each the `RawBlock` it formats or checks with, and nothing else does.

**The medium repairs an installed system.** `shell --rescue` asks privd for
`org.slopos.rescue`, the one `medium` action: `/bin/shell` as uid 0 holding
every delegable capability, for root and, once the person allows it, the
session, never a paired host; the kernel sets `BOOT_FLAG_MEDIUM` only once it
has served a medium Limine carried, so no command line names it. Exec keeps
only the universal capabilities, so the rescue shell runs each program by
spawning it with its whole mask from the process forked for it
(`slopos_userland::held`), which passes on a `SIGHUP`, `SIGINT`, `SIGQUIT` or
`SIGTERM` another process sends it, one the terminal raised having reached the
program's group already, and then ends as the program ended; its `exec`
builtin refuses. It takes `PRIV_ACTION` out of the environment, so a
mechanism it runs acts itself, and puts `SLOPOS_RESCUE` in, so a shell it
starts, a script's included, passes on what it holds the same way; its
`PATH` is privd's `/bin:/sbin`. `mount` and `umount` are
coreutils' in util-linux's spelling, a device's type ext4 unless `-t` names
another. `bootctl --boot <partition>` acts on another disk's boot partition,
whose loader has not run this boot: it offers what that disk's own
`limine.conf` renders (`limine::offered_entries`), it boots by default and
once what the machine's variables say, and the running root's records under
`/var/lib/slopos/slots` are not its. `installer_test` grades it from the
stick: `e2fsck`, a read-only mount by a nested shell, a base rolled back with
a default the medium's loader does not offer, and at a rescue shell's
terminal its size and a resize, an interrupt ending the program it ran, and
the requester's terminal restored.

**A panic leaves a record.** The boot disk's crash partition is a ring of
64 KiB slots in the format `boot-core::crash` states: a header sealing each
record's text with a CRC-32, then a summary of `key: value` lines, the panic
report and the kernel log's tail. In the drivers phase `drivers/src/crash.rs`
finds it on the disk whose GPT disk GUID Limine reports for the kernel, holds
it under a write claim for the boot, and reads what each slot holds once, so
the panic path only writes. The `limine` crate misplaces every `limine_file`
field after `media_type` and leaves its `Uuid` without `repr(C)`;
`boot/src/limine_protocol.rs` rebuilds the GUID from where its fields were
read. A fatal panic's report goes to serial and into a static slot-sized
buffer, which is closed with the kernel log's newest whole lines
(`klog_tail`, a `try_lock`) and written through the NVMe controller's
`PanicQueue`, polled, the chunk holding the header last, then flushed, before
`panic=reboot` resets; a disk on another transport keeps no record. The
format-free abort a lockup takes writes one too, from its message; one a stack
overflow takes, whose data stack is spent, does not. A recovered panic's report
also goes into the kernel log, so a later record carries it. The panic screen says what became of the
record. `/dev/crash` has a file per record, named by its sequence number and
`-torn` after it when its CRC fails; reading one and unlinking it, which erases
the record, take `RawBlock`, the raw-device right. Init runs `bootctl collect` before the
tests and the services: it copies each record to
`/var/log/crash/<index>-<UTC time>-<slot>.txt`, the index taken from
`/var/log/crash/bounds` before the copy, through a flushed sibling and a
flushed rename, notes the slot as crashed under `/var/lib/slopos/slots`, and
only then erases the record; it keeps the newest 64 and every copy a slot
names as its last crash, and refuses to erase anything while
`/var/log/crash` is on no writable disk. Last it notes the slot
this boot came from as up, keeping its last crash. `bootctl status` reports
each slot's last boot and last crash, and `bootctl install` clears both. A
record names its slot by the kernel's own path, `/boot/<slot>/kernel.elf`.
`panic=reboot` holds the panic on screen for ten seconds on bare metal and not
at all under a hypervisor.

**The guest speaks TLS 1.3.** `tls-core` is a sans-I/O client with every
primitive under it written here, `no_std` and `forbid(unsafe_code)`;
`userland::tls` wraps it in a blocking stream, and `curl` is its user. It
trusts `/etc/ssl/certs/ca-certificates.crt`, Mozilla's root store committed
under `assets/certs/` and replaced only by `scripts/update_ca_bundle.sh`,
which checks the digest curl.se publishes. `tls-core` and `http-core` are host
crates under `just test-host` because no userland unit test runs anywhere;
`tls-core`'s interop tests drive `openssl s_server` and `s_client` and
`ssh-keygen -Y` and need `openssl` and `ssh-keygen` on PATH. It verifies
Ed25519 (RFC 8032) strictly — a canonical key outside the small-order
subgroup, `S` below the order, `R` the canonical encoding of `[S]B - [k]A` —
and the SSH signatures `ssh-keygen -Y sign` writes over such keys; its signers
for both are under `test-server`, which nothing shipped enables. BLAKE2b (RFC
7693) and Argon2id (RFC 9106) hash the user database's passwords, held to
the RFCs' vectors, Argon2d and Argon2i compiled for the tests alone.

**The unwinder is not test-only.** `libc.so` and `libc.a` supply the Level-1
Itanium unwinder (`vendor/unwinding`, seventeen `_Unwind_*` entry points) on
every image, behind slibc's `unwinder` cargo feature which only those two
wrapper crates enable, and both are built `-C force-unwind-tables` because the
first frame of every unwind is one of their own. The cost is that the shipped
`libc.so` carries the DWARF reader — 45 904 bytes of a 571 696-byte library,
re-measured by building the cdylib with the feature off — which `--gc-sections` cannot drop while startup registers the frame finder
unconditionally. The base's static Rust binaries are unaffected: they take
slibc as an rlib with the feature off, and `userland/userland.ld` discards
`.eh_frame` outright. The apps outside the base unwind through `libc.so`'s.

`just boot-live` and `just boot-log` boot `builddir/slop.iso` with `BOOT_CMDLINE` as its command line — `root=initramfs` unless it says otherwise — plus `boot.debug=on` under `DEBUG=1` and `roulette=skip` under `ROULETTE=0`; `VIDEO=0` makes `just boot` and `just boot-live` serial-only.

**Every disk goes through the block layer.** `drivers/src/block` is what a
block driver registers its disks with: their Linux names (`vda` in virtio
probe order, `nvme<C>n<N>` for namespace `N` of the `C`-th NVMe controller,
`sda` for a USB disk, the lowest letter free, `nvme0n1p2`, `vda2` and `sda1`
for partitions), their partition tables, their `/dev` nodes and the claims a
mount or a raw write takes. A disk can leave: `unregister_disk` takes it and
its nodes out, and a claim carries its disk's generation, so one a departed
disk's mount still holds releases nothing of the disk given its name next.
A disk reports write protection (`EngineDisk::protected`), which its
partition nodes forward: `BLKROGET` answers 1 and ext4 mounts it read-only. A claim covers what it
names — a partition's excludes the whole disk and itself, so two partitions of
one disk mount side by side; a write claim is exclusive, and the read claims
read-only mounts take share with each other and keep a writer off — and
`BLKRRPART` re-reads a table only while nothing on the disk is claimed,
replacing its partition nodes; a node's inode is never reused, so a descriptor
on a replaced one fails rather than reaching another window. Every disk is an `EngineDisk` over the request engine
(`block/engine.rs`): request slots whose preallocated pages every payload is
staged through, a timeout that quarantines a request with its pages until the
device hands it back, and a fence that holds every write behind one a timeout
abandoned. A transport supplies only `QueueOps` — virtio-blk's descriptor
chains, NVMe's submission entries, Bulk-Only's commands — so the engine's
tests run on all three, and names how long a request waits for a free slot:
250 ms for NVMe and virtio-blk, a Bulk-Only command and its recoveries for a
stick. Spans
are cut into the device's logical blocks: partition tables count in them, a
partial block is read, patched and written back inside one slot's pages, and
ext2 refuses a block smaller than one, whose write would tear its neighbours.
devfs publishes `/dev/disk/by-partuuid`, `by-uuid` and `by-label` (ext2/3/4,
FAT and btrfs identities, udev's escaping), and `root=`, `mount=` and
`mount(2)` take `PARTUUID=`, `UUID=`, `LABEL=` and those paths as well as a
name; block nodes answer `BLKGETSIZE64`, `BLKGETSIZE`, `BLKSSZGET`,
`BLKPBSZGET`, `BLKROGET` and `BLKRRPART`. A volume's identity is read when
something first names the node by one — a listing, a lookup, a `LABEL=` or
`UUID=` — and again only once a write claim that wrote is released, which no
unprivileged task can cause; a probe the disk did not answer is retried after
five seconds.

**NVMe is the disk QEMU attaches.** `drivers/src/nvme` runs an admin queue,
one interrupt-driven I/O queue pair every namespace shares, and a polled pair
created with interrupts off and its pages allocated at probe, which a panic
takes whole with a `try_lock` that never waits (`PanicQueue`). It grants a DRAM-less
drive the host memory buffer it asks for, in `DmaCoherent` chunks, flushes
only a controller with a volatile write cache, and on poweroff and reboot
(`driver_core::shutdown`) takes the buffer back, deletes its queues and waits
for the shutdown notification to complete. Register, command and identify
layouts, and the host memory sizing, are `nvme-core`, host-tested under `just
test-host`, since QEMU's model asks for no host memory. `qemu_run.sh` attaches
the root as `nvme0n1`; test mode adds a scratch `nvme0n2`, a 4096-byte-block
controller carrying the labelled media volume (`nvme1n1`) and a scratch
(`nvme1n2`), a controller of its own a test shuts down (`nvme2n1`), and on
virtio-blk the verified image (`vda`) and a scratch (`vdb`), and on a
`qemu-xhci` at a fixed PCI address (`00:10.0`) a scratch stick (`sda`), so
every transport stays graded; the capacity volume is `nvme0n3`, and the boot
disk is the last controller's.

**USB is xHCI.**
`drivers/src/usb` binds every PCI xHCI controller (class `0x0C`,
subclass `0x03`, prog-if `0x30`) that offers MSI-X or MSI and has 64-bit
addressing, 4 KiB pages and at most 512 scratchpad buffers, up to eight; any
other is declined with its command register as found and stays the
firmware's, and one that would find no dynamic MMIO range left for its
registers, or for an MSI-X table when it has no MSI, is not touched. Probe
brings the function to D0, takes the controller from the firmware through USB
Legacy Support (OS Owned, a second for BIOS Owned to clear, else cleared here,
and the SMIs turned off and acknowledged either way), halts it, turns bus
mastering off and resets it, then turns bus mastering on, gives it single-page
rings and one event-ring segment on interrupter 0, runs it and proves the
rings with a No Op command, served by polling, and said so, if it never
interrupts; a failure past the handoff takes it off the bus and resets it, or
logs that it would not reset. `usb-core` is xHCI as data and as a register sequence — registers,
extended capabilities, TRBs, contexts in both sizes, ring bookkeeping and the
sequences over a `RegisterBus` — and the device half: the standard requests,
descriptors, hub descriptors and class requests, and `usb_core::bus`, the tree
of ports, hubs and devices as a state machine over a `Host` trait, all
host-tested under `just test-host` against a simulated controller with
simulated devices and hubs, since QEMU's reports 32-byte contexts, no
scratchpads and no legacy support, and its only hub is full-speed. The
interrupt handler drains the event ring; the `usb` thread serves every
controller when woken, at its tree's next deadline and once a second: it
drains the ring itself, halts and takes off the bus one that died (Host System
Error, Host Controller Error, registers all ones), and steps the tree — once
the `pci` step has probed every PCI device (`usb::start`) — one device in the
default state per controller: debounce, reset, Address Device, EP0's packet
size, the descriptors and a configuration, a hub's ports through hub requests
with its transaction translator, route string and depth named in its
children's contexts, and a port disabled until unplugged after three
failures. It logs each attach, detach, enumerated device (`USB: 1-4.1
46f4:0001 1 function, bound usb-test`), failure and removal, controllers
numbered from 1 in probe order and a hub port appended with a dot. Every
function of a configured device that is not a hub — an interface, the
interfaces an association groups, or a communications interface and the
interfaces its CDC union names after it — is offered to `UsbBus`, the
third bus, whose drivers register with `usb_driver!` in
`.usb_driver_registry`; probes and removals run on a second thread,
`usb-bind`, and a `BoundDevice<UsbBus>` vends control requests, page-sized
pipes, report endpoints that keep a transfer posted and hand each report to
the driver where the event ring is drained, control requests the `usb`
thread posts and collects on a later pass, another alternate setting of one
of its interfaces (a Configure Endpoint dropping the old setting's rings and
adding the new one's, then `SET_INTERFACE`), and bulk queues: buffers of the
device's own pages that a driver keeps transfers on without waiting, told of
each completion where the event ring is drained and of the ring running
again once the tree has recovered a halt. A device that leaves is marked gone, its endpoints stopped, its
transfers failed, its claims taken out of the `ClaimTable` (`release`, which
only USB calls) and their removals run, then its slot disabled and its pages
freed; a controller that dies, or leaves a command uncompleted for five
seconds, does the same without commands. Under `tests=on`
the `usb settle` boot step waits up to a minute for every port to be quiet and
every device to reach an end, and fails the run with `USB: unsettled`. The
kconsole command `u` lists controllers, ports, devices, drivers and endpoint
queues. A shutdown hook resets each controller at poweroff and reboot, once
its sticks are drained and flushed. `usb-hid` reads keyboards and pointers in
the report protocol through `hid-core`, a boot device in the boot protocol
only when its report descriptor cannot be read or names neither keys nor a
pointer, and each interface claims a keyboard slot, a pointer slot or both;
the `usb` thread keeps each keyboard's repeat and LEDs. `hid-core` is HID
independent of transport, host-tested under `just test-host` with mutation
loops: report descriptors parsed into storage the caller passes, the boot
reports, what a keyboard or pointer report says and the LED output report;
the I²C-HID touchpad parses through it too.

**A USB stick is a disk.** `usb-storage` binds Bulk-Only SCSI functions (class
`08/06/50`, which is a UAS device's alternate setting 0 too) and registers
as a disk, `sda` onwards, each LUN INQUIRY calls direct-access that is ready
within `usb.settle_ms` and counts 512-, 1024-, 2048- or 4096-byte blocks;
MODE SENSE's WP bit makes it write-protected. `usb-core::storage` is the transport as data:
the wrappers, the SCSI commands, sense and capacity, and `Transport`, a state
machine over the device's pipes and EP0 behind a `Wire`, host-tested against
the simulated controller with simulated sticks — stalls, Phase Errors, CSWs
that are not valid, a device that never answers, a slow command with another
behind it — whose bulk endpoints keep data toggles and fail a transfer whose
toggle does not match, since QEMU's `usb-storage` keeps none. Each stick is
a request engine of two slots whose `QueueOps` is that transport: one command
on the wire, one queued, cut at 120 KiB into READ/WRITE (10), or (16) past 2³²
blocks, with SYNCHRONIZE CACHE the flush until a LUN answers it ILLEGAL
REQUEST. The drain moves a command along where the event ring is drained;
`BoundDevice::stream` gives the driver its two bulk pipes, whose halts the
tree leaves to it, and the `usb` thread runs its recovery: a stalled data
stage or CSW cleared and the CSW read, a failed command's REQUEST SENSE
deciding retry or failure, and for a CSW that is not valid, a Phase Error, a
stalled CBW or a command past its 20 s deadline, Reset Recovery — Reset
Endpoint (Stop Endpoint on a pipe not halted), the Bulk-Only reset,
`CLEAR_TT_BUFFER` through a full-speed stick's transaction translator, the
halts cleared, and a Configure Endpoint dropping and adding both pipes, which
resets the controller's toggles and moves each ring past what failed — then
a block request again, once; a probe's own command that hung is not sent
again, and a LUN whose INQUIRY goes unanswered ends the scan, since probes
share the `usb-bind` thread. A recovery that fails fails every command and has
the tree reset the port and enumerate the stick again, which spends one of
the port's three tries unless the stick had run for a minute. A stick that leaves
unregisters its disks and fails what it held at once, so a mount on it turns
read-only through `errors=remount-ro`, answers `EROFS` and stays until
unmounted. At poweroff and reboot each controller's hook stops its sticks'
engines, drains them and sends each LUN with a cache a polled SYNCHRONIZE
CACHE before the reset.

**A USB Ethernet adapter is a NIC.** `usb-net` binds CDC-ECM and CDC-NCM
functions (class `02/06` and `02/0D`); the tree sets the first configuration
a driver matches, so QEMU's `usb-net` is taken in ECM, the configuration it
lists after RNDIS, which nothing binds. `usb-core::cdc` is the class as data,
host-tested with mutation loops: the functional descriptors, the MAC string,
the notifications, the ECM and NCM requests, NTB-16 written and read, and
when a transfer needs a zero-length packet after it. Probe declines a
function whose `wMaxSegmentSize` is under 1514 or whose MAC string is not a
unicast address, holds an NCM device to NTB-16 IN blocks of at most 16 KiB,
selects the data interface's setting with the bulk pair, asks for directed
and broadcast frames, posts four bulk-IN transfers and then publishes the
NIC, `eth1` beside a machine's own `eth0`. Netpoll takes completed transfers
in order and posts them again; `tx` copies the frame, or an NTB holding it,
into one of four buffers, follows a frame of whole packets with a
zero-length packet, and returns; the drain only wakes netpoll, and the tree
recovers a halted pipe as any other. Carrier is what the last
`NETWORK_CONNECTION` said (its `wIndex` unchecked: QEMU names the data
interface there), or always up without a notification endpoint. Removal
calls `nic::retire`.

**Every keyboard feeds one keyboard state, and every pointer moves one
cursor.** `drivers/src/keyboard.rs` takes `(source, usage, pressed)` steps
from the i8042, a fixed source, and from each USB keyboard, which claims one
of `MAX_KEYBOARDS`: a modifier is held while any keyboard holds it, the locks,
the layout and its dead key, the diagnostic console's trigger, the boot log's
Esc, Shift+PageUp/PageDown and the TTY fallback are the machine's, and a
press of a key its source already holds is delivered with
`KEY_FLAG_IS_REPEAT` and toggles no lock. A keyboard that leaves releases
every key it held. `ps2::keyboard` only decodes set 1 and owns the i8042's
LED exchange, which a lock change starts from any context and the i8042's
interrupt carries on, each ACK sending the next byte and a RESEND, or 250 ms
of silence noticed at the next byte, lock change or `usb` pass, sending one
again, twice at most, so nothing masks its lines or polls port 0x60 after
boot; each USB keyboard gets an output report from the
`usb` thread. `input_event` owns the cursor and the screen bounds
the video layer publishes: a pointing device claims a `PointerSource` and
reports a `hid-core` `Motion`, relative axes moving the cursor and absolute
ones mapping their logical range onto the screen when they change, and a
button stays down while any device holds it; a report changes only the
buttons it carries.
The PS/2 mouse and the touchpad move the same cursor as USB mice and
tablets. `usb-hid` reads keys only from Keyboard and Keypad collections and
motion only from Mouse and Pointer ones, so a game pad binds as neither.

**Every NIC goes through `nic::publish`.** The stack owns the netpoll and
net-timer kernel threads, which `nic::init` starts at boot before PCI probe. A
NIC driver supplies a `NetDevice`, an interrupt handler that only calls
`napi::wake_napi`, `rx_pending` (receive work an interrupt raced in after a
poll) and `sample_carrier`, and calls `nic::publish`, which gives the device a
registry slot, an interface, a place in netpoll and in the net-timer's carrier
sampling, and a DHCP client, whose first DISCOVER waits for carrier; no driver
keeps a thread of its own. `nic::retire`, which a USB NIC's removal calls,
undoes all of it: the DHCP client stopped, its routes, neighbours and
interface gone, `set_down` run and the slot freed, so the interface name is
the next NIC's. Since a device plugged in gets a DHCP client unasked, an offer
or ACK whose mask is missing, has holes or is under /8 is not taken, a default
route's metric is 100 plus its interface's index, and the resolver is the
lease of the lowest-indexed interface that names one, the others' offered
again when it goes, so an adapter plugged in later takes neither the default
route nor DNS. A frame takes
its Ethernet source from the device its route leaves on (`ipv4::send` stamps
it), an ARP its sender address from that device's address, and a neighbour's
queued packets leave on the neighbour's device. On any device but loopback,
`ipv4::admits` drops a 127/8 source or destination, a broadcast, multicast or
unspecified source, and a unicast destination that is no address of the host
(any interface's address but a host-scoped one counts: the weak host model),
except an unfragmented UDP datagram to port 68 on a device that holds no
address yet, which is how a DHCP server that ignores the broadcast flag
delivers its offer; an echo request to a broadcast or multicast address
goes unanswered, and `ipv4::send` refuses a 127/8 source on such a device.
ARP refreshes the entry of any sender it already knows and creates one only
for an ARP aimed at the receiving device's address (RFC 826), and a connected
UDP socket queues only its peer's datagrams, so `poll` and `recv` agree. A
socket's `IP_TOS` marks the IPv4 header of every TCP segment and UDP datagram
it sends, ECN bits left to TCP, and an accepted connection inherits its
listener's.

**The laptop's NIC is a Realtek RTL8168h.** `drivers/src/rtl8168.rs` binds
`10ec:8168` and brings up the versions `rtl8168-core`'s table names —
RTL8168h/8111h and the RTL8168M, one MAC version told apart by TxConfig's
hardware id — and declines any other, naming its id in the log, because each
version needs a sequence of its own. `rtl8168-core` is the chip as data and as
a register sequence: the register map, the version table, the descriptor codec
and ring bookkeeping, the take-over from the management firmware, reset, the
station address (ERI, else MAC0), the PHY's settings through GPHY OCP (the PHY
patch firmware is not loaded) and start, written once over a `RegisterBus` and
host-tested under `just test-host` against a simulated chip, since QEMU has no
model of it. Its register offsets, bit values and the order of the bring-up
are facts taken from Linux's `r8169` and Redox's `rtl8168d`; no code or prose
of the GPL-2.0-only driver is. The FIFO-drain and link-list waits are
advisory, as they are there: one that times out is logged and the bring-up
goes on, while a reset that never completes leaves the chip unused. RxMaxSize
is the RX buffer length a descriptor names, so the chip never writes past a
buffer; interrupt status is cleared whole before the rings are harvested, so
any later event is a fresh interrupt; TxPoll is rung again after a reclaim
that leaves frames in flight;
bring-up and stop run with the driver's spinlock released, serialised by a
mutex; ASPM L0s and L1 are turned off in the endpoint's Link Control unless
the chip says the machine's vendor validated them (MAC OCP `0xc0b2`); and
`driver_core::shutdown` stops its DMA on poweroff and reboot. Only the laptop
grades the hardware half.

**The CPUs run at the speed asked of them.** `sched/src/cpufreq.rs` is
frequency control and core-type topology; the register layouts, the command
line and the placement score are `cpufreq-core`, host-tested under `just
test-host`. Every register involved is per logical CPU and reachable only from
that CPU, so each CPU records and programs its own: the boot CPU in the
`cpufreq` boot step ahead of `smp`, each AP during its bring-up, after its IDT
is loaded. Under `cpufreq=hwp`, the default, a CPU with HWP enables it
(`IA32_PM_ENABLE`, write-once until reset, notifications off) and asks for
autonomous selection between its lowest and highest level — guaranteed when the
firmware disabled turbo — at the configured energy-performance preference;
`cpufreq=firmware` writes nothing, and a CPU without HWP keeps the firmware's
legacy settings either way. The boot CPU records what the firmware left first
(`IA32_PM_ENABLE`, `IA32_MISC_ENABLE`, `MSR_PLATFORM_INFO`, each CPU's
`IA32_PERF_CTL` and a firmware-enabled HWP request), and the boot log's
`CPUFREQ:` line says it. Each CPU samples `IA32_APERF`, `IA32_MPERF` and the
TSC together at every timer tick and idle entry — the pair stops while a CPU is
halted, so the idle sample stays current — and `IA32_THERM_STATUS`, CPU 0 also
the package's; `cpu_perf` (unprivileged) hands userland those samples, the HWP
registers, the core type and SMT position (`CPUID` 0x1A and 0xB) and the
measured TSC rate MPERF counts at. `cpu_perf_ctl` (`Power`) changes the
preference, the limits, the placement policy or enables HWP on a boot that
left it to the firmware: a settings generation every CPU applies at its next
tick or idle entry, and is woken for. MSRs are touched only where CPUID
enumerates them, the Intel-only ones only on Intel, and the thermal and
platform ones only on bare metal. On a part whose CPUs differ — hybrid core
types, or SMT siblings — `sched.hybrid=on` places a waking or new task on the
best idle CPU: a P-core whose core is idle, then an E-core, then a thread
beside a busy sibling, the HWP highest level breaking ties and a warm
`last_cpu` kept when it is as good; the kick that sends an idle CPU to steal
prefers the same. A running task is not migrated toward a better CPU; it moves
when it next wakes. Identical CPUs without siblings, as QEMU's are, place as
before. `/bin/cpufreq` (`set` is privd's `org.slopos.cpufreq.set`, run with
`Power`) is the tool: `status`, `watch`,
`run -- CMD` (busy time, effective MHz per class, package temperature and
throttle causes, and one `CPUFREQ[run]:` line on stderr), `set
hwp|epp|limits|placement`, and `bench`, which times one workload pinned to each
CPU, on an SMT pair and wherever the scheduler puts it (`CPUFREQ[bench]:`).
`sysmon` shows each CPU's type and clock. `/bin/prof` (`prof_ctl`; privd's
`org.slopos.prof`, run with `SysInspect`) starts, stops and reports the
sampling profiler on a
running system; its report carries each CPU's busy share and effective clock
over the span.

**A machine on the LAN is driven from this host.** `/bin/remoted`, which init
starts when the base carries `/usr/share/slopos/remote/remote.conf`, dials out
— nothing on SlopOS listens — to `scripts/remote.py serve`, the broker, on one
TCP port (7330), over TLS 1.3 with `tls-core`, pinning the broker's own CA and
the name `slopos-remote`, and proves itself with the broker's token. It keeps
an idle connection open and opens another whenever one takes a request, so
requests run side by side; it redials with backoff and pings both ways.
Requests are `remote-core`'s frames: run a program (argv, cwd, env, stdin,
timeout; stdout, stderr and the exit status streamed back), and write or read
a file, atomically and checked by SHA-256 end to end. Init starts remoted as
the session, uid 1000, holding nothing; each program it runs that the policy
names as a mechanism, and the shell, gets a privd connection remoted made
(`PRIVD_FD`), through which a mechanism asks as the paired agent, which the
policy allows the boot slots and bases, software installs, power, `cpufreq set`, `kconsole`, the profiler
and the keymap and nothing else — so whoever holds the broker's key holds
those. A mechanism keeps the connection its environment names from whatever
it starts, and the shell hands it on only to the mechanisms its own process
starts while its environment names it, not from a subshell or a
pipeline stage and not while one it handed it to lives
(`slopos_userland::privd::takes_connection`, the shell's `privd_for`), so no
code from outside the base asks as the paired agent and no two requesters
share it. A command's timeout sends its group `SIGTERM`, which a requester
relays to its mechanism, before `SIGKILL`. **The pairing lives only in the
base,** which no
process can write: a pairing read from `/etc` would let whatever writes there —
uid 0, or a program holding `DacOverride` — point remoted at a broker of its
own. The host bakes it in
(`REMOTE_PAIRING_DIR`, which `tools/initramfs` packs whole or not at all) into
a separate base, `builddir/initramfs-remote.cpio`, so the shipped
`initramfs.cpio` never carries a token; `scripts/selfhost.sh` carries a
running machine's pairing into every base it builds there; and `remote.py`
refuses to serve or install a base paired with another broker, or with none.
The token is readable inside the base, which lets a local program pose as the
agent to the broker and no more. The broker's plain-HTTP side serves only `GET
/files/<name>`, the files it was told to offer. The CLI reaches the broker over
a unix socket: `just remote status|wait|run|sh|push|pull|klog`, and `just
remote-install`, which builds the paired base under a fresh
`SLOPOS_BUILD_TAG` and installs it, waiting for remoted to report that tag as
the running base's (each hello names the base it runs from now), and builds
the release kernel untagged to compare with the one it last installed
(`remote.py kernel-changed`): only a changed kernel is built again under the
tag, put into `bootctl spare`'s slot and booted once, the machine waited for
until it comes back with that tag (`--commit` makes it the default). `just remote-serve` (`REMOTE_HOST`,
`REMOTE_PORT`) prints the bootstrap for a machine that does not yet run
remoted — `curl` the offered kernel and paired base, `bootctl base install`
and `bootctl install` each with its SHA-256, `oneshot`, `reboot` — and the `ufw`
rule a firewalled host needs. `REMOTE=1 just iso`
packs the paired base into the live ISO instead, so the medium and every
system installed from it dial the broker from their first boot; that ISO
carries the token and is for your own machines. State lives in
`~/.config/slopos-remote`, outside the tree. `just test-remote` grades all of
it under QEMU, a pairing planted under `/etc/remote` included.

**The disk is the root.** `root=auto` mounts a writable `disk0` — the first disk probed that is not a USB disk, `nvme0n1` under QEMU, so a stick left in a port never changes what boots — at `/`, so what a boot writes there persists; the initramfs is the fallback for no disk and for a disk that mounted read-only (the verified `ext2.img` boots `/sbin/init` from RAM with the attested disk at `/mnt`). `root=disk` insists on the disk, and `root=initramfs` mounts no disk it was not asked to by a `mount=` but an install medium's payload, which is what the live ISO boots with. A device `root=` or a `mount=` names that is absent is waited for until USB settles, up to `usb.settle_ms`; beyond `tests=on`'s `usb settle` step and an install medium's search for its payload, nothing else in boot waits for USB, the crash store included. `root=` also takes a device in any spelling a mount source does — `/dev/nvme0n1p2`, `vda1`, `PARTUUID=…`, `UUID=…`, `LABEL=…` — where a partition comes from the GPT or MBR table on its disk; a named device or partition that is absent degrades to the initramfs exactly as no disk does. `just boot` is the developer's persistent machine: it boots this build's kernel and base from an A/B boot disk it rebuilds every run, with `fs/assets/ext2-persist.img` as `/`, built `VERITY=rw` (a v2 trailer, so the image is writable *and* attested everywhere the guest has not written) and refreshed in place across builds (`PRESERVE_FS_IMAGE=1`: the host's toolchain only) so what the guest wrote survives. `VERITY=on` builds the verified image's v1 trailer, which write-protects the device and is what `verity=require` asserts; `VERITY=off` builds no trailer. The verified and *tests* images are regenerated on every build on purpose — a persistent `/` would make every filesystem test a mutation of the image the next run boots from.

**Every volume the tree builds is ext4, in one profile.** `ext4-core/profile`
names it — `extent`, `64bit`, `flex_bg`, 256-byte inodes with nanosecond
timestamps, `metadata_csum`, `has_journal` — and is the one statement of it:
`scripts/lib/ext4.sh` and `scripts/lib/ext4_profile.py` hand it to `mke2fs`,
and `ext4-core` compiles it in and holds it to the features the kernel
writes. `ext4-core` is the format as data, `no_std` and free of I/O:
checksums, the superblock, descriptor and inode codecs, the extent tree over
a caller's block store, directory tails, and jbd2's blocks and recovery,
host-tested under `just test-host` against images `mke2fs`, `debugfs` and
`e2fsck` made and judged; its layouts are kernel.org's ext4 documentation
taken as interface facts. The kernel reads ext2 and ext3 volumes too: a file
mapped by blocks keeps its map, a new file gets an extent tree wherever the
volume has `extent`, directories stay linear (an htree is linearised by the
first change to it) and so hold at most 64998 subdirectories, a group left
`BLOCK_UNINIT` or `INODE_UNINIT` is initialised on first use, and block
numbers are 32-bit inside the kernel, so a volume past 16 TiB at 4 KiB blocks
is refused. `build_fs_image.sh` converts a preserved image short of the
profile in place — an ext2-era root's sealed `/.journal` file dropped, the
features added in the order `tune2fs`, `resize2fs -b` and `e2fsck -fD` need,
on a copy that replaces the image only once it meets the profile, and the
journal added after — and otherwise refuses, showing the step that failed,
and names `just reset root`. `ext_attr`, which `tune2fs` cannot clear,
survives a conversion, and the kernel reads such a volume like any other.
`FS_IOC_GETFLAGS` and `FS_IOC_SETFLAGS` are `lsattr`'s and `chattr`'s: the
immutable flag is the VFS seal, an append-only file another system marked is
sealed too, ext4 also stores `nodump` and `noatime`, any other change is
`EOPNOTSUPP` (a RAM filesystem keeps only the seal, and only sets it), and
moving the seal takes the `Seal` capability, which only init, privd, the
utests and the mechanisms privd runs with it — the installer, the rescue
shell and `extctl` — hold: the seal holds the base whole — every base
inode is sealed for good, and the directories a root keeps for it are sealed
too — and privd's program identity rests on the base being what booted or
what a `BaseUpdate` holder activated since.

**A seal is final for readers.** `FS_IOC_SETFLAGS` refuses to seal a file a
writable shared mapping holds (`EBUSY`, as Linux refuses `F_SEAL_WRITE` on a
memfd so mapped), and a `write(2)` through a descriptor opened before the
seal is refused at the page set as the filesystem refuses it, so what a
sealer reads back once the seal is set is what every reader reads after.

**The root is not the only filesystem.** `mount(2)` with `fstype` `ext2`,
`ext3` or `ext4` (one driver) takes a
`source` naming a block device — `mount("/dev/nvme0n1p3", "/home", "ext4", …)`
or `mount("LABEL=home", …)` — claims that device's exclusive writer (or, with
`MS_RDONLY`, a read claim shared with other readers), and binds it to one of
four pooled `Ext2Mount` instances, each with **its own lock**. A path walk
crossing a mount therefore holds one mount's lock while taking the next one's,
which is why the four instances carry four separate `lock_class!` sites rather
than one shared class;
slot 0's is still named `CACHED_EXT2`, so the class boot registers is the class
it always was. `umount` of the last mount of an instance flushes it, marks the
image clean, drops the device and returns the slot, so the write claim is
released and the same disk can be mounted again — a leaked claim answers
`Busy` forever, and that is the failure the remount test exists to
catch.

**A warm path walk does not take the mount lock.** Each component is a
`lookup` and a `stat`, and both answer from per-mount caches
(`fs/src/ext2_dcache.rs`) of names — negative ones included — and of inode
attributes, checked against
generation counters that the ext2 code bumps under the mount lock whenever a
record is rewritten, an entry of a name is added or removed, or an inode is
allocated or freed. Attach and detach bump every counter, so a pool slot handed
another image cannot answer from the last one. A new path that changes a
record or a directory without `Ext2Fs::write_inode_num` or the `dir` entry
helpers must bump the counters itself, or the walk serves what it replaced.

**A rude exit is survivable.** The flusher marks the image at rest *on the
medium* once a pass leaves nothing dirty, nothing unbarriered, no
superblock drift and an empty journal, and the mount has had nothing to write
for a second — the state ext4 reaches for `fsfreeze`, here reached
automatically at idle; a busy mount would pay a superblock read, write and
barrier each way on every pass — and `Ext2Fs::transaction` re-stamps it
in use before the next mutation reaches the device. At rest is the
journal's superblock emptied before `needs_recovery` is cleared; in use is
the reverse, the flag set before the journal says it is live, so no crash
leaves a live journal under a clear flag, which `e2fsck -p` will not decide
alone. A volume without a journal is stamped through `s_state` instead. A
mount owes the stamp from the moment it attaches, since attaching stamped it
in use and a mount nothing writes to runs no pass. Closing the QEMU window
therefore costs at most the last idle window's writes, instead of leaving an
image that mounts read-only forever after and that `root=auto` then demotes to
`/mnt` while booting the initramfs. The host half is the same promise:
`build_fs_image.sh` never deletes a `PRESERVE_FS_IMAGE=1` image. One that is
damaged, not at rest, or built under a different `VERITY` stops the build
naming the command that repairs it, except that `just boot` boots a disk
closed mid-write without refreshing it, so the kernel replays its journal;
`just reset root` is the only
thing that discards one; a larger `PERSIST_IMAGE_SIZE`, or free space under
the floor, grows the image with `resize2fs` rather than rebuilding it; and a
block the guest wrote stays un-attested across rebuilds, because the seal
leaves out every block that was allocated and unattested once the build has
converted and grown the image, before it installs anything, and every block a
later resize allocates, which is where a resize moves what it must. `PERSIST_IMAGE_SIZE` is a minimum, 512M by default; the
ceiling is what the machine's RAM allows rather than what one allocation
allows, because
the verity hash array is chunked (4 bytes per 4 KiB block, in 256 KiB pieces,
refused past a stated share of usable memory) instead of one contiguous `KVec`.

**A write is logged before it lands.** A writable image carries a jbd2
journal in its journal inode (`FS_JOURNAL_SIZE`, default 1/64 of the image
floored at 4M and capped at 64M; `0` builds none), in the format jbd2
documents and e2fsprogs replays: descriptor blocks of `CSUM_V3` tags, revoke blocks, and a
commit block written only behind a flush that covers its transaction's
records and the data they name. An operation's metadata goes into the journal
before any of it reaches a home location; file data never does, and instead
reaches its home ahead of the commit that names it (`data=ordered`). That is
what makes an operation retractable (a rollback rewinds the journal; nothing
was published) and a crash recoverable: a mount that finds `needs_recovery`
replays every committed transaction and comes up **read-write**, which is the
one case in which this kernel repairs an image instead of deferring to
`e2fsck`. Nothing is written home until every committed copy has checked out,
so a journal with one bad copy is replayed by nobody here and the volume
mounts read-only for `e2fsck` to decide, as does one holding transactions
under a clear `needs_recovery`, whose copies may be older than their homes. `just test-rude-exit` is the other
half: a boot fsyncs a file and ends the machine holding it, and the host's
`e2fsck` must replay what the kernel committed (`scripts/check_fs_replay.sh`).
The journal is an *image* property, not a kernel one — an image without one
gets undo-scoped operations and still refuses an unclean mount — and the boot
log says which of the two a mount got. The journal inode has no name, and a
name that reaches any reserved inode is refused as damage. The default image
is 32M rather than 16M because the journal takes 4M of it.

**A commit is grouped, not synchronous.** An operation's records are staged
into an in-memory ring (`Journal::pending`, up to `PENDING_SLOTS_MAX` slots)
and reach the medium together: `sync_log` writes dirty data home, writes the
ring with each commit block behind a flush, and flushes. The flusher does that every
`COMMIT_INTERVAL_MS` (1 s rather than jbd2's 5, because the build machine is
the one most often closed rudely), a full ring does it inline, and `fsync` is
the file's data plus `sync_log`. The operations whose records are still in the
ring form one open transaction with a single commit record, written when the
ring goes out: an operation touching a block already logged there rewrites
that slot in place rather than logging the block again, which is what keeps a
burst of creates in one directory from logging its inode-table and directory
blocks once per create. A metadata block's home is written only once
its newest record is durable, and a block an operation frees is not handed out
again until that free is durable, or a crash that loses the free leaves another
file's bytes in its old owner's block. Freeing a block whose data is still the
cache's only copy writes it home first: an earlier operation's commit may reach
the medium without the later one's. The flusher's commit copies dirty data
out (512 KiB a trip, the blocks marked clean and in flight) and writes it with
the mount lock released, behind a per-mount gate every other request of the
mount's device waits on — reads included, so nothing finds a home the cache
already counts as written — and a barrier after a failed trip answers an error
until the flusher has made those blocks dirty again. The block cache is sized from memory
(`cache_entries_for`: an eighth of usable frames, capped by the volume and at
`CACHE_ENTRIES_MAX`) and grows in chunks as it fills.

**A writeback pass is bounded, and a mount has one.** `sync(2)`, the flusher
and a writer short of log room all drive the mount's one open pass through
`Ext2Fs::sync_step`, in chunks of `WRITEBACK_CHUNK` device writes, releasing
the mount lock between them, so a path walk or an `exec` queued behind a pass
waits for a chunk rather than for the whole pass, and a burst of callers pays
for one pass rather than one each. A pass fixes a *dirty epoch* and the log's
head and generation when it opens, which is what keeps the ordered phases
ordered across those gaps: an operation that runs in one is entirely outside
the pass — so a `sync` that finds a pass open drives it to its end and then
the next one, the way a jbd2 commit waiter does. Mutations remain serialised
per mount — the plan's per-inode locking is deliberately not what landed,
because the wait, not the lock count, is what G5 was about.

**Memory is promised before it is touched — except a fork's copy, which the
OOM killer stands behind.** Every private mapping is charged against a
*commit* ceiling when it is created — `mmap`, `brk`, an `mprotect` that makes a
`PROT_NONE` reservation accessible — and refused there with `ENOMEM`, so a
process that could never be backed is told at the allocator, not killed at its
first page fault. The ledger is the `CommitPages` quota kind on
the root account, its limit `mem.commit=<percent>` of usable frames (default
100; `0` records without refusing) installed by the `commit ledger` boot step,
and `sys_info` reports the limit, the promised total and the headroom left, and
how many processes the OOM killer has taken and the last one's pid. The
same boot step derives the per-process `PinnedBytes` default — an eighth of
usable memory, the share the file map hands one owner — since the `abi`
default was sized for an appliance, a compiler's shared objects exceed it
before `main`, and a linker maps every rlib of the kernel at once. A region is charged one of three ways, and `VmaRegion::commit` says which: an
`Extent` region owes its whole span when it is created, so a fault in it never
finds itself unaccounted for; a `Frames` region — the loader's eager segments,
the mapped stack, the stack's lazy growth extent, `MAP_NORESERVE` — is charged
one page at a time as pages are placed; a `Forked` region is a fork's copy of
a region its parent had charged, and owes a page from the moment the child
holds it as its own — a write that breaks copy-on-write, a fresh page — so its
paid pages are exactly its present leaves not marked copy-on-write, and a
second fork that marks them again, an unmap and teardown each give them back.
`fork` therefore charges nothing and is never refused on the ledger: Unix
software assumes a cheap one. The parent's own charge still covers its span,
so a parent that writes a page it still shares pays nothing more, and the
child keeps the old frame unpaid until it writes it — the overcommit. Shared
objects, a file's page set and the ring share are `Unreserved`: their frames
are owned elsewhere. A class is never given back — `commit_under` moves only
an unreserved region, the first time a protection lets its pages be populated
— and `MAP_NORESERVE` is an attribute of the region, honoured rather than
ignored because a caller that says it will not touch the whole reservation is
asking for the fault-time road on purpose, and a sparse gigabyte is what a
runtime's address-space reservation looks like. So only forked copies, stack
growth and `MAP_NORESERVE` can outrun memory, and when such a write finds the
ceiling refusing its page or the buddy empty after reclaim, `mm::oom` kills
instead of faulting the writer, as Linux's OOM killer does. The victim is the
process whose own account holds the most of what the write found missing:
`ResidentPages` for an empty buddy, `CommitPages` for a full ceiling — a
process that promised itself a gigabyte and touched none of it frees no frame,
and one holding frames promised elsewhere frees no promise. Both rows count a
memfd toward whoever sized it, mapped or not, and a shared memfd or ring page
toward no mapper: such a region is populated whole from `mmap` to unmap, so
`VmaMap` subtracts its span from the leaves it syncs, and the sizer's memfd
holds the frames charge beside the commit one. Present leaves would count a
shared page in every mapper and a held memfd in none, so any process could pad
another's count and make it the victim. A file page counts toward each process
that faulted it in, which only its own faults can do. The ledger keeps each
row's own share beside the subtree total (`quota::held_by`), so a child's
holdings are the child's, and a process spawned past `MAX_ACCOUNT_DEPTH` still
gets a row, debiting through its nearest ancestor with room, so no fork depth
escapes the ceiling or the measure. A memfd outlives its sizer whenever another
process holds its fd, so the sizer's address-space teardown hands each memfd it
sized to the root (`ChargeSlot::bequeath`): the root is never released, so the
machine's ceiling keeps counting it until the memfd goes, and it is no
process's own, since no death gives it back. Only processes whose every
capability the writer holds (`signal_dominates`, the capability half of
`kill`'s rule; no uid is consulted, as Linux's OOM killer consults none) are
ever taken, as Linux's `oom_score_adj=-1000` is absolute: one holding a
capability the writer lacks is never killed for its write, so a leaking
privileged service is taken by its own write, once it is the heaviest process
it dominates, and an unprivileged writer dies rather than take it. Init's own
write is the exception, shielded from nothing, since it cannot be impersonated
and its death takes the machine down; init itself is never taken. The victim is
killed the way every kill works (the flag each thread unwinds from, I8). The
writer drops the address space, waits — killable, bounded — for the victim's
frames to be back (noted once the teardown has dropped the address space, not
inferred from the slot's unbind), and writes again; while a victim is dying
nobody picks a second — a victim whose last task left before the kill reached
it included — and one still holding its memory five seconds after the kill
stops holding back the next choice. Only when nothing but init, the dying,
processes the writer does not dominate and processes holding none of what is
missing is left does the write fail, as a `SIGKILL`
(`TaskFaultReason::UserOom`). A
write the kernel makes for the task takes the same road wherever it may
block: a user copy from a syscall, and the signal frame, whose delivery on a
trap's way out steps out of interrupt nesting with interrupts on for it, as
the `#PF` path does. It is refused only where no wait is possible: a copy
made under a spinlock or preemption pin, in an interrupt handler or with
interrupts masked answers `EFAULT` — the frame copies that follow delivery's
populate are such copies, and meet the pages it made writable — and a frame
the killer found nothing to free for fails the push, ending in `SIGSEGV` as
Linux's `force_sigsegv` does. An `exec` is charged beside the image it
replaces: the segments, interpreter and stack are sized from the headers and
charged before the old image is released, then advanced to the loader, so a
program that cannot fit is the caller's `ENOMEM` rather than a fault in a
process that no longer has a program. slibc
implements the `posix_spawn` family over the kernel's `spawn` primitive — the
child's descriptor table is computed in the parent and handed over whole, and
no address space is copied at all — and the std fork takes that road for
`Command::spawn` because it is the cheaper one, falling back to `fork` for
`pre_exec` closures and attributes the primitive cannot express.

## Knowledge Index (AI)
`knowledge/` hosts a local semantic index for querying the codebase. Build once with `python3 -m venv knowledge/.venv && . knowledge/.venv/bin/activate && pip install -r knowledge/requirements.txt && python knowledge/index.py`, then query via `python knowledge/query.py "<question>"` for signatures, drivers, or file locations. Rebuild after large refactors or merges. Never commit the venv or embedding database artifacts.

## Coding Style & Naming Conventions
All kernel code is Rust `#![no_std]` on nightly with `#![forbid(unsafe_op_in_unsafe_fn)]`. Keep unsafe blocks tiny and well-documented; prefer `pub(crate)` helpers and prefix cross-module APIs with their subsystem (e.g., `mm::`, `sched::`). Match the existing four-space indentation and brace-on-same-line style. Assembly sources (when needed) are Intel syntax (`*.s`) and should document register contracts.

### Comments
Write code that does not need comments. Most comments are useless: they restate the code, drift out of date, and add noise.

- Default to **no comment**. Express intent through naming, types, and structure instead.
- Comment only when the code genuinely cannot carry the meaning: a hardware/spec quirk, a non-obvious ordering or locking requirement, a deliberate deviation, or a *why* that is invisible from the diff.
- When a comment is warranted, keep it **concise** — one or two lines. No restating the signature, no narrating control flow.
- Treat the urge to comment as a smell. Needing one usually signals a hack or unobvious behaviour; prefer fixing the code so the comment becomes unnecessary. If the hack must stay, say why it exists, not what it does.
- Exempt from the above: `# Safety` sections, `///` public API docs, and register-contract notes in assembly. These are contracts, not commentary.

### Unsafe-code surface
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (the framekernel **AD-1/AD-2** discipline: one trusted crate holds all `unsafe`, every other kernel crate forbids it; CI-enforced by `scripts/check_unsafe_outside_ostd.sh`). Every other crate the kernel binary links (`abi`, `acpi`, `boot`, `boot-core`, `core`, `cpufreq-core`, `drivers`, `ext4-core`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `hid-core`, `keymap-core`, `ktesting`, `mm`, `net`, `nvme-core`, `pidfd`, `ring`, `rtl8168-core`, `sched`, `service-core`, `signalfd`, `usb-core`, `verity-core`, `video`, `vt`) carries `#![forbid(unsafe_code)]`, and `check_unsafe_outside_ostd.sh` asserts that from the binary's own dependency closure, so a new crate is covered the moment it is linked. Userland-side crates (`userland/`, `apps/`, `cabi/`, `slibc/`, `slop-protocol/`, `appkit/`, `slopos-rt/`, `windowing/`, `fat-core/`, `tree-core/`, `remote-core/`) are out of scope for this discipline.

`forbid` is necessary but not sufficient: rustc drops any `unsafe_code` diagnostic whose primary span satisfies `in_external_macro`, so a macro defined in another crate expands `unsafe` into a forbid crate silently, and the call site holds no keyword for a source scan to find. `scripts/check_unsafe_expansion.sh` is what closes that — see below.

One documented exempt site exists outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct. The base-interface ELF note's `#[unsafe(link_section)]` static lives there too, the one written outside OSTD's macros: the note describes the kernel file, so it belongs to the crate that is the file.

Three C-ABI entry points keep `#[unsafe(no_mangle)]` in `boot/src/ffi_boundary.rs` — `kernel_main`, `common_exception_handler`, `isr_iret_frame_corrupt`. Their callers are assembly, so the symbols have to resolve at link time; routing them through a registered hook would leave an uninstalled window across early boot in which a fault triple-faults instead of panicking. `check_unsafe_expansion.sh` allowlists them by name.

These gates enforce the discipline. The source-scanning gates run via `just check-framekernel` and CI (and from a build when `KERNEL_BUILD_GATES=1`); the two ELF-inspecting gates (`check_stack_sizes.sh`, `check_kernel_softfloat.sh`) run on every kernel build, for every variant. A third class builds a probe and inspects what came out (`check_codegen_backend.sh`, `check_linker_script.sh`); those run from `just check-framekernel-gates` and CI only, never from a build, because the codegen one compiles `core` once per flag set. The source scans are kept off the default build path so the interactive boot loop stays fast.

Every gate carries a `--self-test`, run from `check-framekernel-gates` before the real scan. A check that has never been observed to reject has not been observed to work: the source scanners assert exact hit counts against planted violations *and* silence against the forms they deliberately accept, while the three ELF gates synthesise objects with `llc` (an oversized frame, a `movups`, an unblessed `link_section`) and assert the gate rejects them. A gate whose self-test fails is a build failure.

The build produces one ELF per variant — `builddir/kernel-dev.elf`, `kernel-release.elf`, `kernel-tests.elf` — because their codegen differs and a single shared path meant whichever build ran last silently answered for all three, to the gates, to gdb, and to the ISO builder. The two ELF gates take a required `--variant` and read their measured allowlist from `scripts/gates/{stack,vector}/<variant>.txt`. Those files also carry the input-sanity floors, so weakening a check is a diff on a tracked file rather than an edit to a script. Every allowlist entry must match something in the ELF being checked: an entry that matches nothing is a dead exemption and fails the gate, which is both the ratchet (a frame that shrinks hands its exemption back) and what makes a mis-stated `--variant` fail closed rather than pass on a list that happens to fit.

- **`scripts/check_unsafe_outside_ostd.sh`** — fails if any `.rs` file under a kernel crate (other than `slopos-ostd/`, `slopos-ostd-derive/`, or the exempt file above) contains an `unsafe` keyword that is not a comment, not the `#[unsafe(...)]` attribute form, and not `#[cfg(...)]`-gated. Mirrors `check_alloc_dep.sh`'s cfg-aware lookback. Also asserts that every crate in the kernel binary's dependency closure carries `#![forbid(unsafe_code)]` at its crate root, so a newly linked crate cannot start life without the lint.
- **`scripts/check_alloc_dep.sh`** — fails if any kernel crate's `Cargo.toml` declares a direct `alloc` dependency **and** fails if any kernel `.rs` file (other than `kernel/src/main.rs` and the `slopos-ostd/` tree) contains a bare `extern crate alloc;` / `use alloc::` / `use ::alloc::` statement (with `#[cfg(...)]`-aware lookback so cfg-gated usages that compile out of the kernel build are accepted).
- **`scripts/check_stack_sizes.sh`** — fails if any function in the kernel ELF has a stack frame larger than `STACK_SIZE_THRESHOLD` (default **2048 bytes / 2 KiB**, matching Linux mainline's `CONFIG_FRAME_WARN` default on x86_64/arm64 but stricter in enforcement — SlopOS fails the build, Linux merely warns). This is the load-bearing enforcement of SlopOS invariant **S-5** (bounded kernel stack use — named to avoid collision with the framekernel paper's own Inv. 5, *sensitive memory cannot be tampered with by user programs*). Driven by `-Zemit-stack-sizes`; inspects the final ELF's `.stack_sizes` section, so it catches NRVO failures, inlining, and trait-object dispatch that a source-level heuristic would miss. Above the threshold sits a second limit no allowlist can raise: the 4 KiB guard page. The target sets `"stack-probes": {"kind": "none"}`, so a frame larger than that steps clean over the guard in one instruction — a measured cap records how big a frame is, not whether that size is survivable. `min-records` is what stops a dropped `-Zemit-stack-sizes` from reading as a kernel with no large frames: `llvm-readobj` prints an empty `StackSizes [ ]` and exits **0** for an ELF carrying no section at all.
- **`scripts/check_kernel_softfloat.sh`** — fails if the kernel ELF touches XCR0-managed register state outside the sanctioned save/restore. The kernel must be built `+soft-float` so it never touches that register file: a syscall/exception entering from userland does **not** save the caller's FPU/vector state (only a full context switch does, via `xsave`/`xrstor`), so a single kernel instruction that disturbs it in a fault/IRQ path clobbers the interrupted user task's live registers. The scope is all four classes XCR0 enumerates, not just the vector one — x87 and MMX share one physical register file with XMM under XCR0 bit 0, and `xrstor64` overwrites the whole area at once — because XMM is only the instance rustc is likely to emit, and hand-written `asm!` is not subject to target features at all. The soft-float guarantee lives in `targets/x86_64-slos.json` (`features: …,-sse,…,+soft-float` + `rustc-abi: softfloat`) — **not** in `.cargo/config.toml`, because a `RUSTFLAGS` env var fully overrides `target.*.rustflags`. The `slopos-ostd` xsave-conformance helpers in the `kernel/tests` build are one allowlist entry with a measured instruction budget rather than a whole-binary exemption, so a vector instruction anywhere *else* in the tests kernel still fails.
- **`scripts/check_unsafe_expansion.sh`** — expands every kernel crate with `-Zunpretty=expanded`, over each crate's feature configurations, and holds the result to a constant rather than a recorded count: zero executable `unsafe`, `unsafe impl` only of an allowlisted trait, `#[unsafe(link_section)]` only of a `link.ld` section, `#[unsafe(no_mangle)]` only of an asm-called symbol. This is the only mechanism that sees macro-injected `unsafe`; `forbid` and the source scan are both blind to it. A golden fixture fails the gate if a toolchain bump moves the compiler's own emitted shapes. ~16 s warm.
- **`scripts/check_process_designator.sh`** — fails if a process-keyed table entry point (`mm/src/process_vm.rs`, `fs/src/fileio/`) takes a bare `u32` process id, or if a lock-free scan for a matching id grows back. Ids recycle, so a `u32` parameter is a confused-deputy surface: a stale one designates whichever process holds that number *now*, and the kernel services the call against a stranger's address space or open files. The replacements — `slopos_ostd::process::ProcessId` and `slopos_fs::fileio::FdTable` — carry a generation and can only be built from a live process, so a stale one fails the check instead of resolving. Scope is deliberately narrow: a `u32` pid is still correct at the ABI boundary (`getpid` returns one, the PCR carries one across a syscall); what must not happen is a *table lookup* keyed on one.
- **`scripts/check_registry_sections.sh`** — holds the kernel ELF to `link.ld`'s section set and each linker registry's span to a whole number of entries. Catches a *dependency's* `link_section`, which no first-party scan can see, and the wrong-entry-size case that would make `registry_slice`'s `offset_from` unsound.
- **`scripts/check_authority_reachability.sh`** — walks the linked ELF's call graph from every syscall handler to the terminal power primitives, and fails unless each handler that can reach one is either classified `Power` itself or carries a stated reason in `scripts/gates/authority/<variant>.txt`. The `rustc`-level classification gate in `core/src/syscall/handlers.rs` covers *the table*, not *reachability*: `roulette_result` was classified, the gate was green, and its loss arm called `kernel_reboot` two syscalls from an unprivileged caller. The ELF is the input rather than the source because inlining, generic instantiation and trait-object dispatch all change who really calls whom. Indirect calls (`call *%rax`) are the seam it cannot see, which is why the kernel-initiated `PowerOps` callers are a tracked list rather than something it discovers. Runs against the dev kernel from `check-framekernel-gates`, and separately in CI against the **tests** and **release** ELFs — the tests kernel is the only variant whose allowlist carries `run_userland_tests`, which powers the machine off to end the run. Gated in CI rather than on the build path because the walk disassembles the whole ELF (~8 s).
- **`scripts/check_safe_contract_surface.sh`** — ratchet on safe `pub fn`s in `slopos-ostd/` that carry a `# Safety` section. Those are self-declared caller obligations the compiler does not check, so a fault lands in the trusted core while the cause is an ordinary safe call in a service crate. The baseline is **0**: every such contract is currently expressed instead, as a capability witness (`&IrqDisabled`, `&BspToken`, `Osxsave`), a validated newtype (`Xcr0Mask`), a linear handle (`ptr_buf::OneShotBuf`), an owning reference (`KArc`), a sealed trait (`ApTrampolineAbi`), a runtime-checked borrow (`sync::PerCpuSlot`), or a slice in place of a pointer and a length. Reach for those before raising the baseline. Not a count of safe fns containing `unsafe` — that is the design working, not a defect.
- **`scripts/check_toolchain_pin.sh`** — holds the userland target's standard library to the fork it is pinned to. `x86_64-unknown-slopos` builds on an owned sysroot (`third_party/rust-slopos`, materialised by `scripts/make_slopos_sysroot.sh` out of the patches under `toolchain/{rust,libc}`), and every way that can drift is silent: a fork cut against a different `rust-toolchain.toml` channel, a patch edited without its `toolchain/PIN` checksum, a sysroot left over from a previous overlay, or a `slopos` toolchain registered against some other directory. A stale sysroot compiles — it just compiles the previous std. It is the replacement for the retired std-patching script's `cfg_select!` arm-order check, whose failure mode (an arm placed after the `_` wildcard, dead code that still compiles — it shipped once as a `ud2` in `std::process::exit`) cannot occur now that the target is unix-family and rides std's own `sys/pal/unix`. It also asserts that every file the patches *create* is present in a materialized sysroot: the stamp describes the overlay, not the result, so a tree a broken run left half-patched — patched std, unpatched libc — otherwise carries a correct stamp and passes (observed). It grades both materialised trees the same way — the sysroot and the compiler source tree (`third_party/slopos-rustc-src`) — each against the stamp of the overlay half it was built from, so a compiler-fork edit does not restamp the sysroot and a std edit does not restamp the source tree; a `libc` edit restamps both, because the compiler tree carries its own copy of the fork for rustc and cargo. The tree checks and the link check are conditional on those existing, so CI that never materialises either still passes on the pin alone. It also grades `toolchain/crates/PIN` — every crate port pinned by its `.crate` checksum and its patch — and the ports and the libc copy in the materialised compiler tree.
- **`scripts/check_rustc_target.sh`** — holds the built-in `x86_64-unknown-slopos` target to `targets/x86_64-unknown-slopos.json`. The compiler fork exists so bootstrap can resolve `--host=x86_64-unknown-slopos`, which leaves two specs describing one machine, and nothing about a disagreement between them fails to compile: a cross-built toolchain would simply produce binaries for a slightly different target than the tree tests. The comparison is `Target::to_json()` on both sides — rustc's own normalisation, every field — plus the two facts the fork is for: the tuple is in `TARGETS`, and the target still allows dynamic linking, which is what `rustc_driver`'s `crate-type = ["dylib"]` and `libc.so` both need. It then runs rustc's own per-target test (`check_consistency(TargetKind::Builtin)` and a JSON round trip), which is what upstream CI would run on the patch. That check is also why the JSON says `relocation-model: pic`: rustc refuses a built-in target that allows dynamic linking under any other model, and the static images pin `-C relocation-model=static` on the build line instead. 28 s cold and 1.3 s warm, at 1.1 GB under `builddir/gates/` that `just clean` removes; `skipped` without a materialised source tree, `--require` in the CI job that materialises one.
- **`scripts/check_cxx_pin.sh`** — holds the cross-built C++ runtime to `toolchain/cxx/PIN` and to what `libc.so` exports. Two silent failures: a `third_party/slopos-cxx` built from a different llvm-project or a different clang still links, it just links a different C++ ABI; and `libc++.so` is linked without `-z defs`, because an undefined symbol in a shared object is legal and the loader resolves it at load time — so a libc gap that would have been a link error is instead a `dlopen` that fails on a machine, at the point the runtime is first needed. The gate holds every symbol `libc++.so` leaves undefined (171 today) to being one `libc.so` defines, and holds the built tree's stamp to what `make_slopos_cxx.sh --print-stamp` says it should be — asked of the build script rather than recomputed, so there is no second copy of that digest to drift. It also holds the LLVM port to its *scope*: `make_slopos_cxx.sh`'s stamp leaves `toolchain/llvm/*.patch` out on the grounds that a tree carrying them builds the same libc++, and a hunk reaching into `libcxx/` or `libcxxabi/` would make that false while leaving the stamp — and so the decision not to rebuild — unchanged. Both symbol and stamp halves are conditional on the tests userland having *staged* the runtime (`builddir/libc++.so`), so a checkout that never cross-built it still passes on the pin's own consistency. Deliberately not on `builddir/libc.so`: the shipped userland build stages that one too, two steps before the tests build that refreshes the runtime, so keying on it graded a cache-restored tree and turned every commit moving the pin, the build line or slibc's headers into a red gates step.
- **`scripts/check_llvm_port.sh`** — compiles LLVM's `LLVMSupport` and `LLVMTargetParser` for `x86_64-unknown-slopos` against slibc's headers and the cross-built C++ runtime. Those two and not LLVM: `Unix/{Path,Process,Program,Signals}.inc` are the files that name a libc, `raw_ostream.cpp` and `ConvertUTF.cpp` the ones that name a C++ library, `Triple.{h,cpp}` are where the port's `SlopOS` enumerator lives, and the other twelve hundred objects in a full build are portable C++ over them. That covers four of the port's six files; clang's `OSTargets.h` and `Targets.cpp` are held by nothing but `git apply --reverse --check`, because reaching `SlopOSTargetInfo` means building clang, which is a different order of cost from this gate's minute. Two things break it and neither fails to compile on the host — `toolchain/llvm/*.patch` stopping to describe the tree, and slibc losing an entry point or one of the four headers (`<inttypes.h>`, `<endian.h>`, `<sysexits.h>`, `<wctype.h>`) these files include. `--self-test` grades the skip and `--require` paths on any host, and wherever there is a tree to plant one in it grades two rejections against their own positive controls, which a rejection test without one does not have: `Path.cpp` with `-U__slopos__`, the OS-dispatch half's effect removed, and a `static_assert` that `Triple::LastOSType` is `SlopOS` rather than the `Vulkan` it was before the patch. 47 s cold on four cores and 1.5 s warm, the self-test 3.5 s; `skipped` without a materialised source tree, a staged runtime or a host `llvm-tblgen`, `--require` in the CI job that has all three — `toolchain`, which builds the tests userland first, since the stamp it checks reads a `builddir/libbuiltins.a` only a userland build produces. It borrows a host `llvm-tblgen` of the pinned major, because it builds no host tools and what tablegen emits is data tables.
- **`scripts/check_clang_driver.sh`** — builds `clangDriver` and `clangBasic` out of the pinned tree for the *host*, drives a `clang::driver::Driver` at `x86_64-unknown-slopos` and grades the link job's argv. `toolchains::SlopOS` is a second copy of what `build_userland.sh` writes by hand — `crt0.o` first, `--image-base=0x400000`, `--dynamic-linker=/lib/ld-slopos.so.1`, `--eh-frame-hdr`, `-z now`, `-L<sysroot>/lib -lc`, `libbuiltins.a` last — and a driver that disagrees with it is not a compile error: it links on the host and produces a binary that dies at `execve`, with no interpreter, no `crt0.o` and no `PT_GNU_EH_FRAME`, at a load address the loader does not map. Seventy-one assertions over the five shapes of link (static, dynamic, shared, C++, and rustc's `-no-pie` executable), each taken without a driver diagnostic, plus the toolchain's own answers: `ld.lld`, the integrated assembler, no PIC or PIE default, asynchronous unwind tables, libc++, compiler-rt, and `<sysroot>/include/c++/v1` ahead of `<sysroot>/include` — the order libc++'s `#include_next` needs. `skipped` without a materialised source tree; the self-test's rejection is the same probe at a triple the port does not name, which is the `Generic_ELF` a missing dispatch hunk leaves behind. 2 min 29 s cold on 20 cores and 219 MB of build directory, ~4 s warm.
- **`scripts/check_bootstrap_config.sh`** — holds the cross-build configuration to the toolchain it claims to produce, by running bootstrap's own dry run and then compiling and linking with the generated wrapper. Three silent failures: the step graph quietly loses an artifact — `cargo` is an *extended* tool and a stage2 rustc does not depend on it, so a config that stops naming it still builds a compiler and the root arrives with no cargo on it; `toolchain/compiler/0003-bootstrap-cmake-system-name.patch` goes away, and an unrecognised triple prints a note, sets `CMAKE_SYSTEM_NAME=Generic` and exits 0, losing `LLVM_ON_UNIX` and every `Unix/*.inc` file the port patches; and the wrapper stops producing SlopOS binaries, which it does by naming two triples and would regress by naming one. ~1.4 s warm, after a first run that downloads bootstrap's stage0 (~200 MB). Once `just toolchain` has installed a prefix, the gate also grades that: no `DT_NEEDED` outside SlopOS's own libraries, no `R_X86_64_TLSDESC` (the loader does not bind one), every `DT_NEEDED` found through the object's own `$ORIGIN` search path, and every undefined name defined somewhere in its closure — the loader binds eagerly and nothing there links with `-z defs`, so each of those is otherwise a program that does not start, found only in the guest.
- **`scripts/check_offline_build.sh`** — holds the tree to building with no registry. Two silent failures: `Cargo.lock` moves and the vendored copy no longer describes it, which every host build survives because the host has a registry, and the self-hosting root arrives with a tree its own cargo cannot resolve; and `-Zbuild-std` needs std's crates.io dependencies, which cargo resolves from a lockfile the workspace never reads, so a directory holding only the workspace's crates passes every check but a build. The pins half also fails on a vendored package no lockfile names, since that is a pin nobody reviews, and on a vendored `library.lock` that is not std's, since that copy is what the self-hosting root's guest grades its tree against. `--self-test` grades six fixtures, five of them rejections. The build half is 63 s cold on four cores and ~30 s warm; `skipped` without `third_party/vendor`, `--require` in CI.
- **`scripts/check_codegen_backend.sh`** — holds a rustc codegen backend to seven of the capabilities `targets/x86_64-slos.json` depends on: an ELF object format, soft-float, `.stack_sizes`, safestack instrumentation through `__safestack_pointer_address`, `#[unsafe(link_section)]`, `#[unsafe(naked)]`, and `sym` operands in `asm!`. Two of those are flags a backend can *accept and ignore* — `-Zemit-stack-sizes` and `-Zsanitizer=safestack` — so a backend swap can leave the build green with S-5 and the dual-stack split enforced by nothing. Tracked verdicts live in `scripts/gates/codegen/<backend>.txt` and a mismatch fails **in either direction**: a `lacks` the probe finds present is the signal to re-decide whether the toolchain can be Rust-hosted — this measurement is why it is LLVM's. `disable-redzone` and the `unwind` panic strategy are stated as residual rather than probed — the gate's header says why. Cold it costs ~60 s and ~460 MB for `llvm` / ~310 MB for `cranelift` under `builddir/gates/codegen-probe/` (which `just clean` removes); warm it is ~1 s. `llvm` is graded on every `just check-framekernel-gates`; `cranelift` reports `skipped` when the rustup component is absent, and CI installs it after the gates, in the same job, so the answer is re-taken rather than assumed.
- **`scripts/check_linker_script.sh`** — holds a linker to the eighteen linker-script constructs `link.ld` uses, from `. = KERNEL_VIRT_BASE` through `PHDRS`, `(NOLOAD)`, all three spellings of `ALIGN`, `KEEP` under `--gc-sections` and the four page-table reservations past `_bss_end`. Each probe's script carries the construct under test and nothing else a probe grades — a script that scaffolds itself with an `ALIGN` reports the linker's `ALIGN` support under whatever name that probe carries — with one deliberate exception, `composed-layout`, which links a `link.ld`-shaped script because a linker can take every construct alone and compose them differently. That exception is what the gate is built around: wild 0.10.0 refuses `link.ld` on its location-counter assignment, and given the shape it does accept it keeps the script's section order and still starts the image 0x13e8 past the base it was given. A second, self-maintaining half compares the constructs probed against the keywords `link.ld` actually uses, so a construct added to the script with no probe fails the gate and a probe whose construct left the script fails as a dead entry. `scripts/gates/linker/<linker>.txt`; `lld` is graded on every `just check-framekernel-gates`, `wild` reports `skipped` when it is not installed and is pinned in the CI job that installs it.
- **`scripts/check_recipes.sh`** — holds every recipe under `toolchain/recipes/` to a pinned upstream tarball built as shipped bar a patch that teaches the target: one 64-hex `sha256`, an `https` URL naming the recipe's version, a licence, a known template, a `soname` or `program` to install, dependencies that are recipes, no file beside `recipe` but its declared `config` and `patch`es, each named `NNNN-slopos-<what>.patch` and held to the grammar the recipes paragraph above states, `arg`s that pick among upstream's options (a `cmake` or `meson` arg is `-D<name>=<value>` with the built-in names a recipe may set allowlisted and no flag, file, program or search root among the project's; an `autotools` arg a `--enable`/`--disable`/`--with`/`--without` switch or a configure cache answer naming no program, path or system triple), a `NOTICE.md` entry, and — for a recipe that has been built — the stamp `build_recipes.sh --print-stamp` computes now, and, for a built `GPL-2.0-only` recipe, a `DT_NEEDED` closure of libraries a recipe or the sysroot provides under a licence that code may be combined with, each of whose objects carries a symbol table and defines, even locally, no symbol an incompatible recipe's library exports. Nothing built (CI's gates job) skips the stamp and closure halves.
- **`scripts/check_libc_license.sh`** — holds every crate `cargo metadata` resolves for `libc.so`, `libc.a`, `crt0.o` and `libbuiltins.a` to an SPDX expression MIT alone satisfies: the four packages and everything they depend on under `--all-features`, except through dev-dependencies (build-dependencies count: a build script's output is compiled in). MIT is the licence `slibc/NOTICE` gives such crates, and a third-party one needs an entry there; another licence needs its text there and a change to the gate. A crate of this tree must keep its manifest and every target under `slibc/`, `slibc-core/` or `abi/`, so neither a new crate elsewhere nor a crate root pointed outside passes on its licence field. `core`, `alloc` and `compiler_builtins` come from the standard library's workspace, which `cargo metadata` does not describe, and `slibc/NOTICE` records them by hand. The walk must reach slibc, `slibc-core` and `slopos-abi`, so a graph the gate cannot read fails instead of passing on the roots alone. Under those three directories every `include!`, `include_str!`, `include_bytes!` and attribute `path =`, read past comments and literals, must be a literal naming a file inside them, a `.rs` file where it is code, or a single file in `OUT_DIR`; a symlink there is refused. It reads spellings, not macro expansions. The one outside input is `toolchain/libc/`, which `slibc/build.rs` renders the headers from and which is `MIT OR Apache-2.0` itself. The library's licence is the sum of what it is built from, so one dependency on a GPL-3.0-or-later crate compiles, links and passes every test, and leaves git undistributable. `--self-test` grades twenty-seven fixtures, twenty-one of them rejections. Runs from `just check-framekernel-gates`: ~0.7 s, the self-test ~1.2 s.
- **`scripts/check_base_interface.sh`** — holds the private call surface to `slopos_abi::BASE_INTERFACE`. The surface is what rustdoc's JSON says of `slopos-abi`, not its source text: each private syscall's number and the backticked `name(args) -> result` signature its doc states, packing included (a call whose doc states none fails), the items those signatures name and the docs link, everything reachable from them through field and alias types, and the `root` globs in `scripts/gates/base-interface.txt` for what no signature can name — records behind a byte buffer, op and flag families, the wire-packing methods of rooted types. Its sha256 is held to that file's ledger row for the current number. `--record` appends a row only for the number one past the last, so a changed surface takes the next number and an issued row is never rewritten; `--print` writes the surface for a diff. Doc prose, comments, formatting and item order change nothing, and a root that reaches nothing the surface renders fails as dead. Not held: Linux's own shapes a private call passes through (`sockaddr`, `msghdr`, `iovec`, `AF_*`), which Linux compatibility holds, and what a private call takes from outside `slopos-abi` — the font's glyph ranges, the kconsole keys, the EFI GUIDs, limits written into a handler — so such a fact moves into `slopos-abi` before it changes. A parser crash, or a rustdoc `format_version` other than the one the parser reads, exits 2. ~0.6 s cold.
- **`scripts/check_libc_exports.sh`** — holds `libc.so` to only growing. Every defined dynamic symbol is held to `scripts/gates/libc/exports.txt` as `func`, or `object`/`tls` with its size, since a non-PIE program copy-relocates a data export; and the size, alignment and every field offset of every struct and union `slibc/include` defines — not only those a prototype names, because exports take structures through `void *` too — to `scripts/gates/libc/layouts.txt`, as clang's `-fdump-record-layouts-complete` measures them under the target, field types left out because their spelling moves between clang releases. `--record` only adds lines and refuses while anything recorded was removed or changed, so a removal lands only as a hand edit a reviewer sees. Skipped without a library, the layout half without the host clang `cxx_host_tools.sh` resolves; `--require` in CI's `toolchain` job, where both exist. Not held: function prototypes, and an alignment set on a typedef rather than its record.
- **`scripts/check_loose_programs.sh`** — holds each program shipped outside the base to `libc.so` as its one interface: a `PT_INTERP` naming `/lib/ld-slopos.so.1`, `libc.so` in `DT_NEEDED`, and no `syscall` instruction in its disassembly. A static program carries slibc's own copy, and a system call made in place reaches the private surface `BASE_INTERFACE` numbers, or a Linux one, without the library that only grows between them, so either ties a program that changes on its own schedule to one base. Runs on `builddir/apps/bin/*` after every host userland build (`_build-userland`), since the toolkit's crates compile into base programs too and a raw call creeping back into one fails nothing else; `--self-test`, from `check-framekernel-gates`, links a good program and four bad ones with `llc` and `rust-lld` and asserts each finding.
- **`scripts/tcb_ratio.sh`** (via `just tcb-ratio`) — a hard gate at `--max 1.0` from both `just check-framekernel-gates` and `KERNEL_BUILD_GATES=1` builds. Prints lines of `unsafe` in `slopos-ostd/` divided by total kernel Rust LoC. Read it as a trend, not as a TCB fraction comparable to other projects': the denominator is raw LoC including the 41 kLoC vendored DWARF reader, and published comparators measure post-LTO linked code size.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

### Allocation discipline
**`slopos_ostd::mm::heap` is the only kernel allocation surface.** Every kernel crate routes heap allocation through `slopos_ostd`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The `kernel/src/main.rs` global-allocator carve-out above is the lone exception.

The in-place-init primitive (`slopos_ostd::Init<T, E>`, `Zeroable`, `init_from_closure`, `init_zeroed`, `Field<T, U, OFF>` + `#[derive(SlotFields)]`) is **in-house** — defined in `slopos-ostd/src/mm/init.rs` with no external dependency on `pinned-init` or Rust-for-Linux's `pin-init`. Large structs must be constructed via `KBox::try_init(T::init_…())` / `PinBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack. `check_stack_sizes.sh` enforces the upper bound from the other direction. `init_struct_with`'s closure must return `Initialised<T>`, which only `SlotPtr::finish` mints, so a caller cannot claim success without going through the slot; `finish` additionally checks field coverage under `debug_assertions`.

### Authority discipline

**The per-task capability mask is the only authority.** `task.flags` says
where and how a task runs, never what it may do; the privilege bits it once
carried are reserved, and a spawn naming one is `EINVAL`, as is a spawn
naming a retired capability's bit (`caps::RETIRED`). Every check reads the
mask (`slopos_ostd::authority`, its bits stated once in `abi/src/caps.rs`): a
syscall slot through its `cap(...)` clause, anything finer through
`SyscallContext::holds` or a witness, and a kernel thread passes a gate only
where the gate names it. A spawned program holds the universal capabilities
(`ConsoleIo`, `ClipboardGlobal`, `Fate`) its spawner holds, what its spawner
delegates through `SpawnAttrs` — any subset of the spawner's own mask, else
`EPERM` — and nothing for its path; a spawn asking
for ids other than the
spawner's takes `SetId`, one at the `High` tier `SchedHigh`, and a child that
holds a gated capability or runs with ids other than its spawner's real ones
starts `AT_SECURE`. Init holds `CAP_MASK_ALL` and delegates each service what
its service table names (`userland/src/apps/init/service.rs`); a utest
holds what its `utest!` declares, every capability as uid 0 unless it says
`caps = …, uid = …`. exec keeps `caps & universal` in every thread of the
process, so nothing a spawn handed over survives an exec, as Linux keeps no
capability across one for a process that is not root and has no file
capabilities or ambient set; the one exec that keeps the mask is init's own
re-execution of the base's `/sbin/init` as its sole thread
(`exec::exec_keeps_authority`), which loads no other program, and a program
that hands its mask on spawns rather than execs, as the rescue shell and
`priv` do (`slopos_userland::held`); a sender
may signal only a target whose
capabilities it all holds, and `caps_drop` takes capabilities from a whole
process for good, a thread whose clone races it included. A nice value below
zero takes `SchedHigh`. A new check that reads
`task.flags`, a path, a task id or a uid where a capability fits is a second
authority carrier.

**A uid is an identity, never a power.** Each process carries a `Cred`
(`slopos_ostd::process::cred`): real, effective and saved uid and gid and up to
32 supplementary groups, process-wide, copied by fork and passed on by spawn
and exec with the effective ids saved; no set-ID bit is honoured. The id
syscalls follow POSIX's unprivileged rules, and any other id, or `setgroups`,
takes `SetId`. Every VFS entry takes the `Subject` it acts for (the caller's
effective ids and whether it holds `DacOverride`; the kernel's own callers pass
`Subject::KERNEL`) and checks owner, group and mode: search on each directory a
walk crosses, read and write on open, write and search on a parent for a name
made or removed, the sticky bit, and ownership for `chmod`, `utimensat` and the
set-ID bits. `DacOverride` passes every mode check and lets `chown` name any
owner; uid 0 passes nothing on its own. A signal also needs the sender's real
or effective uid to be the target's real or saved uid; `ProcSignal` waives both
halves, and `SIGCONT` to a task of the sender's session needs neither.
`process_list` reports every task to everyone, with its process's login tag:
a number fork, spawn and exec pass on and that only a `SetId` holder sets, at
spawn (`SpawnLoginAttrs`, under `SPAWN_SET_LOGIN`), as Linux's audit session
id is inherited and set once at login. Init gives each session one of its
own (`slopos_abi::task::SESSION_LOGINS`), remoted another (`REMOTE`) and each
extension service its own (`init_control::service_tag`), so a session's
processes are found, and ended, whatever uid they run as, and `login_tag()`
tells a process its own. The ext4 reserve is the
superblock's `s_def_resuid` and `s_def_resgid`, uid 0's. A pty's slave
belongs to whoever opened its master, mode 0620, as Linux's devpts gives it,
judged on the slave an open takes. An `AF_UNIX` name is an inode of type
socket, made through the parent's write and search checks at `bind` and
reached through the name's write check at `connect`; a bound socket holds its
inode open as a descriptor holds a file, so the inode is no other node's and
its volume will not unmount, and a ring's `OP_CONNECT` takes only an absolute
name. `/run` is memory, and each person's runtime directory, `/run/user/<uid>`,
mode 0700, holds their compositor's socket. A symlink in a sticky directory
every class may write is followed only for its owner or the directory's, as
Linux's `protected_symlinks` has it, `DacOverride` or not.

**The session is a person's; the system is uid 0.** Init runs as uid 0 and
starts each person's compositor, terminal and console shell as that person —
their uid and gid, `admin` while the record names them an administrator,
their home and `XDG_RUNTIME_DIR` — roulette and an extension's session
services as the first person, uid 1000, home `/home/user`, remoted as the
person the record names for it, and the login screen as the base's
`greeter` user, uid 50; an extension's services run as its own users, uids
100 to 999. Every root is held
to one ownership map, stated once in `slopos_abi::fs::ROOT_LAYOUT` and
`PERSON_TREES` and mirrored by `scripts/lib/root_layout.sh` and
`tools/initramfs`, whose test holds the three equal: `/`, `/etc`, `/var`,
`/var/lib`, `/var/lib/slopos`, `/var/log`, `/home`, `/media`, `/usr`,
`/usr/local`, `/opt` and `/opt/bin` uid 0's, 0755; `/root` 0700; `/var/tmp`
1777; and the first person's trees — their home `/home/user`, the workspace
`/src` and `/usr/local` — theirs with everything below while
`/var/lib/slopos/owners` names them, a `<uid>:<gid> <tree>` line each, which a
root made for one person does for all three. The first person's home is
0755 until a second person joins, and every later person's 0700. The
image builders lay the map out and write the record on a new image and follow
the one a preserved image carries, read through the listings its trees are
(`fs_tree.py read`, which tells a record that is absent from one it cannot
read), one they cannot read granting nothing; `tools/initramfs` packs the one-person record so a RAM root is never
migrated; and the installer lays out a new root by it and fills a kept one as
its record says. Init makes whatever
directory of the map a root lacks at every boot, and the home the record
names; a record or accounts it cannot read grant nothing and change nothing;
once per root without the record, it hands the first person what uid 0
holds in their trees, never descending into what another uid holds, and writes
the record (`userland/src/root_layout.rs`), so a root that ran everything as
uid 0 is migrated in place, and it rewrites a record from before the map named
the home `/home`, moving nothing. `check_fs_image.sh --map`, the default for
the tests image, holds a root to the map and its record, and a `/usr/local`
the record does not name to being uid 0's, and writable by uid 0 alone, to its
last entry (`fs_tree.py walk`).

**Software is installed for everyone in `/usr/local`, for oneself in
`~/.local`.** While one person has an account `/usr/local` is theirs, and
their tools write it. The add that makes a second (`users add`, holding
`DacOverride`) shares the root (`slopos_userland::root_layout::share`)
before it records the new person, and adds nobody when that fails: the
first person's home closed to others, then every entry of `/usr/local` uid
0's and writable by uid 0 alone, each directory changed before what is in it
is listed, so nothing the first person runs puts a name there behind the walk,
and each mode read again once uid 0 holds the entry; a file with another name
that another uid holds loses only its name there, and a link goes unless
every name it leads through, link by link, is in a directory only uid 0 may
change and it ends at what only uid 0 may change, a directory only within
`/usr/local`; last the record without `/usr/local`. It leaves
`/var/lib/slopos/owners.sharing` from its first change until the record is
written, and init finishes at the next boot a hand-over that mark or a root
of two people whose record still names `/usr/local` shows cut short; none is
undone, and an add on a root whose record cannot be read adds nobody. Owners
change in place, so a descriptor the first person opened there for writing
before writes until it closes; init notes at a boot whose record it read
without `/usr/local` (`root_layout::settled`, `/run/slopos-local`) that none
can be open, `priv` trusts `/usr/local/bin` only then, and an add on a boot
that is not settled says to restart before the new person logs in, since
what they run from `/usr/local` such a descriptor could still change. A manifest the first person wrote stays theirs, and uid 0 installs
over it without believing it. From then on a session installs there through
privd's `org.slopos.software.install` (`allow admin confirm`, `allow
paired`, one at a time), which runs `/bin/tree-install` as uid 0 holding
nothing, installing at `/usr/local` alone from a copy of the tree in
`/var/lib/slopos/tree-install`, which only uid 0 enters and a run cut short
leaves for the next to take away, made through descriptors that follow no
link, of what everyone may read and with every link relative and climbing no
higher than the tree's top before it names anything (`tree::stage`). Since
that copy counts with the install against the mechanism's own `DiskBlocks`
ceiling, a tree through privd is at most half of it. `tree-install` asks for
it when its destination is `/usr/local` and its caller does not hold it, and
`scripts/selfhost.sh install` runs the base's, to which a shell hands privd's
connection. A tree installed in the guest is writable by its owner alone, as
one the host installs is. A person's own software lives in `~/.local`: `tree-install` into
their home keeps the manifest in `~/.local/share/slopos/trees`, `~/.local/bin`
is on their `PATH`, which the desktop's terminal and dock hand what they start
with the rest of the session's environment, the dock reads `~/.local/share`,
and the loader searches `$HOME/.local/lib`. On a boot that began with
`/usr/local` the system's, `priv` finds commands in `/usr/local/bin` too. `apps_test --trees` grades it,
and `dl_test` the loader's search.

**The machine knows its people.** The accounts record
(`slopos_accounts_core::record`) keeps each person's uid, name, home, whether
they administer the machine and their password's Argon2id hash at RFC 9106's
second recommended cost, as a PHC string (`slopos_accounts_core::password`);
whom the desktop logs in by itself; and the next uid a person is given, so
none is given twice. It lives in `slopos_abi::fs::ACCOUNTS_STORE`, a directory
only uid 0 reads, as sealed generations (`userland/src/sealed.rs`, as the
extension store's records are), and a root without one has the one person's:
`user`, uid 1000, the first administrator, with no password and logged in by
the desktop. `/etc/passwd`, `/etc/group` and `/etc/shadow` (0600) are
rendered from it and from the extensions' records
(`slopos_accounts_core::{passwd, group, shadow}`): root, the people, the
extensions' users, then `nobody`, the first holder of a name or uid winning;
each person's group is their own, and `admin` (gid 10) lists the
administrators, which slibc's `initgroups` and `getgrouplist` read. Every
writer holds `ACCOUNTS_STORE.lock` across reading the record and writing the
database, `extctl` after an extension changes among them, so the last
database written is the newest of both. The uids are Debian's division: 1 to
99 the base's own, 100 to 999 extensions' (`EXTENSION_UIDS`), 1000 to 59999
people (`PERSON_UIDS`) and 65534 `nobody`. `/bin/users` lists the people to
anyone and, through privd's `org.slopos.accounts.manage` (`Seal` and
`DacOverride`), for root, an administrator, or a person an administrator's
password answers for, adds one with a home of their own
(`/home/<name>`, 0700), removes one — the home kept unless `--home` says —
renames one, sets a password, promotes and demotes, sets automatic login and
whom remoted runs as (`users remote`, the first administrator unless set);
the last administrator stays one, and the first person, who holds the
workspace at `/src`, stays. A passwordless
administrator who adds someone sets a password first, and an add turns
automatic login off unless `--keep-autologin` says.
`org.slopos.accounts.password` (`Seal`) changes the requester's own password
once the current one is typed, as does the manage action for an
administrator's own; a wrong one costs two seconds, and privd runs the
accounts' mechanisms one at a time. Typed secrets are read from a terminal
whose echo goes off before the prompt and stays off across the exchange, the
one privd gives the mechanism (`userland/src/secret.rs`).
`/sbin/authcheck` is the one program that reads a hash back: it takes
`<name>\n<secret>\n` on standard input, believes `/etc/shadow` only sealed,
which only uid 0 reads, and answers by its status — 0 for the password, 1 for
anything else, 2 when it cannot tell. Init runs `users sync` at every boot,
holding `Seal`, which writes the database and gives a disk root without a
record the one person's; a RAM root keeps every seal for good, so it keeps
slibc's own rows, which are the one person's rendering
(`slopos_slibc_core::passwd::BASE_PASSWD`). The kernel keeps `/etc` and
`/var/lib/slopos` (`KEPT_DIRS`, which base interface 7 counts, since a
base's stores rest on it), so no mount covers the database or the record and
no volume holding either is unmounted, and
the seal keeps a `DacOverride` holder from rewriting either; what such a
holder leaves at a writer's names — a link, a FIFO, a socket, a file uid 0
may not read, a directory — is taken away, a directory moved aside if it
will not go, and never believed, and `/etc/shadow` is written first and from
the record alone. A change the
record took whose database was not written says so, and the old database —
a removed person's hash among it — stands until the next boot writes it. The
installer asks for the first person's name, `user` unless one is given
(`--person`), and a password unless `--no-password`; a kept root keeps its
people. The session's `USER` is the first person's name, and their home the
map's `/home/user`.

**Init keeps the services running.** It starts privd and waits for it,
remoted where the base is paired, the extensions' services, the console's
`/bin/login` on every boot but a test boot, and the sessions — a desktop's
compositor, then its terminal once the compositor is ready or a minute has
passed — and starts privd, remoted, the console's prompt or a session's
leader again when one exits, at most five times in any minute each, as systemd's start limit counts them, the next start
waiting until the minute allows it (`userland/src/apps/init/`). A privd
started again takes no exclusive group while what its predecessor started and
was running when it started runs, since it cannot know which they hold. A dead compositor's
seats are revoked by its exit's cleanup hooks, after init is woken, so the
next compositor waits up to five seconds for them. A base switch restarts
only what it changed, as NixOS's activation does: init compares the manifest
it read before the switch with the new one (`boot_core::base::changed`) and
sends `SIGTERM` to privd or remoted when its program or what its service
entry `reads` changed — privd's policy and the pairing — clearing its start
budget; each drains, finishing what it serves, and its successor starts when
it exits or beside it after five seconds. Init starts or stops remoted as the
new base is paired or not, ends what the old broker ran whenever the pairing
changes — `SIGTERM` and `SIGCONT`, `SIGKILL` to what is left after five
seconds, and only then the new remoted — and re-executes itself once it may
when `/sbin/init` changed; a changed
compositor or terminal is the compositor's to offer, as a session restart
in its system menu, and never forced. Init is single-threaded,
waiting on a poll of pidfds, its ready pipes, the consent ends of sides not
running, the base monitor and `/run/init`. A session
ends as systemd's `KillMode` stops a unit: `SIGTERM` to every live process
carrying its login tag, or, for the first person's desktop, a session
service's, again to each one forked since, and `SIGCONT` to each one stopped,
since every kill walks the task table, `SIGKILL` to all after five seconds,
given up on after ten, so remoted's
processes, privd's mechanisms but those `priv` runs as the person, and
anything else the session did not start survive it. A process that forks
faster than init signals can outlast the ten seconds, and the request that
asked is answered with how many did.
`/run/init` (mode 0666) takes one line per connection, judged by
`SO_PEERCRED` and, as of the request, `SO_PEEREXTENSION` and
`SO_PEERPROGRAM` (`peer_base_program`), four connections
waiting per uid and a desktop's compositor's beside them: a person's `restart-session` and `end-session` (which logs
them out), `lock` and `greeter` for the session their process carries the
tag of — `greeter` from the desktop in front alone — and `locked` and
`unlocked` from that desktop's compositor, which `/bin/session` and the
compositor's system menu send; and from root — uid 0 running a base
program, as
privd's `root` principal is, since `SO_PEERCRED` names no capability and uid
0 alone is no authority — those for any session by its tag, `switch <tag>`,
`login desktop|console <name>` (a console's handing its terminal over
`SCM_RIGHTS`), `reexec`, `extensions`, and `restart-session` and
`end-session` from outside every session, for all of them; each is answered
once done, eight at most waiting on the sessions but uid 0's, and the
sessions uid 0 ended stay down until uid 0 restarts them or a test run fails.
Init re-executes `/sbin/init
--resume <fd>`, as systemd's `daemon-reexec` does, carrying its service table
and budgets, the session table (`slopos-init 2`, a console session's
terminal name among it, so `/run/sessions` keeps naming it and `/bin/login`
keeps relaying), the consent links and the sessions' pair, its listener,
each compositor's session pipe and the request in a pipe, and keeps its pid, its children and its capabilities, and the
extensions' services in pipes of their own; an image that cannot read the
state closes the descriptors its first line names, adopts the privd and
remoted among its children, starts the sessions again, and runs without
consent until the next boot. Before an image of the first version, which
knows one desktop, the sessions end and none starts again until it runs; an
image that resumes the first
version's state takes its desktop over as the first person's, under
`DESKTOP`, and starts privd again to hear the table. A test boot runs the userland tests in a
copy of init (`/sbin/init --run-tests`, holding `TestHarness`), so init serves
while they run, and starts no session until one is asked for or the tests
fail.

**People log in to sessions of their own.** Init keeps a session table
(`userland/src/apps/init/session.rs`): each person's desktop (class `user`),
the login screen (`greeter`) and a shell a console login started
(`console`), each under its own login tag from `SESSION_LOGINS`, never given
twice in a boot; remoted is its person's `background` session. One
graphical session is in front, holding the screen and the input sink: init
brings one there with `seat_switch(task_id)` (`SeatSwitch`, init's alone),
which hands both seats from whoever holds them at the compositor's rank and
routes input to the new holder, and tells each compositor on its session
pipe (fd 5) `front`, `front unlock` (its person proved themselves at the
login screen), `behind` or `lock`; a desktop behind locks and draws nothing.
The table is written to `/run/sessions/<tag>` (`UID`, `USER`, `CLASS`,
`ACTIVE`, `LOCKED`, `LEADER`, `TTY`, through a sibling and a rename, as
logind keeps `/run/systemd/sessions`), which slibc's `getlogin` reads by the
caller's login tag, `ENXIO` outside every session, and sent whole to privd
on a socket pair of its own (`slopos_priv_core::sessions`) whenever it
changes. A person's runtime directory lives while any session of theirs
does, remoted's among them, and the first person's, whom the boot's own
programs run as, for the boot, as a lingering user's does. The boot's
session is the person the record logs in by itself, else
the login screen, which also comes in front whenever nothing else is. The
login screen is the compositor run with `--greeter` as `greeter`, serving
no clients, listing the people and asking privd's `org.slopos.login`
(`allow greeter`: uid 50 running a base program) with the password on a
pipe; `/bin/session login desktop|console <name>` checks it through
authcheck — nothing for a person with none — each wrong one since that
person's last right one doubling the wait before the mechanism answers,
from two seconds to a minute, counted under `/run/slopos-auth`, and asks
init to bring that person's desktop in front, unlocked if
they had one, or to start a shell on the terminal at its standard output.
`/bin/login` prompts on init's console as `greeter` and relays the console
to a terminal of each login's own while init lists that session, since the
kernel hangs a console up for good once a session's leader on it exits, and
closes it once the session ends, so nothing the session left can pose as
the prompt. A compositor init starts takes no seat itself: init hands them
over once it is the session in front, and one started behind is told so
before it starts, locked. A desktop locks from its system
menu, whenever it goes behind, and at Ctrl+Alt+Delete — the secure
attention key, which only the compositor reads, so the screen that asks for
a password after it is the compositor's — and its lock screen hands the
password to `org.slopos.session.unlock` (`allow program /bin/compositor`),
which answers for the requester's own person, counted as a login is; init
counts a desktop locked from the moment it goes behind, only the session's
compositor tells init it locked or unlocked, saying it locked again until
init has heard and unlocked only while in front, and a restart any other
process of the person asks for brings the desktop back locked. A compositor init starts is handed the seats only once
it says it is ready, so one that never draws leaves the console its keys,
and reads what init said before each pass's input, so a desktop told to
lock before the seats came back takes no key unlocked.
The clipboard is a session's: the kernel keeps one per login tag
in `SESSION_LOGINS`, eight at most, the one copied into longest ago going,
and a process of no session has none. Each person's processes are charged
to a quota account of their own (`quota::person_account`), a quarter of
`MAX_PROCESSES` and of `MAX_TASKS` for all of them together — every task
counted, a fork's and a thread's as a spawn's (`charge_clone`) — and a zombie
of theirs holds one of those tasks until it is reaped
(`ChargeSlot::bequeath_into` onto their row, which outlives the process's
own) instead of counting against its parent's budget, so one person's fork
bomb, or what they leave unreaped, leaves another's session running. The
rows are the boot's first sixteen people to run anything; a later one
debits the root and keeps the parent's budget. Memory
stays the machine's, under one commit ceiling. A key reaches the console's line discipline only
while nothing holds the input sink, so nothing typed into a desktop, a
password among it, is there for a reader of the console.

**A peer is told who connected, for as long as it runs what it ran then.**
An `AF_UNIX` socket records an `Identity` (`slopos_ostd::process::identity`:
the pid `getpid` answers, the effective uid and gid, the program, and the
process and program instance it was taken from) at `listen` for the
listener, at `connect` for the connector — for a ring's `OP_CONNECT`, the
task whose `ring_enter` performs it, asked at each try — and at `socketpair`
for both ends. `SO_PEERCRED` and `SO_PEERPIDFD` read the ids and pid as
recorded, as Linux's do (`{0, -1, -1}` and `ENODATA` without a peer): not an
id change, not an exec, not the descriptor passing to another process moves
them. `SO_PEERPROGRAM` at `SOL_SLOPOS` reads the program and
`SO_PEEREXTENSION` its activation's label, each only while the process it was
taken from is live and runs the program instance it ran then
(`Identity::now`): an exec, an executable mapping of code the program may not
run, or its leader's exit, before its parent is told, end it for good, and it
reads as no program, never as a later program's. Every change of a process's
program moves its instance, a 64-bit counter, but init's re-execution of the
base's `/sbin/init`, which keeps its capabilities (`exec_keeps_authority`)
and so its instance, so the consent pair init made at boot, under the booted
generation, stays init's. The socket layer is told who connects; nothing below the syscall layer reads the current task for it. Task ids are
never reused, so the pid is a handle: a peer's pidfd is handed out even once
the peer is gone, and is readable at once; a live one turns readable when the
peer's leader thread exits. A process's program is `Base(inode)` of a base
generation when exec or spawn loaded it with every link of its `#!` chain and
its ELF interpreter served by `BASE_FS`, `Extension(inode)` of an activation
when the file named is that activation's and every other link the base's or
that activation's — each interpreter named by an absolute path and each script
by the absolute path it was found at, since its interpreter opens that
spelling again — and `Other` otherwise. Fork copies it with the address
space. An exec that begins with another thread alive attests nothing, and one
that fails once the old image is gone leaves `Other`. An executable mapping of
anything but a base file or, for an extension's program, a file of its own
activation — anonymous memory, a memfd, a file elsewhere, another
activation's — and an `mprotect` making any page executable that was not,
decided under the lock that applies it, make the process `Other` before that
code can run, until its next exec. The inode names its generation, so
`SO_PEERPROGRAM` answers the path in the program's own generation,
`/opt/<name>/<path>` for an activation's, and nothing once that generation is
freed. `pidfd_open` takes any process's leader, as Linux's does. A
descriptor passed on or inherited speaks as its connector for as long as the
connector runs its program. uid 0 alone is no principal, at privd or at
`/run/init`; what keeps an extension from root is that none runs as uid 0.

**privd runs what the session may not.** `/sbin/privd`, which init starts as
uid 0 holding `slopos_priv_core::PRIVD_CAPS` and waits for, listens on
`/run/privd` (mode 0666) and runs a named action's mechanism — the base program
`/usr/share/slopos/privd/actions` (`assets/privd/actions`, sealed in the base,
grammar `slopos_priv_core::policy`) names for it, or an extension's own — as a
fresh child holding the action's capabilities as uid 0, an extension's as its
first user, or one whose `uid` is `requester` as whoever asked — their uid,
their own group and `admin` while the record names them an administrator, and
their session's login tag — an action only `root`, `user` and `admin` may ask
for, whose capabilities stay within `slopos_priv_core::REQUESTER_CEILING`,
with an environment of `PATH`, `LANG`, `PRIV_ACTION` and
`PRIV_UID`, the requester's uid, alone, cwd `/` or the directory the requester sent, and the
requester's descriptors as stdio, refused when the directory's path no
longer leads where the descriptor did. Who asks is the connection's recorded
`Identity`, its program read again before every verdict and before the
mechanism starts, so a requester whose base program has stopped running since
it connected — it exec'd, mapped code from elsewhere or exited — is refused:
`root` is uid 0 running a
base program, `greeter` uid 50 running one, `paired` is `/bin/remoted` on a
base carrying a pairing and nothing else, `user` is a person the accounts record names and `admin` one it
names an administrator, either from any program but the paired agent and read
from the record as the request comes, and `program <path>` is a static base
program or, in an extension's action, a program of its own activation. An
action's `allow` lines are tried in order and the first that matches decides
at its level (`slopos_priv_core::policy`, as polkit's `yes`, `auth_self` and
`auth_admin` do): none runs it, `confirm` asks for a click and `password` for
the person's own password. Only the session the requester's process carries
the tag of is asked, as init's table has it: its desktop's dialog while that
desktop is in front and unlocked, a click only from the person whose desktop
it is, so anyone else types their password and one without a password, who
confirms by a click where it would be typed, cannot; or, for a console
session's own person, a prompt privd sends the requester (`Reply::Prompt`),
which reads their password at its terminal with echo off and sends it back,
whatever the level; any other request that asks is refused, and a desktop
that goes behind or locks has what it was asked refused. `user alone` runs
it while no other person is logged in at a desktop or a terminal, and is
`admin` otherwise; `greeter alone` runs it while nobody is. A person no line
matches, at an action with an `admin` line, is answered by an
administrator's password, typed into a dialog alone: the dialog lists the
administrators who have one, never the asker, and refuses when there are
none. With no line matching, it refuses. A password reaches privd in the
consent answer and `/sbin/authcheck` on a pipe, never a command line; checks
run one at a time, and each wrong one doubles the wait before that person's
next check, from two seconds to a minute, for the life of privd. Once it is
answered privd judges the request again, and runs it only if it would now
run unasked or ask the same of the same person, the administrator who
answered still listed. `keep <seconds>`, up to 900, runs a request unasked
once one of the same uid and session (its login tag and eldest task), started
by the same live task, of the same action read from the same place and
asked the same way, command line and directory was allowed within that time
by someone who could answer it now, as polkit keys a kept answer on a live
parent, so one given to a person's shell covers that shell and what it
starts, as sudo's covers a terminal, and nothing else in their session; it is
forgotten when privd reads the extensions' actions again and
never kept for a requester carrying no login tag; a mechanism leaves who may
ask to privd.
An action marked `medium` exists only on a boot the kernel says served the
install medium (`BOOT_FLAG_MEDIUM` in `sys_info`): off it privd answers it as
no action, and so does a requester's own reading of the policy
(`slopos_userland::privd::policy`).
argv outside the action's shapes, a request carrying other than its
descriptors, a requester whose process has exited — before the question and
after it — and every lookup that fails are refusals. A connection serves its
requests one after another, every reply naming the request's id, so a requester
on an inherited connection passes over what an earlier request left; a signal
relayed before the mechanism starts withdraws the request, a second one stops
the requester waiting, and a mechanism whose requester leaves is not stopped,
though the pipes its requester relayed a terminal through close with it and a
terminal of its own hangs up. One
uid running one program holds at most eight connections, every uid but root's
and the paired agent's at most 32, so no person crowds out another, and those
uids 128 together, and one that leaves a reply unread for five seconds loses
its connection. A program started without its action's
capabilities calls `slopos_userland::privd::elevate`, which sends its own argv
through the connection it inherited (`PRIVD_FD`, held to being the real
privd's) or a fresh one and exits with the mechanism's status, relaying a
terminal's output through pipes, and for the installer, the accounts'
mechanisms and the rescue shell, whenever standard input is a terminal, every
byte through a terminal of its own, locked against opens by its path once the
requester holds it, fed nothing before the mechanism starts and relayed only
once the requester's terminal is raw, which it stays while the mechanism
runs, since the mechanism runs in another session; started by privd, a mechanism acts
only for the action `PRIV_ACTION` names; while init starts privd again, a
fresh request waits two seconds for it to listen. On `SIGTERM`, which init
sends when a base switch changes privd, it drains: it stops listening, lets
go of the consent pair, refuses each request it reads from then on as
restarting, and exits once every request it took is answered, so a mechanism
mid-run is never cut, and starts no mechanism from then on. Init starts the
new base's privd when it exits, or
beside it after five seconds of draining (`service::DRAIN_MS`), which a
mechanism waiting on its terminal outlasts; the new one takes no exclusive group
while a mechanism the old one started runs, and a program whose requests span a
switch waits a minute for it (`privd::SWITCH_WAIT`). Consent travels socket
pairs init makes and keeps for the boot, one per desktop or login screen
running at once (`sessions::LINKS`, four): privd holds them at fd 5 to 8
with a broker on each and, started `--hear-sessions`, the table's pair at fd
9, which init makes anew for each privd it starts so one still draining
reads end of file; fd 4, where a privd of a base from before the table looks
for its one link, is left empty, so that privd asks nobody on a screen that
may be anyone's, and one told to hear a table and handed none reaches nobody
and knows nobody logged in. Each compositor holds its own link at fd 4, and
each side refuses a pair init did not make. Init holds both ends, so neither reads end of file when the other side exits,
and each new instance first sends
`RESET` naming an epoch of its own and acts on nothing until a `RESET_SEEN`
naming it comes back (`consent::Link`), sending the `RESET` again with each
`RESET_SEEN` it writes until then; a side that reads a new `RESET` drops what it
had pending — privd interrupts every question — and init sends one on behalf
of a side that exited, and discards what is written to a side not running.
privd puts a question to the compositor only after a `READY` sent since the
last, so one question at most is in flight and no frame is ever left torn in
the pair's buffer. `READY` names the version the compositor speaks, the one
the running base's interface says its privd reads, since either side of a
live base switch may be the older: one of
the first, which names none, is asked only for a click, in that version's
words, so a desktop older than a live base switch still confirms and refuses
what needs a password until its session restarts; from the second on a
question says what answers it and whose request it is, the compositor says
`SHOWN` once it is on screen, which is when the requester hears it is being
asked, and an answer carries the password typed and whose. The compositor
draws one question at a time over every surface, naming the person asking,
the whole command line and its directory wrapped (privd asks about neither
past 320 escaped bytes), Deny focused, or the password field of a question
that takes one, masked, with Up and Down choosing among the administrators
listed; Allow by a click, or by Enter once Tab, or Left or Right between the
buttons, has moved to it, and a question that takes a password only once one
is typed, which Enter in the field also allows; input is ignored for 750 ms
after the frame that first shows the question and after each move. No client
sees what is typed there: no key pressed while a question is up, or after
one that takes a password goes unanswered until no key has been pressed for
1.5 s, reaches a client, nor does any pointer press or motion while one is
up, and a key pressed into it keeps its repeats and its release from every
client, since a release names its key. Until the compositor says it can
draw, and with none, an action that needs consent is refused, at most eight
questions wait, four of one uid's, a question unanswered for 120 s from when
privd sends it is a refusal, and a refusal refuses what that uid had waiting
and what it asks in the next five seconds; a wrong password refuses only its
own request.

**`priv` runs a command with power over the machine, as whoever asks.**
`priv <command> [argument...]` asks privd for `org.slopos.priv` and `priv -s`, or
the shell named with nothing after it, for `org.slopos.priv.shell`
(`/bin/priv`, `apps::r#priv`), both `allow root` and `allow admin confirm`,
the first alone with `keep 300`, since a shell runs whatever its input says,
and both with `uid requester`: privd starts `/bin/priv` again as the person,
as `run0` has the service manager start a command, holding `REQUESTER_CEILING`
— `Mount`, `NetAdmin`, `ConsoleConfig`, `SysInspect`, `Clock` and
`DacOverride` — on a terminal of its own whenever standard input is one, in
their directory, with nothing else of theirs but their name and home in its
environment, never for the paired agent; a standard user's is allowed by an
administrator's password and runs as the standard user. That copy runs the
command holding the same (`slopos_userland::held`), with `PATH`, `HOME`,
`USER`, `LOGNAME`, `SHELL`, `LANG`, `SLOPOS_PRIV` and, on a terminal, `TERM`
alone, and ends as it ends; a lone command's terminal of its own takes no
Ctrl+Z as the command starts, since nothing would continue what it stopped.
The command is a program in one of `DEFAULT_PATH`'s directories that only uid
0 owns and can write, with every directory above it — `/usr/local/bin` only
once uid 0 has held `/usr/local` since the boot began
(`slopos_userland::root_layout::settled`) — named there by its path or found there by its
name, and leading, through links each in a directory only uid 0 can change, to
a file only uid 0 can change in one, which is what runs, and so is the
interpreter a script names; never `/bin/priv`, nor the shell told to read what
it runs; the requester sends privd the path it found, which the dialog shows
and a kept answer covers, and the copy privd starts holds it to the same rule.
`-s` runs `/bin/shell`, which finds `SLOPOS_PRIV` in its environment and,
while it holds more than every program does, hands it on as the rescue shell
does, naming it in its banner and before its prompt. The ceiling leaves out
whatever writes a raw disk, the base, an extension, a seal, the firmware's or
the loader's variables or another identity, so the raw disks, the base, the
user database and every sealed file refuse what `priv` runs, though `Mount`
mounts any ext4 volume nothing holds, and a mechanism it runs, `halt` and
`bootctl` among them, asks privd as the person. `DacOverride` reads
`/etc/shadow`'s Argon2id hashes and every person's files, so an administrator
who allows a standard user's `priv` allows them that; what `priv` makes is the
person's, so a file it makes in a uid-0 directory stays theirs until a
`priv chown` gives it to uid 0; a working directory is a path, which the person's
processes can move from under what runs there; and a kept answer repeats its
command with whatever input the person's shell and what it starts give it, as
sudo's timestamp lets them run anything. A requester already holding the
ceiling runs the command itself, handing on its whole mask, which in a rescue
shell is everything. `priv_test` grades it.

### Licensing discipline

SlopOS is `GPL-3.0-or-later`, except its C library and the CMake platform
modules in `toolchain/cmake/Platform/`, which are `BSD-3-Clause` like the
project they are written to be contributed to.

**No verbatim code from a GPL-2.0-only source, ever.** GPL-2.0-only (Linux, the
seL4 kernel, `rust/kernel/**`) and CDDL (illumos) are incompatible with the GPL
version SlopOS ships under, and CDDL is incompatible with every GPL version.
Concepts, algorithms and interface facts are free to take — ABI numbers, `errno`
values, ioctl codes, struct layouts and hardware register offsets carry no
copyright, which is why the ABI-compatibility work is sound. Upstream *prose* is
not: never paste an upstream comment block, design essay or documentation
paragraph. When citing an influence in a comment, name the **specification or
the documented behaviour**, not the implementation file — "values follow the
Linux x86-64 ABI" is an interoperability statement, "derived from
`kernel/sched/fair.c`" is not. Keep the existing influence comments; they are
contemporaneous evidence that what was borrowed was the concept.

**The C library is `MIT OR Apache-2.0` and stays permissive.** Every program
links it, and a GPL program's licence governs the whole it is linked into: git
is GPL-2.0-only, so whoever distributes it takes the MIT option. `slibc/`,
`slibc-core/` and `abi/` therefore carry that licence and nothing copyleft goes
into them — no code from glibc or any other GPL or LGPL source, and no
dependency on a GPL crate, this tree's own included.
`scripts/check_libc_license.sh` holds every crate `cargo metadata` resolves for
the library to MIT and its sources to including nothing from outside those
three directories, bar the `toolchain/libc/` fork the headers are rendered
from; a third-party crate takes an entry in `slibc/NOTICE`, which ships with the
licence texts in `/usr/share/licenses/slibc/`.

**Fonts load at runtime; never `include_bytes!` one into a shipped binary.**
`assets/fonts/*.ttf` are SIL OFL 1.1 and ship as separate files in
`/usr/share/fonts/`, which is aggregation and imposes nothing on the kernel.
Baking a font into `kernel.elf` or a userland binary would put OFL §5 ("must be
distributed entirely under this license") in direct conflict with GPLv3 §5(c)
("license the entire work, as a whole, under this License"). The
`include_bytes!` sites in `font/src/` are `#[cfg(test)]`-gated and must stay
that way. Each font's license text ships beside it, in `assets/fonts/` and on
the installed images.

**The CA bundle is data, loaded at runtime, like a font.** It is MPL-2.0 and
ships as its own file with its license text beside it in
`/usr/share/licenses/ca-certificates/`; nothing compiles it in, and only a host
test reads it from the tree.

New third-party code linked into a shipped binary needs an entry in
`NOTICE.md`; `MIT OR Apache-2.0` crates elect MIT there.

### Task-ownership discipline

**`KArc<Task>` is the only owning handle for a task, and `TaskRef` is the only
way to hold one outside `slopos-ostd`.** A raw task pointer says nothing about
whether the task is still alive, whether anyone else is mutating it, or who is
responsible for tearing it down; the owning handle says all three.
CI-enforced by `scripts/check_task_ownership.sh` (run from
`just check-framekernel`, hard-failing), whose header documents each check.

The invariants the gate protects:

- **I1** Raw task pointers exist only inside the ostd placement/link
  primitives, the PCR slots, and the pre-heap `.bss` stubs — the surfaces the
  gate lists as sanctioned. Everything else binds `&Task`, a guard, or a
  `TaskRef`.
- **I2** Linked implies owned: a task on any queue, inbox or wait map has its
  owning reference held *by that container*, moved in and out only through
  `slopos_ostd::task::placement`.
- **I3** The final drop never runs on the dying task's own stack, never with
  IRQs disabled, and never under a lock. `task_put` is the sole release; its
  destructor frees to the buddy allocator, whose reuse path performs
  synchronous cross-CPU TLB drains.
- **I4** Wake and enqueue allocate nothing; a `KArc` clone is one atomic.
- **I5** `current` is a borrow (`CurrentTask`), never an owned handle. PCR
  offset 40 stays raw and ABI-frozen — `__safestack_pointer_address` reads it
  from asm on every instrumented prologue. `IdleTask` is the same shape for the
  idle slot. The CPU separately holds *one* owning reference to the running
  task in `PCR.current_task_ref`, which is what lets a task be dispatched
  directly from its predecessor: a reference living in the dispatching frame
  would be owned by a stack the successor outlives. Idle owns none, being
  pinned by `task_is_dispatch_pinned`'s idle disjunct.
- **I6** Lookup is weak-upgrade only. Fabricating a strong reference from a
  raw pointer is not a thing that can be written.
- **I7** `KArc` is fallible everywhere and saturates on refcount overflow.
- **I8** **A task only ever exits from its own context.** Kill is a flag:
  `task_kill_and_wake` marks the target and wakes it, every blocking primitive
  but one returns `Err(WaitAbort::Killed)`, and the task unwinds by
  *returning*, so destructors run on its own stack at a point it chose. An
  owning task handle may therefore live in a stack frame that blocks. The one
  is `wait_event_uninterruptible_timeout_until`, deadline-only and capped at
  `UNINTERRUPTIBLE_MAX_MS`, for a block request the device already owns:
  abandoned, a write could land after a later one to the same sectors. A new
  caller owes the same kind of reason. The residual is a kernel loop that
  reaches no blocking primitive at all: nothing can stop one, and
  `task_terminate`'s remote branch survives only as the bounded shutdown
  fallback and the IRQ-exit self-kill.

I1–I7 above are the tree's naming for this discipline. The proof in
`verification/proofs/task_ownership.rs` checks a model of it under the names
T1–T7, and `verification/STATUS.md` records which parts of the tree that model
does *not* reach — weak-memory ordering, the intrusive links and the raw-pointer
provenance are audited, not proved. (I1–I4 elsewhere in `STATUS.md` are
`mm::frame`'s refcount invariants, a different set.) Read that
file's header before changing `task_is_dispatch_pinned`, because the proof
keeps verifying whether or not the model still describes the tree.

## Testing Guidelines
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over serial in KTAP grammar. A test flagged `FLAG_EXPLICIT` runs only when a `tests.run` pattern is its full name — `utest!(…, explicit)` is that for a userland test a host drives, its log uncaptured — and `stest!(…, kind = Userland)` runs a kernel test in the userland phase, sorted among the userland tests. A utest runs as uid 0 holding every capability unless it declares `caps = …` (`none`, or the gated capabilities it holds beside the universal ones) and `uid = …`, which a test of an unprivileged gate or of the person's work sets to `caps = none, uid = 1000`. A test answers the desktop as a person would by `test_key` (`TestHarness`, `ENOSYS` but on a test boot), which hands the input sink's holder a key a keyboard sent after its layout, past the diagnostic console and the TTY; `userland/src/bin/tests/support/asking.rs` is the requester and the keys `privd_test` and `extension_action_test` answer privd's dialog with. The Go host wrapper (`tools/run_tests/` → `builddir/run_tests`) parses that stream into a live progress bar + per-failure detail. `just test` builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and exits 0 green / 1 on any failure.

**Run `just test` before sending changes.** A green `just test` is necessary but **not** sufficient — it runs neither the framekernel gates nor the three boot-log ratchets, all of which CI runs and any of which can fail on a commit whose tests pass. See **Pre-commit (MANDATORY)** below for the full sequence. For manual inspection use `just boot` or `just boot-log` (serial transcript in `test_output.log`; `VIDEO=1` for a framebuffer). Note regressions or warnings in your PR description.

### `just test` recipes
- `just test` — full run; dotted progress, per-failure blocks, summary line.
- `just test 'slopos_mm::*'` — run only tests whose `<module>::<name>` matches the glob (positional — a `FILTER=` prefix is passed through literally and breaks the first glob). Comma-separated globs supported (`'mm::*,core::*'`). Filtered runs that create files must include `'*ext2_aaa*'` so the lex-first ext2 root mount runs.
- `just test-rerun-failed` — re-run only the names in `builddir/last-fail.list` (written automatically after every non-aborted run).
- `just test-verbose ['glob']` — also dump captured klog of every passing test.
- `just test-quiet ['glob']` — render only failures + summary; suppresses pass lines on the wire too.
- `just test-raw` — passthrough QEMU stdout verbatim (KTAP + klog interleaved). Last-resort debugging.
- `just test-json builddir/events.jsonl` — also write one JSON event per line to PATH (machine-consumable).
- `just test-userland-only` — skip the kernel phase; run only the userland (`utest!`) phase.
- `just check-tests-host` — run the Go wrapper's own unit tests via `go test ./tools/run_tests/...` (host-side, no QEMU).
- `just check-test-count` — count-regression CI guard; fails if total planned tests across phases drops below `TEST_COUNT_BASELINE`. The default lives in `scripts/check_test_count.sh` and is written down only there — read it from the script rather than restating it here, and bump it there when the suite grows. Measure the new value with `TEST_COUNT_BASELINE=0 scripts/check_test_count.sh`; never guess it.
- `just check-fs-image` — hold the image the suite just wrote to `e2fsck -fn`, to being at rest — `Filesystem state: clean`, no `needs_recovery`, an empty journal — and to the ownership map (`--map`, the default for the tests image). Runs in CI after the test capture; an image SlopOS wrote that e2fsck rejects is a bug in SlopOS.
- `just test-persist` — two boots of one image with no rebuild between: write + `fsync` under `/var` on the disk root, power off, read back. In CI after `check-fs-image`. Needs its own boots and cannot reuse the shared capture.
- `just test-rude-exit` — one boot that fsyncs a file into the root's journal and ends the machine holding it, then `scripts/check_fs_replay.sh`: the image must need recovery, the file must be reachable only through the journal, and after `e2fsck -E journal_only` the image must pass `e2fsck -fn`, be at rest and hold the file. In CI after `test-persist`. The kernel test runs only when `tests.run` names it exactly (`FLAG_EXPLICIT`), since it ends the machine.
- `just test-usb` — the USB check: a boot from the tests base, with no root disk and none of the suite's (`root=initramfs`, `QEMU_NO_ROOT_DISK=1`, `QEMU_TEST_DISKS=0`, so no image is built for it), carrying both of QEMU's xHCI models, `qemu-xhci` on MSI-X and `nec-usb-xhci` with `msix=off` on MSI, each with two USB 3 and five USB 2 root ports: a SuperSpeed stick, QEMU's full-speed hub with a stick, a tablet and a mouse behind it, a high-speed stick and a full-speed keyboard, the keyboard and the tablet bound to the stdvga `video0`; on `qemu-xhci` a high-speed ext4 stick as well, and a `usb-net` behind the hub on a SLIRP network of its own, `10.0.3.0/24` with the echo peer at `10.0.3.100`, and for `nec-usb-xhci` a read-only ext4 drive the guest asks for. `qemu_run.sh` opens a QMP socket when `QEMU_QMP` names one, and `scripts/test_usb.py` drives the boot at the guest's `USB-TEST:` lines: it deletes and re-adds devices through `device_del` and `device_add`, each stick a `-blockdev` node so it survives its device, and injects keys, buttons and motion through `input-send-event`, naming `video0` for a USB keyboard or the tablet and nothing for the i8042 or the newest usb-mouse. The guest half is kernel tests registered `FLAG_EXPLICIT | FLAG_UNCAPTURED`, which the recipe names in `tests.run`, and the explicit userland tests `usb_disk_test`, `usb_net_test` and `usb_shell_test`; they hold both controllers running, every device enumerated, each 1 MiB stick bound by the `test-hooks` driver `usb-test`, which sends TEST UNIT READY over its bulk pipes, stalls its bulk-in with a stray read and sends TEST UNIT READY again once the endpoint has recovered and the stick's halt is cleared, every keyboard, tablet and mouse tried first by `usb-test-hid`, which abandons a read of its idle interrupt endpoint, waits for the endpoint to come back idle and declines, and bound by `usb-hid`; every posted report, abandoned as a halt leaves it, to being posted again by recovery; a held USB key to repeating and the i8042's second press of a held key to being a repeat; Caps Lock on a USB keyboard to lighting both USB keyboards and the i8042; Alt+PrintScreen and a command key to running the command; the tablet, a USB mouse and the PS/2 mouse to moving one cursor, and a button to staying down while either the tablet or a mouse holds it; a keyboard pulled with Shift held to leaving nothing shifted unless the i8042 still holds it; the `usb-net` to being `eth1` beside virtio-net's `eth0`, leasing `10.0.3.15`, routing its network through itself and resolving its gateway, then to leaving with its interface, routes, neighbours and DHCP client and coming back as `eth1`, and to carrying TCP to the echo peer, segments of every length from 1 to 128 bytes; every device to leaving and returning twice with no slot, claim or page left behind, the ext4 stick `sda` each time; the ext4 stick mounted, written and fsynced, then pulled under a writer that fsyncs in a loop and is refused within a minute, the mount answering `EROFS` to a create, a mkdir and a write, a read of a block never cached failing within ten seconds, `umount` releasing it, and plugged back as `sda` holding what was fsynced; the read-only drive answering `BLKROGET` with 1 and mounting read-only; each controller to interrupting for its pulls and its plugs; the dynamic MMIO registry to eight free ranges; the kconsole listing to running; a shell on the console to running what the host types on a USB keyboard; and last, as a kernel test that runs in the userland phase, each shutdown hook to leaving its controller halted, reset (DCBAAP cleared) and off the bus. The host holds the log to the same, to every device enumerated and removed as often as it was plugged and pulled, to the listing's lines, to QEMU's `ps2_set_ledstate` trace showing Caps Lock, to no enumeration failing, to neither controller answering its first command only when polled, to the ext4 stick named `sda` and the drive `sdb` and the NIC `eth1` each time they were plugged, the stick's image to `check_fs_image.sh` and to holding the guest's fsynced file, and the suite to green. In CI after `test-rude-exit`.
- `just test-capacity` — the capacity check: build (once, then preserve) a 16 GiB ext4 volume, attach it as `nvme0n3`, and let the suite mount it, walk it, write to it and report. Separate from `just test` because the image takes minutes to build and ~70M of host disk once populated; what CI grades per run is the cheaper `check-fs-throughput` ratchet below. `CAPACITY_IMAGE_SIZE` overrides the size; the guest measures a *mount* in device reads rather than in seconds, because reads are deterministic and wall time is not.
- `just test-toolchain` — the toolchain check: build the self-hosting root, boot it twice at 6G with no rebuild between, and let `toolchain_test` hold the toolchain to its manifest and the clone to its vendored crates and climb the ladder on both boots — the clone's `git status` must be clean, since nobody has edited that tree — while `reboot_clone_test` makes a clone on `/` on the first boot and finds it intact on the second; the host holds the root to `e2fsck -fn` after each. Without a toolchain the root still carries the clone, and the run stops after one boot: in CI it grades the seeded clone, its vendored crates and the grown root. Separate from `just test`, where the same utests pass by reporting that the root carries no toolchain.
- `just test-install` — the install check: boot from `builddir/boot-disk.img`, one disk in the bare-metal layout (an ESP holding Limine and `limine.conf` under `\EFI\SlopOS\` and at the removable-media path, the boot partition with a kernel per slot under `/boot/<slot>/` and the tests base as both `/boot/base/current.img` and `previous.img`, the tests image as the root partition every slot boots with as `root=PARTUUID=`, and the crash partition), and across the resets of one QEMU let `install_test` register SlopOS's firmware entry first in `BootOrder`, clone slot a into b with `/bin/bootctl`, boot it once through the Boot Loader Interface's `LoaderEntryOneShot` and through that entry, commit it as `LoaderEntryDefault`, then boot once into a slot whose kernel panics with `panic=reboot` and see the reset land on the committed default, through the entry again, and find the panic's record moved from the crash partition to `/var/log/crash/` and reported by `bootctl status` as that slot's last boot; then the same for a slot that takes the format-free abort; then install a base the test repacks from the current one under another build tag, named by its SHA-256 once another digest is refused, which moves the running base to previous and runs at once, boot it and find it kept, roll it back, which runs the first at once, and boot that again, and install one naming an interface the kernel does not serve, which the next boot passes over for previous, freeing its pages (the kernel's `base_tests` grade them on every boot); there a base installed replaces current alone, since previous runs, a kernel serving neither base is refused a slot, and a base the default slot's kernel would not serve is refused while that slot is the default. The host then holds the ESP to the bytes it built, so no commit touched `limine.conf`, each slot to its kernel alone, both bases to `bootdisk base`, current to the base the disk was built with after the last rollback, and the root partition to `e2fsck -fn` and to being at rest. Boot-disk runs use a second, pinned OVMF (`third_party/ovmf-nv`, Arch's `edk2-ovmf`), because the nightly the ISO boots needs a secure varstore and keeps UEFI variables in RAM.
- `just test-installer` — the installer check: the tests system on a USB stick (`INSTALL_STICK`, `qemu-xhci`), which the kernel reads as `sda` and the installer never offers — write-protected but for the blank disk's install, which takes a writable copy that only its GPT disk GUID excludes and that the host holds to the ISO's bytes afterwards — its install medium carrying the payload in the partition the kernel mounts over USB, which the module Limine loads does not carry, on the pinned NV-varstore OVMF with the varstore kept from one QEMU run to the next (`OVMF_VARS_FILE`) but for the reinstall's, which starts afresh as a person picking the stick from the firmware's menu does, and one NVMe disk without the suite's own (`QEMU_TEST_DISKS=0`), one of three `scripts/make_installer_disk.sh` makes: a blank one, erased; another system's, holding an ESP with `\EFI\other\`, a data partition and the firmware entry `installer_test` gives it, which SlopOS goes beside in free space; and one whose partition is reused as the root. A second blank disk, `optical`, is installed from the ISO in a CD drive the kernel has no driver for (`INSTALL_CDROM`): its install must say `the medium carries no toolchain`, and its disk must clone a slot rather than build one. `installer_test` runs `/bin/installer`, for the blank disk as a person does — typed at `/bin/shell`, holding nothing, so it asks privd, its questions answered on standard input — for the other system's disk and the reinstall with flags, a root size among the other system's, and for the partition to reuse as a window does, asking privd as `root` for `installer --serve` and speaking its protocol, an engine whose input ends before an install writing nothing and an install of a proposal it did not make last refused; booted from the disk with the stick gone, through SlopOS's own firmware entry (`BootCurrent`), it takes the loop once — `scripts/selfhost.sh install tests` and `install-kernel tests`, a boot of slot b and `bootctl commit` — after holding the other system's entry to `BootOrder` and Limine's menu. The blank disk then takes a reinstall from the stick that keeps its root, with a file left in it and a base left in slot a as a disk laid out before the bases moved beside the slots holds one, which the reinstall removes once the new configuration is written, and a boot that finds the file and the old slots' state gone. A built system must run the base it built as well as its kernel. The host holds each table to `sfdisk --verify`, each FAT volume to `fsck.fat -n`, each boot partition to a kernel alone per slot and two bases `bootdisk base` takes, each root to `e2fsck -fn`, to being at rest and to sealed base mount points, the other system's entries, partition and files to their bytes, its root to the size asked with the crash partition after it and free space after both, and a reused root to a new PARTUUID. Each installed root, the reinstalled one too, must hold the install's log, and an install of the other system's disk confirmed after its table changed must write nothing. `INSTALLER_PAYLOAD=0` installs without the toolchain, cloning a slot where it would build one, and needs no `just toolchain`; `just test-installer foreign` runs one disk.
- `just test-remote` — the remote-control check: an installed machine under QEMU, a boot disk whose base is paired with a broker on this host's loopback (SLIRP's 10.0.2.2), and a root carrying a decoy pairing under `/etc/remote` for a second, live broker; driven through `scripts/remote.py`: the agent's identity and build tags; commands' stdout, stderr, exit codes, cwd, env, stdin, a timeout and two at once; an 8 MiB file pushed and pulled back byte-identical; `/dev/kmsg`; the base's pairing unwritable and the decoy never dialled; a second daemon, preferred as the newest; a base built under another tag installed and running at once, the agent reporting its tag and the kernel logging `BASE: generation 2 is the base`, and then a kernel put into the spare slot, the boot into it and its commit, keeping that base current and the one that ran before it previous, every `bootctl` asked of privd as the paired agent; and `/bin/halt`, which must end QEMU.
- `just test-install-guest` — the two loops in one QEMU: a clean tree, `just toolchain` and the self-hosting root; slot a is the optimized tests kernel, and `install_test`, finding a workspace at `/src/slopos`, fetches the host's `HEAD` into its clone, checks it out and runs `scripts/selfhost.sh install tests` and `install-kernel tests` there with a fresh `SLOPOS_BUILD_TAG` — a build-time variable that appears in `uname -v` and in the boot log's `BOOT: kernel <path> (<n> bytes), build tag <tag>` line, and is otherwise unset — which build the tests userland and base and install the base as current, running it at once, and the tests kernel into slot b, then checks the tree's own branch out again; the run boots them once, and that boot must report the tag in `uname -v` and in the base's `/usr/share/slopos/build-tag`. The kernel the guest built then commits a change on the fetched `HEAD` in a scratch clone and pushes it into a scratch repository, which the host fetches and holds to that commit's parent being `HEAD`. The run then commits and rolls back as `test-install` does, and the host holds slot b's kernel and the current base to the root's `kernel-tests.elf` and `initramfs-tests.cpio` byte for byte, and the boot log's `BOOT: base current` line to the base's size. `INSTALL_TIMEOUT_SECS` defaults to the self-hosting budget.
- `just test-selfhost` — the self-hosting check: needs `just toolchain` and a clean working tree (the host grades the guest's build of `HEAD` with its own gates and tests). The guest, booted on the optimized tests kernel (`release-tests`, gated by its own allowlists under `scripts/gates/{stack,vector}/`), fetches the host's `HEAD` into the self-hosting root's clone and checks it out — refusing a tree with uncommitted edits — builds the dev and tests systems — kernel, userland and base — with `scripts/selfhost.sh build` (`selfhost_test`), leaving cargo's `--timings` report under the clone's `builddir/target/cargo-timings`, and checks the tree's own branch out again; the host holds the commit the guest names to `HEAD`, the root to `e2fsck -fn` and to being at rest, exports both kernels and the tests base, runs the ELF gates on the kernels and runs the suite on the guest's tests kernel and base. The boot's budget is eight hours, sized for KVM; `SELFHOST_TIMEOUT_SECS` raises it for TCG, which runs the guest's build about 25 times slower.
- `just bench-selfhost` — the self-hosting build as a profile: boots the optimized tests kernel on the self-hosting root with `prof=on` (`BENCH_PROF=` turns it off), runs only `selfhost_test`, so the guest builds the host's `HEAD`, and hands the log to `scripts/prof_report.py`, which prints the guest's build times, per-CPU busy and halted time, the ext2 lock's wait and hold (writeback's share apart) and the same lock and the per-process VM lock by call site, block I/O counts and latency, syscall costs, and kernel and user ticks symbolized — user ticks through the exec-mapping table the kernel prints, `builddir/bench-libc.so` and the installed toolchain's libraries. No grading and no clean-tree requirement; run it with nothing else loading the host, because every number in it is wall time.
- `just check-fs-throughput` — filesystem cost ratchet over the `FSPERF[…]` / `FSCAP[…]` report lines, with gate data in `scripts/gates/fsperf/<variant>.txt`. Counts per MiB — transactions, journal commits, device write requests, barriers — are deterministic for one ISO and carry caps; a write rate is not, so the only rate graded is the quotient of the filesystem's write rate and the **same run's** raw block-device write rate, which is invariant under a change of accelerator (the gate's `--self-test` asserts exactly that: a uniformly three-times-slower machine must still pass). Floors (`min-bytes`, `min-volume-gib`, `min-dirents`) exist because a measurement that stopped happening looks exactly like one that got free. `--log` / `--emit-allowlist` / `--self-test` as in the other ratchets.
- `just check-quota-headroom` — resource-quota ratchet; asserts every account's peak stays under its measured cap in `scripts/gates/quota/<variant>.txt`, that nothing was denied, and that the charge path has not got slower. What the `used`/`peak` packing buys is that a *reported* peak is a value that was genuinely held — the caps themselves are measured maxima carrying the observed spread as margin, exact only on the rows the gate file records as deterministic (`process`, and the fd/object rows). The **cost** check is one cap and two floors, never a cycle count: a cycle count on that path measures the accelerator, not the kernel, and the absolute caps this gate used to carry failed on the *unmodified* tree on any machine without `/dev/kvm`. The cap is `max-depth-cost-ratio` — depth 7 against depth 1, the only quantity here invariant under a change of accelerator. The floors are `min-charge-over-reference` (one charge+refund round trip against a same-run bare CAS, a floor and not a ceiling because that ratio *does* move with the accelerator) and `min-reference-cycles` (an absolute physical bound on the reference itself, since the first floor is a ratio over it). Stated plainly: a slowdown that scales the whole charge path uniformly passes every one of them, and catching it would need the absolute ceiling that failed without KVM. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate, with one difference: this gate's `--log` is a single run, so its file records spreads in prose rather than merging several logs mechanically. `--emit-allowlist` emits a depth cap a quarter above the observation, and its own output is round-tripped through the check path by the self-test — the property that makes "re-measure with `--emit-allowlist`" a remedy that actually works.
- `just check-lockdep-headroom` — lock-order ratchet; boots the test ISO and fails unless every phase the kernel reports (`boot`, `post-kernel-tests`, `post-userland-tests`) says `ACTIVE`, reports no violation, and stays inside the gate file's `max-fill-pct`. Gate data lives in `scripts/gates/lockdep/<variant>.txt` in the same measured-and-tracked style as the stack/vector gates, and an entry matching nothing fails as a dead entry. The three pools are not graded alike. Class counts are deterministic — a class registers on the first acquire of a declaration site, a mutex's or a wait queue's on its first lock or wait rather than the first waiter that sleeps, and three runs of one pinned ISO measured boot at 71 every time — so they carry **exact caps**, and under `tests=on` each report is followed by `LOCKDEP-CLASS[<phase>]:` lines naming the classes that phase registered, so a count that moved names its lock. Boot's edge and chain counts carry caps rather than bands for the same reason, though the recorded values still hold the old convention's slack until they are re-measured onto the observed 43/110. The two test phases' edge and chain counts measure which orderings a run *happened to observe* and move between runs of identical code, so they carry **bands** (`band <phase> <pool> <lo> <hi>`) instead: leaving one prints `DRIFT` on stderr and the run still passes. Be clear about what that gives up — a banded pool has no upper failure of its own, so growth up to `max-fill-pct` (~3.5x observed) reaches you only as that DRIFT line; an *inverted* order is caught by the cycle detector and still fails. `min-classes` / `min-edges` / `min-chains` are the floors that stop a validator which quietly stopped recording from reading as maximally healthy. `--emit-allowlist` writes a fresh baseline (and accepts several `--log`s to merge), a single `--log FILE` parses a capture instead of booting, and `--self-test` (run from `check-framekernel-gates`) drives its crafted logs through the parser — proving both that the gate rejects and that it stays silent on the forms it deliberately accepts.

- `just check-sched-spread` — SMP placement-eligibility gate. Reads the `SCHEDCPU[<phase>]:` lines and holds the run to one structural invariant: **a CPU that is online is eligible for task placement.** Not a measured ceiling — the only tracked number is `eligible-lag`, how many online CPUs may legitimately not be eligible yet, which is `1` at `boot` and at `post-kernel-tests` (the BSP executes boot steps on the bootstrap stub and cannot dispatch until `enter_scheduler(0)` runs at the end of boot init) and `0` afterwards. A lag above that is a bug, not a number to raise. It exists because the failure is invisible: every placement helper filters candidates through `is_schedulable_cpu`, so a CPU whose runqueue is online but not *enabled* disappears from every fork, exec and wakeup while still dispatching whatever work stealing hands it — the machine boots, the suite passes, and one core does all the work. It shipped that way: `boot_step_scheduler_init` ran in the `services` phase and `init_all_percpu_schedulers` reset every runqueue it found, including the three APs that had already entered their scheduler loops during `drivers`. `allow-ineligible` records *which* CPUs that lag may name, because a count alone accepts an AP dropping out as readily as the BSP. `require-dispatch` is the companion check, so a CPU that is eligible but never actually dispatches also fails — and it is a flag, never a volume: how *many* switches a CPU makes is a property of the machine, so any floor above zero fails on a runner that is merely faster or slower. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate.

All four boot-based ratchets (`check_test_count.sh`, `check_lockdep_headroom.sh`, `check_quota_headroom.sh`, `check_sched_spread.sh`) accept `--log`, and CI feeds them one `builddir/run_tests --raw --no-color` capture rather than booting QEMU once per question. Do the same locally — see the pre-commit sequence below — rather than paying four boots.

`check_sched_spread.sh` is the one gate in this list that is not a ratchet: its number describes the boot sequence, not a resource high-water mark, so a failure means a CPU stopped participating in scheduling and the fix is upstream of the gate.

A ratchet failure is a **measurement to re-take, not a number to raise**. Bump a cap only with a fresh `--emit-allowlist` in the same commit, and say in the commit message which lock, test or account added the delta. Never edit a gate file by hand to make a run pass.

### Cmdline knobs
The kernel parses these from the Limine cmdline (threaded through `scripts/build_iso.sh`'s third positional arg, controlled by the `test_cmdline` justfile constant or the `TEST_CMDLINE=…` env override), followed by whatever the kernel was built with in `SLOPOS_BUILTIN_CMDLINE` — appended, so a knob read by its last occurrence takes the built-in value, and a slot whose `limine.conf` entry is fixed still boots with an experiment's knobs (`SLOPOS_BUILTIN_CMDLINE='cpufreq=firmware prof=on' just remote-install`). The boot log says `BOOT: built-in command line appended: …`. For manual `just boot-log` invocations, set `BOOT_CMDLINE='tests=on tests.run=mm::*'` to run a subset.

| Key | Values | Effect |
|---|---|---|
| `tests` | `on` / `off` | master enable |
| `tests.shutdown` | `on` / `off` | write to `isa-debug-exit` after the run |
| `tests.verbosity` | `quiet` / `summary` / `verbose` | per-test emit policy |
| `tests.warn_ms` | integer | mark slower tests as `OVER_TIME` |
| `tests.run` | comma-separated globs | only run matching tests |
| `tests.skip` | comma-separated globs | skip matching tests |
| `root` | `auto` / `initramfs` / `disk` / a device | which filesystem `/` is. `auto` prefers a writable `disk0`, the first disk that is not a USB disk, and falls back to the initramfs; `initramfs` mounts no disk a `mount=` does not name but an install medium's payload; a device is any mount-source spelling — `nvme0n1p2`, `/dev/vda`, `PARTUUID=`, `UUID=`, `LABEL=`; an absent device or partition degrades to the initramfs with a klog line |
| `mount` | `<device>:/<path>` / `LABEL=<label>:/<path>`, repeatable | mount an ext2/3/4 volume read-write after the root is up, in cmdline order; the source takes every spelling `mount(2)` accepts. A failure is one klog line and the boot goes on |
| `lockdep` | `off` / `warn` / `panic` | lock-order validator policy; default `panic` |
| `verity` | `require` | the root disk must mount with a verity trailer or the `fs init` boot step fails; no disk at all still passes |
| `sched.ap_pause_ms` | integer | wall-clock budget for the AP pause; `0` disables the deadline and falls back to the iteration bound. Default measured — see `AP_PAUSE_BUDGET_NS_DEFAULT` |
| `kconsole` | `off` / `on` / `<hex mask>` | diagnostic-console permission mask; default `on` (informational only) |
| `kconsole.serial` | `on` / `off` | serial BREAK trigger; default `on` |
| `kconsole.arm_ms` | integer | how long the keyboard chord stays armed; default 3000 |
| `kconsole.max_lines` | integer | per-command line budget; default 512 |
| `kconsole.probe_ms` | integer | per-CPU answer budget for the all-CPU probe; default 250 |
| `watchdog.miss_threshold` | integer | consecutive heartbeat samples a CPU may miss before the watchdog reports it; default 100, `0` refused |
| `watchdog.panic` | `on` / `off` | whether a stall five thresholds long is fatal; default on bare metal only, since under a hypervisor a descheduled vCPU looks the same |
| `mem.commit` | integer | percent of usable frames the commit ledger may promise to private mappings; default 100, capped at 400, `0` measures without a ceiling |
| `prof` | `on` | sample where the time goes — per-CPU user/kernel/idle ticks and halted time and each CPU's busy share and effective clock, the hottest kernel RIPs and where idle loops spend ticks that never halt, user ticks by task name, and the kernel stacks blocked user tasks are parked on whenever a CPU idles — printed as `PROF[post-userland-tests]:` lines; off by default. `TEST_CMDLINE_EXTRA=prof=on just test-selfhost` profiles the guest's build. `prof start`, `prof report <label>` and `prof stop` (`prof_ctl`) do the same on a running system, printing `PROF[<label>]:` lines that `scripts/prof_report.py --label` reads |
| `cpufreq` | `hwp` / `firmware` | `hwp` (default) enables HWP where the CPU has it and asks every CPU for autonomous selection between its lowest and highest level; `firmware` leaves every frequency register as the firmware left it |
| `cpufreq.epp` | `performance` / `balance_performance` / `balance_power` / `power` / 0-255 | the energy-performance preference HWP is given; default `balance_performance` (128) |
| `sched.hybrid` | `on` / `off` | `on` (default) places a task on the best idle CPU by class — a P-core with nothing on it, then an E-core, then a busy core's sibling thread — on a part whose CPUs differ; `off` treats every idle CPU alike |
| `extensions` | `off` | activate no extension this boot: `ext_activate` answers `EPERM`, and `BOOT_FLAG_EXTENSIONS_OFF` tells userland |
| `panic` | `reboot` | a kernel panic resets the machine (ACPI, then `0xCF9`, then the keyboard controller; the format-free abort triple-faults) instead of halting it: how a boot slot that panics falls back to the loader's default. The crash record is written first; on bare metal a panic's screen stays up for ten seconds before the reset, under a hypervisor not at all |
| `panic.boot` | `on` / `abort` | panic as soon as boot initialisation completes, or take the format-free abort a lockup takes — the broken slots `just test-install` rolls back from |
| `usb` | `on` / `off` / `report` | `on` (default) takes every xHCI controller it can drive from the firmware and runs it; `off` binds none, touching nothing; `report` logs each controller's capabilities, protocols and ports and leaves it to the firmware |
| `usb.settle_ms` | integer | how long a USB disk's LUN may stay not ready at probe, and how long boot waits for USB to settle when the device `root=` or a `mount=` names is absent, or before an install medium looks for its payload; default 5000 |

`lockdep=warn` reports each distinct finding once (deduped per class pair) and
keeps booting, so one boot enumerates every ordering finding in the tree instead
of stopping at the first. `lockdep=off` keeps the held-lock stack — the poison
walk, the TLB ack-wait diagnostic and the watchdog all read it — but runs no
ordering checks, which is how the validator's own per-acquire cost is measured
without a separate build.

### Diagnostic console

`slopos_ostd::kconsole` is the kernel's magic-key facility: a key pressed on the
**physical console** makes the kernel describe itself. Press SysRq
(Alt+PrintScreen) to arm and one command key to run, or send a serial BREAK and
then the command key. Press the trigger then `h` for the list. A program may
run the informational ones too: `/bin/kconsole t` (privd's
`org.slopos.kconsole`, run with `SysInspect`) queues `t` through
`kconsole(2)`, waits for it to run and prints what it
added to the kernel log, so `just remote run -- kconsole tp` reads a machine
nobody is sitting at. The probe (`p`) writes each CPU's RIP, current task and
idle slot there too, beside the NMI handler's own lines on the UART.

Commands live in the `.kconsole_registry` linker registry, so the crate that
owns a subsystem's data owns the command that prints it — `kcommand!` in `mm/`,
`sched/`, `core/`, `boot/`. OSTD defines the registry and never names an entry;
registration must happen in a crate only the kernel links, because OSTD is
linked into userland binaries too and their linker script brackets no kernel
section.

Three properties are load-bearing:

- **Only the physical console reaches a destructive command.** The keyboard
  hook sits in the keyboard state ahead of layout resolution and consumes its
  keys, so they reach neither the TTY nor the focused GUI application; the
  i8042 feeds it from its IRQ handler and a USB keyboard from wherever the
  xHCI event ring is drained, and a USB keyboard is the physical console as
  much as the i8042 is, as on Linux. The serial trigger is a BREAK condition,
  which no byte pattern can forge. The one
  other caller of `kconsole::request` is `request_informational`, which refuses
  any key a `KCMD_DESTRUCTIVE` command takes and is reached only by
  `kconsole(2)` for a `SysInspect` holder — the shape of Linux's root-only
  `/proc/sysrq-trigger`, narrowed to the commands that only print. The facility
  this replaced was reachable by any process holding a PTY master.
- **One execution tier.** Every command runs at the bottom-half point with
  interrupts and preemption enabled. No command runs in NMI context and none may
  assume it can: a *returning* NMI handler must be fault-free, and the
  frame-pointer walk a backtrace needs is only fault-*recoverable*. The all-CPU
  probe therefore asks each CPU to describe itself from its own NMI handler
  rather than walking a peer.
- **Triggers only queue.** `request` is one `fetch_or` and one `gs`-relative
  byte store — what `bh::raise` permits from a hard IRQ and from under a
  cli-spinlock. The pending set is global rather than per-CPU because
  `bh::raise` marks only the calling CPU, and every CPU's timer tick pokes its
  own bottom half while anything is queued.

`KCMD_DESTRUCTIVE` commands are registered but refused unless the mask names
their bit, which the default does not: boot `kconsole=0x3` to enable them.

### Output format and JSONL events
The public KTAP docs describe the wire grammar and the JSONL event schema. The wire format is stable; the JSONL schema is a strict superset suitable for downstream JUnit XML conversion or test-history regression detection.

A utest's cases reach the wire as subtest lines ahead of its own result line, written past the per-test klog capture: a long utest overflows that ring, and a verdict lost with it leaves a failure with no name. A passing case's note follows `#` on its line, which is where the toolchain ladder records rustc's startup time and each build's wall time.

## Commit & Pull Request Guidelines
Subjects are `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`), ≤72 chars. Add a body for rationale, boot implications, or follow-ups. For PRs include: motivation, testing artifacts (command + result), issue references, and serial excerpts or screenshots when boot flow or visible output changes. Flag breaking changes and downstream-script coordination.

### Commit messages (MANDATORY)
**Always use the `caveman-commit` skill to write the commit message. Never hand-write one.**

- It is checked in at `.agents/skills/caveman-commit/SKILL.md`, which `.claude/skills/caveman-commit/SKILL.md` links, so nothing needs installing. Load it with `/skill:caveman-commit`, or read that file directly if skill commands are unavailable.
- The skill only *generates* the message; staging and running `git commit` remain your job.
- This applies to every commit, including one-line and trivial ones.

### Branching (MANDATORY)
Commit directly to `develop` (the working branch). Do **not** create a new branch unless explicitly asked to — this overrides any default "branch before committing on the default branch" behavior. Likewise, do not open PRs or push to a remote unless explicitly asked.

### Pre-commit (MANDATORY)
Before every `git commit`, **always** run `cargo fmt --all` and stage any reformatted files. If formatting fails, fix the issue before committing. Never commit unformatted Rust code.

**`just test` alone is not the bar.** CI runs gates that `just test` does not, and a commit that only satisfies fmt + tests routinely fails on them — most often the lockdep ratchet. Reproduce CI's lanes locally:

```sh
cargo fmt --all                       # then stage the reformatted files
just fmt                              # ci: Check formatting
just test-host                        # gates: Host-side unit tests
just build                            # gates: Build the dev kernel
just check-framekernel-gates          # gates: Framekernel gates (self-tests + vendor/toolchain pins + all source/ELF scans)
just check-offline-build              # gates: Offline build from vendored sources

# ci: Run tests — one raw capture, which the ratchets then parse.
just _build-run-tests
set -o pipefail
builddir/run_tests --raw --no-color 2>&1 | tee builddir/ci-test.log

# The tests userland has staged the cross-built runtime and this commit's
# libc.so now, so the stamp and symbol halves of the C++ gate and the libc
# export gate hold what was just built.
scripts/check_cxx_pin.sh
scripts/check_libc_exports.sh --require builddir/libc.so

# The LLVM port and the clang driver in it. `just llvm-src` materialises the
# sources first — 1.5 GB, so it is a deliberate step rather than something a
# build does; without them both report `skipped`. CI materialises them in the
# `toolchain` job and passes `--require`.
scripts/check_llvm_port.sh
scripts/check_clang_driver.sh

# The cross-build configuration, which needs both the source tree
# (`just rustc-src`) and the target sysroot the tests build just staged.
scripts/check_bootstrap_config.sh

scripts/check_authority_reachability.sh --variant tests builddir/kernel-tests.elf
scripts/check_test_count.sh        --log builddir/ci-test.log
scripts/check_lockdep_headroom.sh  --log builddir/ci-test.log
scripts/check_quota_headroom.sh    --log builddir/ci-test.log   # not yet a CI step; run it anyway
scripts/check_sched_spread.sh      --log builddir/ci-test.log
scripts/check_fs_throughput.sh     --log builddir/ci-test.log   # not yet a CI step; run it anyway
```

The capture is reused deliberately: booting QEMU once per ratchet is a boot per
question, and a second boot could disagree with the one that was graded.

A changed lock order, a new lock, a new test, a new quota account, or a write
path that issues more device requests per MiB will move a ratchet. That is the
gate working — re-measure with the gate's `--emit-allowlist` and explain the
delta in the commit message. Never hand-edit `scripts/gates/**` to silence a
run.

Two checks are *not* in the sequence above because they are slow and run in a CI job of their own, `ostd-verify`: `just check-miri` (KernMiri) and `just verify` (Verus). Run them when touching `slopos-ostd/` or `verification/`; `just check-framekernel` is the recipe that runs the gates plus both.

CI is four parallel jobs, and its wall clock is the longest one: the boot lane (`ci`), which holds only the tests build, the boot and the graders that read its capture. Every job pays its own setup in runner minutes, so a new check joins the job whose inputs it needs — `gates` for the source tree and a kernel, `toolchain` for the staged tests userland and the patched source trees — rather than a job of its own, and goes in the boot lane only if it reads the capture. Compiler output is cached by content: the LLVM and clang builds under ccache through CMake's `CMAKE_C_COMPILER_LAUNCHER`/`CMAKE_CXX_COMPILER_LAUNCHER` environment variables, which a first configure reads, the recipes' cross compiles under the same ccache through `SLOPOS_CROSS_LAUNCHER`, which the SlopOS compiler wrappers run each compile through, and the Rust builds under the sccache `scripts/ensure_sccache.sh` pins, as `RUSTC_WRAPPER` on build steps only — never on a gate that probes what a compiler does, since a cache hit there would be a cached answer. Locally, export the two CMake variables for the same warm C++ rebuilds, and set `RUSTC_WRAPPER` on a build command rather than in the shell, for the same reason. `plans/ci-latency.md` is the measured model and the work still open.

Commit order: `cargo fmt --all` → the sequence above → stage → `caveman-commit` for the message → `git commit`.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads the pinned Limine binary release into `third_party/limine`; offline environments should pre-populate that directory with the pinned release's `limine-binary.tar.xz`, unpacked, or set `LIMINE_URL`/`LIMINE_VERSION` to avoid network stalls. Rust crates are auto-discovered via the workspace, so most build changes belong in `justfile`, `scripts/`, `Cargo.toml`, and `targets/*.json`; ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered. VirtIO devices require MSI-X (preferred) or MSI as a minimum — legacy polling has been removed; probe panics if neither interrupt mechanism is available.**

`scripts/make_slopos_sysroot.sh` needs the pinned `libc` crate; it takes it from `$CARGO_HOME/registry/cache` when it is already there and only falls back to `static.crates.io`, so an offline environment should pre-populate that cache (or point `LIBC_URL` at a local copy). It also needs the `rust-src` component, which `scripts/ensure_toolchain.sh` installs.

## Safety & Execution Boundaries
Keep all work inside this repository. Do not copy kernel binaries to system paths, do not install or chainload on real hardware, and never run outside QEMU/OVMF — with one exception: a SlopOS machine the user has booted a base paired with this host on (`just remote-serve` and the bootstrap it prints) may be driven through `scripts/remote.py` alone, which runs programs there, moves files and installs a base on its boot partition and a kernel into its spare boot slot. Never write its ESP, its firmware variables beyond what `bootctl` does, or another system's partitions, and commit a slot (`--commit`, `bootctl commit`) only once the boot it came from answered with the expected tag. The scripts already sandbox execution; if you need fresh firmware or boot assets, use the provided automation instead of manual installs. Treat Limine, OVMF, and the kernel as development artifacts only and avoid touching `/boot`, `/efi`, or other host-level locations.


## Security Triage & CVSS Ledger (MANDATORY)

All agents must run a recurring vulnerability review loop for newly written and recently changed code.

### Required cadence
1. Run a security sweep after each major milestone and before any release/PR handoff.
2. Re-scan subsystems touched by recent commits (at minimum: syscall paths, memory management, filesystems, drivers).

### Triage workflow (strict order)
1. **List all findings first** in a raw triage section (do not score as CVSS yet).
2. For each finding, assign a **confidence score (0-100)** using this model:
   - Evidence quality (0-40): direct code proof, exact path/line references
   - Exploitability clarity (0-30): realistic attacker path and impact
   - Reproducibility (0-30): deterministic repro or strong step-by-step plausibility
3. Only findings with **confidence >= 80** are considered **guaranteed issues**.
4. Only guaranteed issues get a CVSS vector/score entry.
5. Use `scripts/cvss_calc.py` to compute CVSS v3.1 vectors/scores consistently across agents.

### CVSS file lifecycle requirements
1. Maintain `CVSS.md` as the single living ledger of **open findings only** (pre-alpha policy).
2. Every entry must include:
   - Stable internal ID (e.g., `SLOPOS-YYYY-NNNN`)
   - Status: `open` or `needs-retest`
   - Confidence score and reasoning
   - CVSS vector + score (only if confidence >= 80)
   - Exact evidence paths/lines
3. When an issue is fixed (pre-alpha policy):
   - **Remove it** from `CVSS.md`. SlopOS is pre-alpha with no audit-trail obligation, so resolved findings are deleted rather than retained as historical `fixed` records.
   - Internal IDs stay stable for findings that remain open; gaps in the numbering are expected.
4. When new guaranteed issues are found, append them with incremented IDs.

### Repro/examples (required when possible)
1. Add a minimal repro recipe for each guaranteed issue when technically feasible.
2. Repro can be a syscall sequence, malformed input artifact, or concise PoC steps.
3. If no safe repro is possible, document why and provide nearest deterministic validation method.

### Non-negotiable rule
- Never present speculative issues as CVSS-scored vulnerabilities.
- Confidence-gated, evidence-backed issues only.

---
