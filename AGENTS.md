# Repository Guidelines

## Project Structure & Module Organization
Kernel sources are split by subsystem: `boot/`, `mm/`, `drivers/`, `sched/`, `video/`, `fs/`, and `userland/`. Each hosts a Rust crate (`Cargo.toml` + `src/`). `link.ld` and the `justfile` drive the canonical `no_std` Rust build flow via cargo + `rust-lld`. Generated artifacts stay in `builddir/`, while `scripts/` contains the build/boot/test automation and `third_party/` caches Limine and OVMF assets.

## Build, Test, and Development Commands
[`just`](https://github.com/casey/just) is the command runner; the `justfile` drives cargo + `rust-lld` via `scripts/`. Run `just --list` for all recipes. No git submodules — `scripts/ensure_limine.sh` fetches pinned Limine v12.3.1 into `third_party/limine` on first ISO build.

- `just setup` — install pinned nightly from `rust-toolchain.toml`; materialize the owned `slopos` sysroot (`scripts/make_slopos_sysroot.sh`, see below); verifies Go >= 1.22 on PATH (for `tools/run_tests/`)
- `just build` — emits `builddir/kernel-dev.elf`; `just iso` regenerates `builddir/slop.iso`, the live ISO bare metal boots from RAM
- `just boot` (the development machine: persistent `/`, the dev disk at `/devel`, an A/B boot disk; spins the Wheel of Fate) / `just boot-fast` (the same without the wheel, `ROULETTE=0`) / `just boot-live` (the live ISO from RAM, no disk; `ROULETTE=0` skips the wheel) / `just boot-log` (the live ISO headless, 15 s timeout, fails unless `/sbin/init` launched)
- `just test` — the CI/agent entry point (see Testing Guidelines)

**Both targets build on an owned toolchain.** The kernel
(`targets/x86_64-slos.json`) and the userland
(`targets/x86_64-unknown-slopos.json`) are both built by `cargo +slopos`
against an *owned* sysroot at `third_party/rust-slopos` — the kernel too, so
one toolchain builds everything and the dev disk carries it at the same
workspace-relative path: `trim-paths` makes a kernel's panic locations read
the same wherever it was built. That sysroot is a hardlink clone of the
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
the vendored road is the one the dev disk and the offline gate take.
`scripts/check_offline_build.sh` holds it: `--pins-only`, from
`check-framekernel-gates`, holds every registry package in both lockfiles to
the checksum its vendored copy records wherever `third_party/vendor` exists,
which is a developer's tree and not CI's gates step; `just
check-offline-build`, in CI, checks the kernel and the userland from an empty
`CARGO_HOME`, `--locked --offline`, where a warm registry cache cannot answer
for a missing crate. A new dependency is a `Cargo.lock` diff, a `just vendor`
and a `NOTICE.md` entry.

**The C++ runtime is cross-built and test-only.** `x86_64-unknown-slopos` has
a C++ standard library: LLVM's `libc++` and `libc++abi`, cross-built from the
llvm-project release `toolchain/cxx/PIN` names, linked whole-archived into a
single `third_party/slopos-cxx/lib/libc++.so`. `scripts/make_slopos_cxx.sh`
builds it (idempotent: a stamp over the pin, the host compiler's version, the
build script and `slibc/include` makes a warm run milliseconds),
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
compiler predefines, and `toolchains::SlopOS`, the clang driver that turns
`cc a.o -o a` into the link line `build_userland.sh` writes by hand — pinned
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
rolling host); a major outside it builds with a note on stderr, because
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

**The C libraries cargo's network features link are recipes, and so is git.**
`toolchain/recipes/<name>/recipe` pins an upstream release tarball (URL,
SHA-256, licence), a build template (`cmake`, `meson` or `openssl`), its
configure arguments and its dependencies: zlib, nghttp2, OpenSSL, curl,
libssh2, libgit2 and git. `scripts/build_recipes.sh` (`just recipes`) builds
them shared and `-z defs` into `builddir/slopos-recipes/prefix`, with the
target sysroot and `x86_64-unknown-slopos-clang{,++}` wrappers that
`scripts/make_slopos_cross.sh` assembles for bootstrap too; tarballs are
cached in `third_party/recipes/`, and the recipes add `meson`, `make`, `perl`
and `pkg-config` to the C++ runtime's host tools. A library is
`$ORIGIN`-rpathed; a program
(`program=`, git's `bin/git`) is built by the `meson` template with
`--prefix` the guest path of the toolchain, because it finds its exec path,
templates and system config through that compiled-in prefix, and its run path
reaches `lib/` from `libexec/<name>/`. Bootstrap copies the libraries into
the target sysroot and `build_recipes.sh --install-programs` copies the
programs into the install, so both reach the dev disk; only the libraries'
stamps clear cargo. **No patch, ever:** a build that would need an edit to
upstream is a slibc or kernel finding, fixed there — git's build is what
brought `<utime.h>`, `<grp.h>`, `mkstemp`, `freopen`, `execl`, `getpass` and
`pthread_setcancelstate` into slibc. `scripts/check_recipes.sh` holds the
shape and holds every `arg` and the OpenSSL target definition to a grammar
that can carry no code (a flag, CMake script, launcher or search root edits
what is built without touching a file); the driver fails any build that
changes the unpacked tree.

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

**The result lands on a dev disk.** `scripts/build_devdisk.sh` (`just
_fs-image-devdisk`) builds `fs/assets/ext2-devdisk.img`, a preserved,
trailer-less 8 GiB volume labelled `slopos-dev`, carrying the target sysroot
and, on a new volume, the prefix `just toolchain` installed, at
`src/slopos/third_party/rust-slopos` — where the host keeps its owned sysroot,
because the kernel carries the source paths of `core` and `alloc` in its panic
locations, so the two machines build one image only if those sources sit at
one place in both trees. A preserved volume whose toolchain
differs from the installed one fails the build and names the fix. It is
attached by `DEV_DISK_IMG` as **virtio-disk4** and mounted by the kernel from
its command line: `mount=LABEL=slopos-dev:/devel`, since the guest's disk
letters are positional (`just boot` boots it that way, with 4G of RAM and the
optimized kernel, because a dev-profile kernel spends ten times as long in
every syscall and page fault a compiler makes). The marker file at the volume root records every staged
path's size read back *out of the image*, and `devdisk_test` grades the
inventory and the source tree, unmounts `/devel` and mounts it again by label
— the second mount is the point, because a leaked write claim answers
`AlreadyClaimed` forever — and then climbs the toolchain ladder: `rustc
--version` under `LD_DEBUG=statistics`, rustc linking a program through `cc`,
cargo building a crate with a build script and a proc macro, cargo fetching a
`git = "file:///devel/git/greeting.git"` dependency through libgit2, cargo
fetching a crate through libcurl and OpenSSL from the volume's `registry/`,
served over loopback TLS and verified against a per-volume test root (the
image's own CA bundle must refuse it first), clang compiling C and C++, and
git reading the clone and reaching the host's checkout.

**The source moves by git.** A new dev disk is seeded with `src/slopos`, a
clone of the checkout with `HEAD`'s branch checked out (`git clone
--no-local`, so the history is what the branches and tags reach and nothing
else), the vendored crates in its ignored `third_party/vendor`, and the
offline vendor configuration in `src/.cargo/config.toml`, above the tree,
where cargo reads it for any directory below while the clone's own
`.cargo/config.toml` stays as committed. The volume is given to uid 0, the
guest's only user: `mkfs -d` copies the host's ids, and git refuses a
repository its user does not own. The clone's `origin` is
`git://10.0.2.4/slopos` and its `host` remote, the push default, is
`git://10.0.2.4:9419/slopos`: with `GIT_PUSH_REPO` set, `qemu_run.sh` adds two
SLIRP `guestfwd` rules, as for the echo peer, each running one `git daemon
--inetd` per connection — one serving this checkout read-only, one serving
the bare repository `GIT_PUSH_REPO` names with receive-pack — so nothing
listens on the host. Each daemon is pinned to its git directory twice: its
`--interpolated-path` answers a request that names a host, which every git
client sends, and `--strict-paths` with that directory as the whole allowlist
refuses a hand-made request that names none and so bypasses the template.
`just boot` names `fs/assets/devdisk.git`, which `just reset
devdisk` keeps; the tests name a scratch repository under `builddir/`. In the
guest, `git pull` takes the host's commits and `git push` hands the guest's
back (`git fetch fs/assets/devdisk.git <branch>` on the host). A preserved
volume whose `src/slopos` is not a clone fails the build and names the fix;
`just devdisk-export-file` copies one file, a kernel the guest built, off a
volume whose guest has shut down.

**The kernel builds in the guest.** `scripts/build_kernel.sh` is POSIX sh
that runs under `/bin/sh` with the coreutils and nothing else; what only
the host has — `ensure_toolchain.sh` before, the ELF gates after
(`scripts/check_kernel_elf_gates.sh`) — lives in the justfile's `_kernel`
recipe. The embedded symbol table comes from `tools/kallsyms`, an ELF reader
built for whichever machine runs the build, byte-identical to what `llvm-nm`
gave less LLVM's `.llvm.<hash>` promotion suffix, which would keep a release
table from ever reaching a fixed point; the driver writes the empty safestack runtime archive itself; and
`trim-paths` keeps every absolute path out of the image. `just test-selfhost`
is the whole loop: the guest builds the dev and tests kernels
(`selfhost_test`), the host holds the volume to `e2fsck`, exports both kernels,
grades them with the ELF gates and runs the kernel suite on the tests kernel
(`just test-elf`). The guest's kernel is not held to a host build's bytes:
the gates and the suite are the grade.

**The guest installs what it builds.** `/bin/bootctl` (granted `Mount` for the
raw partition and `Power` for the loader's variables) writes a kernel into a
slot of the boot disk's EFI system partition through `fat-core`, a FAT32
implementation whose every file write is copy-on-write — the new contents go
into free clusters and a single directory-entry store commits them, so neither
a kernel nor `/limine.conf` is ever half replaced. It then sets
`LoaderEntryOneShot`, which Limine consumes on the next boot, and after that
boot `bootctl commit` makes the entry Limine reports in `LoaderEntrySelected`
the default. A slot that panics resets under `panic=reboot`, and the reset lands
on the old default. `/dev/vd*` nodes accept writes from a `Mount` or `SYSTEM`
holder, each one through the device's exclusive claim, so a mounted device
answers `EBUSY`. UEFI variables are read and written on a kernel thread — the
firmware is mapped only into the kernel master address space and may use the
vector registers — and only under the Boot Loader Interface's and SlopOS's own
vendor GUIDs. In the guest, `scripts/selfhost.sh install` is the one
command for the whole of it: it builds with the dev disk's toolchain, installs
into the slot that is not the default and arms the one-shot boot.
`selfhost_test` and `install_test` run that script as a person at the shell
does, so the loop a developer types is the loop the tests grade.

**The guest speaks TLS 1.3.** `tls-core` is a sans-I/O client with every
primitive under it written here, `no_std` and `forbid(unsafe_code)`;
`userland::tls` wraps it in a blocking stream, and `curl` is its user. It
trusts `/etc/ssl/certs/ca-certificates.crt`, Mozilla's root store committed
under `assets/certs/` and replaced only by `scripts/update_ca_bundle.sh`,
which checks the digest curl.se publishes. `tls-core` and `http-core` are host
crates under `just test-host` because no userland unit test runs anywhere;
`tls-core`'s interop tests drive `openssl s_server` and `s_client` and need
`openssl` on PATH.

**The unwinder is not test-only.** `libc.so` and `libc.a` supply the Level-1
Itanium unwinder (`vendor/unwinding`, seventeen `_Unwind_*` entry points) on
every image, behind slibc's `unwinder` cargo feature which only those two
wrapper crates enable, and both are built `-C force-unwind-tables` because the
first frame of every unwind is one of their own. The cost is that the shipped
`libc.so` carries the DWARF reader — 45 904 bytes of a 571 696-byte library,
re-measured by building the cdylib with the feature off — which `--gc-sections` cannot drop while startup registers the frame finder
unconditionally. The 60 static Rust binaries are unaffected: they take slibc as
an rlib with the feature off, and `userland/userland.ld` discards `.eh_frame`
outright.

`just boot-live` and `just boot-log` boot `builddir/slop.iso` with `BOOT_CMDLINE` as its command line, plus `boot.debug=on` under `DEBUG=1` and `roulette=skip` under `ROULETTE=0`; `VIDEO=0` makes `just boot` and `just boot-live` serial-only.

**The disk is the root.** `root=auto` mounts a writable `disk0` at `/`, so what a boot writes there persists; the initramfs is the fallback for no disk and for a disk that mounted read-only (the verified `ext2.img` boots `/sbin/init` from RAM with the attested disk at `/mnt`). `root=` also accepts `initramfs`, `virtio`, and a device name — `/dev/vda`, `/dev/vda1`, `vdb2` — where the partition comes from the GPT or MBR table on that device; a named device or partition that is absent degrades to the initramfs exactly as no disk does. `just boot` is the developer's persistent machine: it boots this build's kernel from an A/B boot disk it rebuilds every run, with `fs/assets/ext2-persist.img` as `/`, built `VERITY=rw` (a v2 trailer, so the image is writable *and* attested everywhere the guest has not written) and refreshed in place across builds (`PRESERVE_FS_IMAGE=1`, binaries only) so what the guest wrote survives. `VERITY=on` builds the verified image's v1 trailer, which write-protects the device and is what `verity=require` asserts; `VERITY=off` builds no trailer. The verified and *tests* images are regenerated on every build on purpose — a persistent `/` would make every filesystem test a mutation of the image the next run boots from.

**The root is not the only filesystem.** `mount(2)` with `fstype=ext2` takes a
`source` naming a block device — `mount("/dev/vdb1", "/home", "ext2", …)` —
claims that device's exclusive writer (or mounts read-only with `MS_RDONLY`,
which needs no claim), and binds it to one of four pooled `Ext2Mount`
instances, each with **its own lock**. A path walk crossing a mount therefore
holds one mount's lock while taking the next one's, which is why the four
instances carry four separate `lock_class!` sites rather than one shared class;
slot 0's is still named `CACHED_EXT2`, so the class boot registers is the class
it always was. `umount` of the last mount of an instance flushes it, marks the
image clean, drops the device and returns the slot, so the write claim is
released and the same disk can be mounted again — a leaked claim answers
`AlreadyClaimed` forever, and that is the failure the remount test exists to
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

**A rude exit is survivable.** The flusher marks the image clean *on the
medium* once a pass leaves nothing dirty, nothing unbarriered, no
superblock drift and an empty log, and the mount has had nothing to write
for a second — the state ext4 reaches for `fsfreeze`, here reached
automatically at idle; a busy mount would pay a superblock read, write and
barrier each way on every pass — and `Ext2Fs::transaction` re-stamps it
dirty before the next mutation reaches the device. A mount owes the stamp
from the moment it attaches, since attaching stamped it dirty and a mount
nothing writes to runs no pass. Closing the QEMU window
therefore costs at most the last idle window's writes, instead of leaving an
image that mounts read-only forever after and that `root=auto` then demotes to
`/mnt` while booting the initramfs. The host half is the same promise:
`build_fs_image.sh` never deletes a `PRESERVE_FS_IMAGE=1` image. One that is
damaged, left dirty, or built under a different `VERITY` stops the build
naming the command that repairs it, except that `just boot` boots a disk
closed mid-write without refreshing it, so the kernel replays its log;
`just reset root` (or `devdisk`) is the only
thing that discards one; a larger `PERSIST_IMAGE_SIZE` grows the image with
`resize2fs` rather than rebuilding it; and `gen_verity.py` AND-s the old
attested bitmap into the new one, so a block the guest rewrote stays
un-attested across rebuilds. The persist default is 512M; the ceiling is now
what the machine's RAM allows rather than what one allocation allows, because
the verity hash array is chunked (4 bytes per 4 KiB block, in 256 KiB pieces,
refused past a stated share of usable memory) instead of one contiguous `KVec`.

**A write is logged before it lands.** A writable ext2 image carries a metadata
redo log in a preallocated sealed file at `/.journal` (`FS_JOURNAL_SIZE`,
default 1/64 of the image floored at 4M and capped at 64M; `0` builds none),
located at mount by path lookup and used as a
physical redo log: an operation's metadata goes into the log with a
CRC-covered commit record before any of it reaches a home location; file data
never does, and instead reaches its home ahead of the commit that names it
(`data=ordered`). That is
what makes an operation retractable (a rollback rewinds the log; nothing was
published) and a crash recoverable: a mount that finds `s_state` unclean and
replays a committed transaction comes up **read-write**, which is the one case
in which this kernel repairs an image instead of deferring to `e2fsck`. So does
one whose log is empty *and* carries the volume's current `[s_mnt_count,
s_mtime]`: every mount writes that stamp into the log superblock, so a match
says the last mount logged every metadata write it made and nothing mounted
the volume since — a boot that panicked with the log checkpointed left nothing
half done, and a Linux mount in between moves the count and keeps the refusal. The log
is an *image* property, not a kernel one — an image without one gets the
previous undo-scoped behaviour and still refuses an unclean mount — and the
boot log says which of the two a mount got. `/.journal` is refused to readers
and protected from write, rename, unlink and truncate by `EXT2_IMMUTABLE_FL`:
its blocks hold copies of bitmaps, inode tables and directory blocks, so a
reader of it would see the metadata of every recently changed file. The default
image is 32M rather than 16M because the log takes 4M of it.

**A commit is grouped, not synchronous.** An operation's records are staged
into an in-memory ring (`Journal::pending`, up to `PENDING_SLOTS_MAX` slots)
and reach the medium together, jbd2-style: `sync_log` writes dirty data home,
barriers, writes the ring, barriers. The flusher does that every
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
process's own, since no death gives it back. Only processes the writer could
`kill` (`signal_dominates` on the writer's flags) are ever taken, as Linux's
`oom_score_adj=-1000` is absolute: one holding privileged flags the writer
lacks is never killed for its write, so a leaking privileged service is taken
by its own write, once it is the heaviest process it may signal, and an
unprivileged writer dies rather than take it. Init's own write is the
exception, shielded from nothing, since it cannot be impersonated and its
death takes the machine down; init itself is never taken. The victim is
killed the way every kill works (the flag each thread unwinds from, I8). The
writer drops the address space, waits — killable, bounded — for the victim's
frames to be back (noted once the teardown has dropped the address space, not
inferred from the slot's unbind), and writes again; while a victim is dying
nobody picks a second — a victim whose last task left before the kill reached
it included — and one still holding its memory five seconds after the kill
stops holding back the next choice. Only when nothing but init, the dying,
processes the writer may not signal and processes holding none of what is
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
**`slopos-ostd` is the only kernel crate allowed to use `unsafe`.** It is SlopOS's Operating System Trusted Domain — the trusted core that owns every line of `unsafe` in the kernel (the framekernel **AD-1/AD-2** discipline: one trusted crate holds all `unsafe`, every other kernel crate forbids it; CI-enforced by `scripts/check_unsafe_outside_ostd.sh`). Every other crate the kernel binary links (`abi`, `acpi`, `boot`, `core`, `drivers`, `font`, `fs`, `gfx`, `hermetic`, `karch`, `kernel-services`, `keymap-core`, `ktesting`, `mm`, `net`, `pidfd`, `ring`, `sched`, `service-core`, `signalfd`, `video`, `vt`) carries `#![forbid(unsafe_code)]`, and `check_unsafe_outside_ostd.sh` asserts that from the binary's own dependency closure, so a new crate is covered the moment it is linked. Userland-side crates (`userland/`, `slibc/`, `slop-protocol/`, `appkit/`, `slopos-rt/`, `windowing/`, `fat-core/`) are out of scope for this discipline.

`forbid` is necessary but not sufficient: rustc drops any `unsafe_code` diagnostic whose primary span satisfies `in_external_macro`, so a macro defined in another crate expands `unsafe` into a forbid crate silently, and the call site holds no keyword for a source scan to find. `scripts/check_unsafe_expansion.sh` is what closes that — see below.

One documented exempt site exists outside `slopos-ostd/`:

- **`kernel/src/main.rs`** — global allocator + alloc-error-handler declarations (`#[global_allocator]`, `#[alloc_error_handler]`) require `extern crate alloc;` direct.

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
- **`scripts/check_bootstrap_config.sh`** — holds the cross-build configuration to the toolchain it claims to produce, by running bootstrap's own dry run and then compiling and linking with the generated wrapper. Three silent failures: the step graph quietly loses an artifact — `cargo` is an *extended* tool and a stage2 rustc does not depend on it, so a config that stops naming it still builds a compiler and the dev disk arrives with no cargo on it; `toolchain/compiler/0003-bootstrap-cmake-system-name.patch` goes away, and an unrecognised triple prints a note, sets `CMAKE_SYSTEM_NAME=Generic` and exits 0, losing `LLVM_ON_UNIX` and every `Unix/*.inc` file the port patches; and the wrapper stops producing SlopOS binaries, which it does by naming two triples and would regress by naming one. ~1.4 s warm, after a first run that downloads bootstrap's stage0 (~200 MB). Once `just toolchain` has installed a prefix, the gate also grades that: no `DT_NEEDED` outside SlopOS's own libraries, no `R_X86_64_TLSDESC` (the loader does not bind one), every `DT_NEEDED` found through the object's own `$ORIGIN` search path, and every undefined name defined somewhere in its closure — the loader binds eagerly and nothing there links with `-z defs`, so each of those is otherwise a program that does not start, found only in the guest.
- **`scripts/check_offline_build.sh`** — holds the tree to building with no registry. Two silent failures: `Cargo.lock` moves and the vendored copy no longer describes it, which every host build survives because the host has a registry, and the dev disk arrives with a tree its own cargo cannot resolve; and `-Zbuild-std` needs std's crates.io dependencies, which cargo resolves from a lockfile the workspace never reads, so a directory holding only the workspace's crates passes every check but a build. The pins half also fails on a vendored package no lockfile names, since that is a pin nobody reviews, and on a vendored `library.lock` that is not std's, since that copy is what a dev disk's guest grades its tree against. `--self-test` grades six fixtures, five of them rejections. The build half is 63 s cold on four cores and ~30 s warm; `skipped` without `third_party/vendor`, `--require` in CI.
- **`scripts/check_codegen_backend.sh`** — holds a rustc codegen backend to seven of the capabilities `targets/x86_64-slos.json` depends on: an ELF object format, soft-float, `.stack_sizes`, safestack instrumentation through `__safestack_pointer_address`, `#[unsafe(link_section)]`, `#[unsafe(naked)]`, and `sym` operands in `asm!`. Two of those are flags a backend can *accept and ignore* — `-Zemit-stack-sizes` and `-Zsanitizer=safestack` — so a backend swap can leave the build green with S-5 and the dual-stack split enforced by nothing. Tracked verdicts live in `scripts/gates/codegen/<backend>.txt` and a mismatch fails **in either direction**: a `lacks` the probe finds present is the signal that the self-hosting question in `plans/self-hosting.md` needs re-deciding. `disable-redzone` and the `unwind` panic strategy are stated as residual rather than probed — the gate's header says why. Cold it costs ~60 s and ~460 MB for `llvm` / ~310 MB for `cranelift` under `builddir/gates/codegen-probe/` (which `just clean` removes); warm it is ~1 s. `llvm` is graded on every `just check-framekernel-gates`; `cranelift` reports `skipped` when the rustup component is absent, and CI installs it after the gates, in the same job, so the answer is re-taken rather than assumed.
- **`scripts/check_linker_script.sh`** — holds a linker to the eighteen linker-script constructs `link.ld` uses, from `. = KERNEL_VIRT_BASE` through `PHDRS`, `(NOLOAD)`, all three spellings of `ALIGN`, `KEEP` under `--gc-sections` and the four page-table reservations past `_bss_end`. Each probe's script carries the construct under test and nothing else a probe grades — a script that scaffolds itself with an `ALIGN` reports the linker's `ALIGN` support under whatever name that probe carries — with one deliberate exception, `composed-layout`, which links a `link.ld`-shaped script because a linker can take every construct alone and compose them differently. That exception is what the gate is built around: wild 0.10.0 refuses `link.ld` on its location-counter assignment, and given the shape it does accept it keeps the script's section order and still starts the image 0x13e8 past the base it was given. A second, self-maintaining half compares the constructs probed against the keywords `link.ld` actually uses, so a construct added to the script with no probe fails the gate and a probe whose construct left the script fails as a dead entry. `scripts/gates/linker/<linker>.txt`; `lld` is graded on every `just check-framekernel-gates`, `wild` reports `skipped` when it is not installed and is pinned in the CI job that installs it.
- **`scripts/check_recipes.sh`** — holds every recipe under `toolchain/recipes/` to a pinned upstream tarball built unmodified: one 64-hex `sha256`, an `https` URL naming the recipe's version, a licence, a known template, a `soname` or `program` to install, dependencies that are recipes, no file beside `recipe` but its declared `config` and never a `*.patch`/`*.diff`, `arg`s that pick among upstream's options (a `cmake` or `meson` arg is `-D<name>=<value>` with the built-in names a recipe may set allowlisted and no flag, file, program or search root among the project's), a `NOTICE.md` entry, and — for a recipe that has been built — the stamp `build_recipes.sh --print-stamp` computes now. Nothing built (CI's gates job) skips only the stamp half.
- **`scripts/check_libc_license.sh`** — holds every crate `cargo metadata` resolves for `libc.so`, `libc.a`, `crt0.o` and `libbuiltins.a` to an SPDX expression MIT alone satisfies: the four packages and everything they depend on under `--all-features`, except through dev-dependencies (build-dependencies count: a build script's output is compiled in). MIT is the licence `slibc/NOTICE` gives such crates, and a third-party one needs an entry there; another licence needs its text there and a change to the gate. A crate of this tree must keep its manifest and every target under `slibc/`, `slibc-core/` or `abi/`, so neither a new crate elsewhere nor a crate root pointed outside passes on its licence field. `core`, `alloc` and `compiler_builtins` come from the standard library's workspace, which `cargo metadata` does not describe, and `slibc/NOTICE` records them by hand. The walk must reach slibc, `slibc-core` and `slopos-abi`, so a graph the gate cannot read fails instead of passing on the roots alone. Under those three directories every `include!`, `include_str!`, `include_bytes!` and attribute `path =`, read past comments and literals, must be a literal naming a file inside them, a `.rs` file where it is code, or a single file in `OUT_DIR`; a symlink there is refused. It reads spellings, not macro expansions. The one outside input is `toolchain/libc/`, which `slibc/build.rs` renders the headers from and which is `MIT OR Apache-2.0` itself. The library's licence is the sum of what it is built from, so one dependency on a GPL-3.0-or-later crate compiles, links and passes every test, and leaves git undistributable. `--self-test` grades twenty-seven fixtures, twenty-one of them rejections. Runs from `just check-framekernel-gates`: ~0.7 s, the self-test ~1.2 s.
- **`scripts/tcb_ratio.sh`** (via `just tcb-ratio`) — a hard gate at `--max 1.0` from both `just check-framekernel-gates` and `KERNEL_BUILD_GATES=1` builds. Prints lines of `unsafe` in `slopos-ostd/` divided by total kernel Rust LoC. Read it as a trend, not as a TCB fraction comparable to other projects': the denominator is raw LoC including the 41 kLoC vendored DWARF reader, and published comparators measure post-LTO linked code size.

`scripts/check_return_types.sh` is a separate, advisory `just check-return-types` recipe that flags `pub fn`s returning large by-value types — useful when reviewing new code, not part of the load-bearing build path.

### Allocation discipline
**`slopos_ostd::mm::heap` is the only kernel allocation surface.** Every kernel crate routes heap allocation through `slopos_ostd`'s `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, and `PinBox` rather than `alloc::*`. The `kernel/src/main.rs` global-allocator carve-out above is the lone exception.

The in-place-init primitive (`slopos_ostd::Init<T, E>`, `Zeroable`, `init_from_closure`, `init_zeroed`, `Field<T, U, OFF>` + `#[derive(SlotFields)]`) is **in-house** — defined in `slopos-ostd/src/mm/init.rs` with no external dependency on `pinned-init` or Rust-for-Linux's `pin-init`. Large structs must be constructed via `KBox::try_init(T::init_…())` / `PinBox::try_init(T::init_…())` so the `T` rvalue never materialises on the caller's stack. `check_stack_sizes.sh` enforces the upper bound from the other direction. `init_struct_with`'s closure must return `Initialised<T>`, which only `SlotPtr::finish` mints, so a caller cannot claim success without going through the slot; `finish` additionally checks field coverage under `debug_assertions`.

### Licensing discipline

SlopOS is `GPL-3.0-or-later`, except its C library.

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
The kernel ships a per-test harness that boots under QEMU, runs every `stest!`/`utest!` registration in lex order, and reports results over serial in KTAP grammar. The Go host wrapper (`tools/run_tests/` → `builddir/run_tests`) parses that stream into a live progress bar + per-failure detail. `just test` builds `builddir/slop-tests.iso` with `tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on`, runs QEMU with `isa-debug-exit`, and exits 0 green / 1 on any failure.

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
- `just check-fs-image` — hold the image the suite just wrote to `e2fsck -fn` and a clean superblock. Runs in CI after the test capture; an image SlopOS wrote that e2fsck rejects is a bug in SlopOS.
- `just test-persist` — two boots of one image with no rebuild between: write + `fsync` under `/var` on the disk root, power off, read back. In CI after `check-fs-image`. Needs its own boots and cannot reuse the shared capture.
- `just test-capacity` — the capacity check: build (once, then preserve) a 16 GiB ext2 volume, attach it as `virtio-disk3`, and let the suite mount it, walk it, write to it and report. Separate from `just test` because the image takes minutes to build and ~70M of host disk once populated; what CI grades per run is the cheaper `check-fs-throughput` ratchet below. `CAPACITY_IMAGE_SIZE` overrides the size; the guest measures a *mount* in device reads rather than in seconds, because reads are deterministic and wall time is not.
- `just test-devdisk` — the dev-disk check: build (once, then preserve) the 8 GiB volume a cross-built toolchain lands on, attach it as `virtio-disk4`, boot with it mounted at `/devel` by `mount=LABEL=slopos-dev:/devel` and 4G of RAM, and let `devdisk_test` read the staged inventory back, grade the source tree against its own vendor directory, mount the volume a second time, and — when the volume carries a toolchain — climb the toolchain ladder, whose last rung runs `git status` in the clone and `git ls-remote origin` against the host; on a volume this run created, that status must be clean, since nobody has edited that tree. Separate from `just test` for the reason `test-capacity` is: the volume is opt-in, and the same utest under `just test` passes by reporting that no dev disk is attached. `DEV_DISK_SIZE` overrides the size.
- `just test-install` — the install check: boot from `builddir/boot-disk.img` (GPT, one FAT32 ESP holding Limine, `/limine.conf` and one kernel per slot under `/boot/<slot>/`), and across the resets of one QEMU let `install_test` clone slot a into b with `/bin/bootctl`, boot it once through the Boot Loader Interface's `LoaderEntryOneShot`, commit it as `default_entry`, then boot once into a slot whose kernel panics with `panic=reboot` and see the reset land on the committed default. Boot-disk runs use a second, pinned OVMF (`third_party/ovmf-nv`, Arch's `edk2-ovmf`), because the nightly the ISO boots needs a secure varstore and keeps UEFI variables in RAM.
- `just test-install-guest` — the two loops in one QEMU: a clean tree, `just toolchain` and a dev disk; slot a is the optimized tests kernel, and `install_test`, finding a dev disk at `/devel`, fetches the host's `HEAD` into its clone, checks it out and runs `scripts/selfhost.sh install tests` there with a fresh `SLOPOS_BUILD_TAG` — a build-time variable that appears in `uname -v` and in the boot log's `BOOT: kernel <path> (<n> bytes), build tag <tag>` line, and is otherwise unset — which builds the tests kernel and installs it into slot b, then checks the tree's own branch out again; the run boots it once, and that boot must report the tag. The kernel the guest built then commits a change on the fetched `HEAD` in a scratch clone and pushes it into a scratch repository, which the host fetches and holds to that commit's parent being `HEAD`. The run then commits and rolls back as `test-install` does, and the host holds slot b's file to the dev disk's `kernel-tests.elf` byte for byte. `INSTALL_TIMEOUT_SECS` defaults to the self-hosting budget.
- `just test-selfhost` — the self-hosting check: needs `just toolchain` and a clean working tree (the host grades the guest's build of `HEAD` with its own gates and tests). The guest, booted on the optimized tests kernel (`release-tests`, gated by its own allowlists under `scripts/gates/{stack,vector}/`), fetches the host's `HEAD` into the dev disk's clone and checks it out — refusing a tree with uncommitted edits — builds the dev and tests kernels with `scripts/selfhost.sh build` (`selfhost_test`), leaving cargo's `--timings` report under the volume's `builddir/target/cargo-timings`, and checks the tree's own branch out again; the host holds the commit the guest names to `HEAD`, the volume to `e2fsck -fn` and a clean superblock, exports both kernels, runs the ELF gates on them and runs the kernel suite on the tests kernel. The boot's budget is eight hours, sized for KVM; `SELFHOST_TIMEOUT_SECS` raises it for TCG, which runs the guest's build about 25 times slower.
- `just bench-selfhost` — the self-hosting build as a profile: boots the optimized tests kernel with the dev disk and `prof=on` (`BENCH_PROF=` turns it off), runs only `selfhost_test`, so the guest builds the host's `HEAD`, and hands the log to `scripts/prof_report.py`, which prints the guest's build times, per-CPU busy and halted time, the ext2 lock's wait and hold (writeback's share apart) and the same lock and the per-process VM lock by call site, block I/O counts and latency, syscall costs, and kernel and user ticks symbolized — user ticks through the exec-mapping table the kernel prints, `builddir/bench-libc.so` and the installed toolchain's libraries. No grading and no clean-tree requirement; run it with nothing else loading the host, because every number in it is wall time.
- `just check-fs-throughput` — filesystem cost ratchet over the `FSPERF[…]` / `FSCAP[…]` report lines, with gate data in `scripts/gates/fsperf/<variant>.txt`. Counts per MiB — transactions, journal commits, device write requests, barriers — are deterministic for one ISO and carry caps; a write rate is not, so the only rate graded is the quotient of the filesystem's write rate and the **same run's** raw block-device write rate, which is invariant under a change of accelerator (the gate's `--self-test` asserts exactly that: a uniformly three-times-slower machine must still pass). Floors (`min-bytes`, `min-volume-gib`, `min-dirents`) exist because a measurement that stopped happening looks exactly like one that got free. `--log` / `--emit-allowlist` / `--self-test` as in the other ratchets.
- `just check-quota-headroom` — resource-quota ratchet; asserts every account's peak stays under its measured cap in `scripts/gates/quota/<variant>.txt`, that nothing was denied, and that the charge path has not got slower. What the `used`/`peak` packing buys is that a *reported* peak is a value that was genuinely held — the caps themselves are measured maxima carrying the observed spread as margin, exact only on the rows the gate file records as deterministic (`process`, and the fd/object rows). The **cost** check is one cap and two floors, never a cycle count: a cycle count on that path measures the accelerator, not the kernel, and the absolute caps this gate used to carry failed on the *unmodified* tree on any machine without `/dev/kvm`. The cap is `max-depth-cost-ratio` — depth 7 against depth 1, the only quantity here invariant under a change of accelerator. The floors are `min-charge-over-reference` (one charge+refund round trip against a same-run bare CAS, a floor and not a ceiling because that ratio *does* move with the accelerator) and `min-reference-cycles` (an absolute physical bound on the reference itself, since the first floor is a ratio over it). Stated plainly: a slowdown that scales the whole charge path uniformly passes every one of them, and catching it would need the absolute ceiling that failed without KVM. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate, with one difference: this gate's `--log` is a single run, so its file records spreads in prose rather than merging several logs mechanically. `--emit-allowlist` emits a depth cap a quarter above the observation, and its own output is round-tripped through the check path by the self-test — the property that makes "re-measure with `--emit-allowlist`" a remedy that actually works.
- `just check-lockdep-headroom` — lock-order ratchet; boots the test ISO and fails unless every phase the kernel reports (`boot`, `post-kernel-tests`, `post-userland-tests`) says `ACTIVE`, reports no violation, and stays inside the gate file's `max-fill-pct`. Gate data lives in `scripts/gates/lockdep/<variant>.txt` in the same measured-and-tracked style as the stack/vector gates, and an entry matching nothing fails as a dead entry. The three pools are not graded alike. Class counts are deterministic — a class registers on the first acquire of a declaration site, and three runs of one pinned ISO measured boot at 71 every time — so they carry **exact caps**. Boot's edge and chain counts carry caps rather than bands for the same reason, though the recorded values still hold the old convention's slack until they are re-measured onto the observed 43/110. The two test phases' edge and chain counts measure which orderings a run *happened to observe* and move between runs of identical code, so they carry **bands** (`band <phase> <pool> <lo> <hi>`) instead: leaving one prints `DRIFT` on stderr and the run still passes. Be clear about what that gives up — a banded pool has no upper failure of its own, so growth up to `max-fill-pct` (~3.5x observed) reaches you only as that DRIFT line; an *inverted* order is caught by the cycle detector and still fails. `min-classes` / `min-edges` / `min-chains` are the floors that stop a validator which quietly stopped recording from reading as maximally healthy. `--emit-allowlist` writes a fresh baseline (and accepts several `--log`s to merge), a single `--log FILE` parses a capture instead of booting, and `--self-test` (run from `check-framekernel-gates`) drives its crafted logs through the parser — proving both that the gate rejects and that it stays silent on the forms it deliberately accepts.

- `just check-sched-spread` — SMP placement-eligibility gate. Reads the `SCHEDCPU[<phase>]:` lines and holds the run to one structural invariant: **a CPU that is online is eligible for task placement.** Not a measured ceiling — the only tracked number is `eligible-lag`, how many online CPUs may legitimately not be eligible yet, which is `1` at `boot` and at `post-kernel-tests` (the BSP executes boot steps on the bootstrap stub and cannot dispatch until `enter_scheduler(0)` runs at the end of boot init) and `0` afterwards. A lag above that is a bug, not a number to raise. It exists because the failure is invisible: every placement helper filters candidates through `is_schedulable_cpu`, so a CPU whose runqueue is online but not *enabled* disappears from every fork, exec and wakeup while still dispatching whatever work stealing hands it — the machine boots, the suite passes, and one core does all the work. It shipped that way: `boot_step_scheduler_init` ran in the `services` phase and `init_all_percpu_schedulers` reset every runqueue it found, including the three APs that had already entered their scheduler loops during `drivers`. `allow-ineligible` records *which* CPUs that lag may name, because a count alone accepts an AP dropping out as readily as the BSP. `require-dispatch` is the companion check, so a CPU that is eligible but never actually dispatches also fails — and it is a flag, never a volume: how *many* switches a CPU makes is a property of the machine, so any floor above zero fails on a runner that is merely faster or slower. `--log` / `--emit-allowlist` / `--self-test` as in the lockdep gate.

All four boot-based ratchets (`check_test_count.sh`, `check_lockdep_headroom.sh`, `check_quota_headroom.sh`, `check_sched_spread.sh`) accept `--log`, and CI feeds them one `builddir/run_tests --raw --no-color` capture rather than booting QEMU once per question. Do the same locally — see the pre-commit sequence below — rather than paying four boots.

`check_sched_spread.sh` is the one gate in this list that is not a ratchet: its number describes the boot sequence, not a resource high-water mark, so a failure means a CPU stopped participating in scheduling and the fix is upstream of the gate.

A ratchet failure is a **measurement to re-take, not a number to raise**. Bump a cap only with a fresh `--emit-allowlist` in the same commit, and say in the commit message which lock, test or account added the delta. Never edit a gate file by hand to make a run pass.

### Cmdline knobs
The kernel parses these from the Limine cmdline (threaded through `scripts/build_iso.sh`'s third positional arg, controlled by the `test_cmdline` justfile constant or the `TEST_CMDLINE=…` env override). For manual `just boot-log` invocations, set `BOOT_CMDLINE='tests=on tests.run=mm::*'` to run a subset.

| Key | Values | Effect |
|---|---|---|
| `tests` | `on` / `off` | master enable |
| `tests.shutdown` | `on` / `off` | write to `isa-debug-exit` after the run |
| `tests.verbosity` | `quiet` / `summary` / `verbose` | per-test emit policy |
| `tests.warn_ms` | integer | mark slower tests as `OVER_TIME` |
| `tests.run` | comma-separated globs | only run matching tests |
| `tests.skip` | comma-separated globs | skip matching tests |
| `root` | `auto` / `initramfs` / `virtio` / `/dev/vdX[N]` / `vdX[N]` | which filesystem `/` is. `auto` prefers a writable `disk0` and falls back to the initramfs; a device name selects a probe-order device and, with a number, a GPT/MBR partition of it; an absent device or partition degrades to the initramfs with a klog line |
| `mount` | `<device>:/<path>` / `LABEL=<label>:/<path>`, repeatable | mount an ext2 volume read-write after the root is up, in cmdline order; the source takes every spelling `mount(2)` accepts. A failure is one klog line and the boot goes on |
| `lockdep` | `off` / `warn` / `panic` | lock-order validator policy; default `panic` |
| `verity` | `require` | an attached disk must mount with a verity trailer or the `fs init` boot step fails; no disk at all still passes. `just iso` sets it: the live ISO trusts no disk it finds without a trailer |
| `sched.ap_pause_ms` | integer | wall-clock budget for the AP pause; `0` disables the deadline and falls back to the iteration bound. Default measured — see `AP_PAUSE_BUDGET_NS_DEFAULT` |
| `kconsole` | `off` / `on` / `<hex mask>` | diagnostic-console permission mask; default `on` (informational only) |
| `kconsole.serial` | `on` / `off` | serial BREAK trigger; default `on` |
| `kconsole.arm_ms` | integer | how long the keyboard chord stays armed; default 3000 |
| `kconsole.max_lines` | integer | per-command line budget; default 512 |
| `kconsole.probe_ms` | integer | per-CPU answer budget for the all-CPU probe; default 250 |
| `watchdog.miss_threshold` | integer | consecutive heartbeat samples a CPU may miss before the watchdog reports it; default 100, `0` refused |
| `watchdog.panic` | `on` / `off` | whether a stall five thresholds long is fatal; default on bare metal only, since under a hypervisor a descheduled vCPU looks the same |
| `mem.commit` | integer | percent of usable frames the commit ledger may promise to private mappings; default 100, capped at 400, `0` measures without a ceiling |
| `prof` | `on` | sample where the time goes — per-CPU user/kernel/idle ticks and halted time, the hottest kernel RIPs, user ticks by task name, and the kernel stacks blocked user tasks are parked on whenever a CPU idles — printed as `PROF[post-userland-tests]:` lines; off by default. `TEST_CMDLINE_EXTRA=prof=on just test-selfhost` profiles the guest's build |
| `panic` | `reboot` | a kernel panic resets the machine (ACPI, then `0xCF9`, then the keyboard controller) instead of halting it: how a boot slot that panics falls back to the loader's default |
| `panic.boot` | `on` | panic as soon as boot initialisation completes — the broken slot `just test-install` rolls back from |

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
then the command key. Press the trigger then `h` for the list.

Commands live in the `.kconsole_registry` linker registry, so the crate that
owns a subsystem's data owns the command that prints it — `kcommand!` in `mm/`,
`sched/`, `core/`, `boot/`. OSTD defines the registry and never names an entry;
registration must happen in a crate only the kernel links, because OSTD is
linked into userland binaries too and their linker script brackets no kernel
section.

Three properties are load-bearing:

- **Only the physical console triggers it.** The keyboard hook sits in the IRQ
  handler ahead of layout resolution and consumes its keys, so they reach
  neither the TTY nor the focused GUI application; the serial trigger is a BREAK
  condition, which no byte pattern can forge. There is no call edge from any
  userland write path to `kconsole::request`, and that is the point — the
  facility this replaced was reachable by any process holding a PTY master.
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

A utest's cases reach the wire as subtest lines ahead of its own result line, written past the per-test klog capture: a long utest overflows that ring, and a verdict lost with it leaves a failure with no name. A passing case's note follows `#` on its line, which is where the dev-disk ladder records rustc's startup time and each build's wall time.

## Commit & Pull Request Guidelines
Subjects are `<area>: <imperative summary>` (e.g., `mm: tighten buddy free path`), ≤72 chars. Add a body for rationale, boot implications, or follow-ups. For PRs include: motivation, testing artifacts (command + result), issue references, and serial excerpts or screenshots when boot flow or visible output changes. Flag breaking changes and downstream-script coordination.

### Commit messages (MANDATORY)
**Always use the `caveman-commit` skill to write the commit message. Never hand-write one.**

- Load it with `/skill:caveman-commit`, or read `~/.agents/skills/caveman-commit/SKILL.md` directly if skill commands are unavailable.
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

# The tests userland has staged the cross-built runtime now, so the stamp and
# symbol halves of the C++ gate run. Both are skipped by the gates above.
scripts/check_cxx_pin.sh

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

CI is four parallel jobs, and its wall clock is the longest one: the boot lane (`ci`), which holds only the tests build, the boot and the graders that read its capture. Every job pays its own setup in runner minutes, so a new check joins the job whose inputs it needs — `gates` for the source tree and a kernel, `toolchain` for the staged tests userland and the patched source trees — rather than a job of its own, and goes in the boot lane only if it reads the capture. Compiler output is cached by content: the LLVM and clang builds under ccache through CMake's `CMAKE_C_COMPILER_LAUNCHER`/`CMAKE_CXX_COMPILER_LAUNCHER` environment variables, which a first configure reads, and the Rust builds under the sccache `scripts/ensure_sccache.sh` pins, as `RUSTC_WRAPPER` on build steps only — never on a gate that probes what a compiler does, since a cache hit there would be a cached answer. Locally, export the two CMake variables for the same warm C++ rebuilds, and set `RUSTC_WRAPPER` on a build command rather than in the shell, for the same reason. `plans/ci-latency.md` is the measured model and the work still open.

Commit order: `cargo fmt --all` → the sequence above → stage → `caveman-commit` for the message → `git commit`.

## Environment & Tooling Tips
First-time developers should run `scripts/setup_ovmf.sh` to download firmware blobs; keep them under `third_party/ovmf/`. The ISO builder auto-downloads the pinned Limine binary release into `third_party/limine`; offline environments should pre-populate that directory (it only needs `limine-bios.sys` + `BOOTX64.EFI`) or set `LIMINE_URL`/`LIMINE_VERSION` to avoid network stalls. Rust crates are auto-discovered via the workspace, so most build changes belong in `justfile`, `scripts/`, `Cargo.toml`, and `targets/*.json`; ensure `link.ld` maps any new sections intentionally. The entry point is the assembly `_start` trampoline, which jumps into `kernel_main`; keep `no_std`, rely on `rust-lld`, and avoid host installs. **SlopOS requires LAPIC + IOAPIC hardware (or QEMU `q35`/`-machine q35,accel=kvm:tcg` with IOAPIC enabled); the legacy 8259 PIC path has been sacrificed to the Wheel of Fate, so the kernel panics immediately if an IOAPIC cannot be discovered. VirtIO devices require MSI-X (preferred) or MSI as a minimum — legacy polling has been removed; probe panics if neither interrupt mechanism is available.**

`scripts/make_slopos_sysroot.sh` needs the pinned `libc` crate; it takes it from `$CARGO_HOME/registry/cache` when it is already there and only falls back to `static.crates.io`, so an offline environment should pre-populate that cache (or point `LIBC_URL` at a local copy). It also needs the `rust-src` component, which `scripts/ensure_toolchain.sh` installs.

## Safety & Execution Boundaries
Keep all work inside this repository. Do not copy kernel binaries to system paths, do not install or chainload on real hardware, and never run outside QEMU/OVMF. The scripts already sandbox execution; if you need fresh firmware or boot assets, use the provided automation instead of manual installs. Treat Limine, OVMF, and the kernel as development artifacts only and avoid touching `/boot`, `/efi`, or other host-level locations.


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
