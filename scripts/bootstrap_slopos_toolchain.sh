#!/usr/bin/env bash
set -euo pipefail

# Cross-build the Rust toolchain that runs on SlopOS.
#
# Usage: bootstrap_slopos_toolchain.sh [--dry-run] [--stage <dir>] [-- <x.py args>]
#
# One bootstrap invocation, `--build=x86_64-unknown-linux-gnu
# --host=x86_64-unknown-slopos`, producing rustc, cargo, rust-lld, clang and
# libLLVM for SlopOS out of `third_party/slopos-rustc-src`. Nothing in that
# sentence is novel — it is how every cross-hosted Rust distribution is
# produced — and everything in it depends on what this tree already has: a
# built-in target rustc can resolve, a C library and a C++ runtime for the
# triple, a dynamic loader, and a cargo whose C-backed dependencies are
# behind a feature.
#
# Three things bootstrap cannot work out for itself, and this script supplies
# all three:
#
#   * A C and C++ compiler for the target. `[target.<triple>].cc` is a program
#     name with no room for flags, and the host clang has no SlopOS toolchain
#     to find `crt0.o` and `-lc` with — `toolchains::SlopOS` is in the port,
#     which only a clang built *from* this tree carries. So the wrapper below
#     stands in, exactly as Motor OS's `motor-clang` does, and completing an
#     executable link is the half that matters: CMake's `try_compile` probes
#     link, and a probe that fails to link is a capability LLVM then builds
#     without.
#   * `--no-default-features` for cargo. `build.tool.<name>.features` can only
#     add, so `toolchain/compiler/0002-bootstrap-tool-default-features.patch`
#     adds the key this config sets.
#   * An LLVM for the host triple. `llvm.download-ci-llvm` serves the build
#     triple only, so LLVM is built from source for SlopOS, with clang and lld
#     in the same pass.
#
# `--dry-run` runs bootstrap's own dry run: it validates the config, resolves
# `--host` through the compiler's built-in target list, and walks the step
# graph without compiling anything. That is the half of this a gate can
# afford, and it is what `scripts/check_bootstrap_config.sh` runs.
#
# Environment:
#   SLOPOS_SYSROOT   - the target sysroot (default: builddir/slopos-sysroot,
#                      assembled here from the staged libraries and headers)
#   BUILD_DIR        - where artifacts go (default: builddir)
#   SLOPOS_TOOLCHAIN_OUT - the wrapper and config directory
#                      (default: <build dir>/slopos-toolchain)
#   BOOTSTRAP_JOBS   - -j for x.py (default: nproc)

SELF="bootstrap_slopos_toolchain"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

DRY_RUN=0
STAGE=""
XPY_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --stage)
            STAGE="${2:?--stage needs a directory}"
            shift 2
            ;;
        --)
            shift
            XPY_ARGS=("$@")
            break
            ;;
        *) die "unknown argument: $1" ;;
    esac
done

TARGET="x86_64-unknown-slopos"
HOST_TRIPLE="x86_64-unknown-linux-gnu"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
SRC="$REPO_ROOT/$TP_RUSTC_SRC_REL"
OUT="${SLOPOS_TOOLCHAIN_OUT:-$BUILD_DIR/slopos-toolchain}"
SYSROOT="${SLOPOS_SYSROOT:-$BUILD_DIR/slopos-sysroot}"
RUSTC_BUILD="$BUILD_DIR/slopos-rustc-build"
CXX_DIR="$REPO_ROOT/third_party/slopos-cxx"
JOBS="${BOOTSTRAP_JOBS:-$(nproc)}"

[ -f "$SRC/x.py" ] || die "no rustc sources at $TP_RUSTC_SRC_REL — run scripts/make_rustc_src.sh"
[ "$(cat "$SRC/$TP_STAMP_NAME" 2>/dev/null)" = "$(tp_rustc_stamp "$REPO_ROOT")" ] ||
    die "$TP_RUSTC_SRC_REL is stale — run scripts/make_rustc_src.sh"
command -v python3 >/dev/null 2>&1 || die "python3 is required to run x.py"

CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"

# ---------------------------------------------------------------------------
# Two subtrees a real run needs and no gate does, taken from the tarball
# `make_rustc_src.sh` already fetched. Never on a dry run, so the gate that
# drives one keeps paying nothing for them.
#
#   src/llvm-project  1.4 GB of C++, carrying `toolchain/llvm-rustc/` — the
#                     port a second time, for the reason
#                     `toolchain/compiler/PIN` gives.
#   library/          the std and libc forks. The sysroot's copy is what
#                     `-Zbuild-std` reads; this one is what bootstrap builds
#                     the target's std out of, and without it `library/libc`
#                     has no `slopos` module and `std` does not compile for
#                     the triple at all.
#
# Each carries its own stamp over the overlay it was staged from — `library/`
# the sysroot's, since both trees hold the same two patches and an edit to
# either must re-stage this one. A directory-existence check alone would
# leave either silently describing the previous fork. Re-staging re-extracts
# first, because the patches do not apply twice.
# ---------------------------------------------------------------------------
CHANNEL="$(tp_channel "$REPO_ROOT")"
TARBALL="$REPO_ROOT/third_party/rustc-src-$CHANNEL.tar.xz"
LIBRARY_STAMP="$SRC/library/.slopos-std-stamp"
STD_STAMP_WANT="$(tp_stamp "$REPO_ROOT")"
LLVM_STAMP="$SRC/src/llvm-project/.slopos-port-stamp"
LLVM_STAMP_WANT="$(
    cd "$REPO_ROOT" && find "$TP_LLVM_RUSTC_OVERLAY_REL" -type f -print |
        tp_hash_lines "$REPO_ROOT" | tp_sha256_stream
)"

if [ "$DRY_RUN" -eq 0 ]; then
    [ -f "$TARBALL" ] ||
        die "no $TARBALL to take src/llvm-project and library/ from — run scripts/make_rustc_src.sh"
    command -v git >/dev/null 2>&1 || die "git is required to apply the forks"

    if [ "$(cat "$LLVM_STAMP" 2>/dev/null)" != "$LLVM_STAMP_WANT" ]; then
        rm -rf "$SRC/src/llvm-project"
        echo "$SELF: extracting rustc's llvm-project (about 1.4 GB on disk)..." >&2
        tar -xf "$TARBALL" -C "$SRC" --strip-components=1 'rustc-nightly-src/src/llvm-project' ||
            die "failed to unpack src/llvm-project"
        LLVM_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_LLVM_RUSTC_OVERLAY_REL/")" ||
            die "the llvm port did not apply to $TP_RUSTC_SRC_REL/$TP_LLVM_RUSTC_TREE_REL"
        [ "$LLVM_PATCHES" != "0" ] ||
            die "no patches under $TP_LLVM_RUSTC_OVERLAY_REL/ — an unported LLVM has no SlopOS triple"
        printf '%s\n' "$LLVM_STAMP_WANT" >"$LLVM_STAMP"
    fi

    if [ "$(cat "$LIBRARY_STAMP" 2>/dev/null)" != "$STD_STAMP_WANT" ]; then
        rm -rf "$SRC/library"
        tar -xf "$TARBALL" -C "$SRC" --strip-components=1 'rustc-nightly-src/library' ||
            die "failed to unpack library/"
        tp_unpack_libc_crate "$REPO_ROOT" "$SRC/library/libc" ||
            die "could not stage the pinned libc crate into the source tree"
        # libc first: the std patch adds `libc = { path = "libc" }` under
        # `[patch.crates-io]`, so the tree it names has to exist already.
        LIBC_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_OVERLAY_REL/libc/" "$TP_RUSTC_SRC_REL/library")" ||
            die "the libc fork did not apply to $TP_RUSTC_SRC_REL/library"
        STD_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_OVERLAY_REL/rust/" "$TP_RUSTC_SRC_REL/library")" ||
            die "the std fork did not apply to $TP_RUSTC_SRC_REL/library"
        [ "$LIBC_PATCHES" != "0" ] && [ "$STD_PATCHES" != "0" ] ||
            die "no std or libc patches applied — an unpatched library/ has no slopos std"
        printf '%s\n' "$STD_STAMP_WANT" >"$LIBRARY_STAMP"
    fi
fi

# ---------------------------------------------------------------------------
# The target sysroot: what a cross compiler for this triple needs to find.
# Assembled rather than pointed at, because the pieces live in three places —
# the userland build's output, slibc's generated headers, and the cross-built
# C++ runtime.
# ---------------------------------------------------------------------------
RELEASE_DIR="$BUILD_DIR/target/$TARGET/release"
for library in libc.so crt0.o libbuiltins.a; do
    [ -f "$BUILD_DIR/$library" ] ||
        die "no $library in $BUILD_DIR — run a tests userland build first"
done
[ -f "$RELEASE_DIR/libc.a" ] || die "no libc.a in $RELEASE_DIR — run a tests userland build first"
[ -f "$CXX_DIR/lib/libc++.so" ] || die "no C++ runtime — run scripts/make_slopos_cxx.sh"

rm -rf "$SYSROOT"
mkdir -p "$SYSROOT/lib" "$SYSROOT/include"
cp "$BUILD_DIR/libc.so" "$BUILD_DIR/crt0.o" "$BUILD_DIR/libbuiltins.a" "$SYSROOT/lib/"
cp "$RELEASE_DIR/libc.a" "$SYSROOT/lib/"
cp "$CXX_DIR/lib/libc++.so" "$CXX_DIR/lib/libc++.a" "$SYSROOT/lib/"
cp -r "$REPO_ROOT/slibc/include/." "$SYSROOT/include/"
cp -r "$CXX_DIR/include/c++" "$SYSROOT/include/c++"

# slibc is one library: there is no separate libm, libdl, libpthread or
# librt, and a build system that probes for them finds the host's unless
# something answers. Empty archives are what musl-derived sysroots answer
# with, and they turn a probe that would link against glibc into one that
# links against nothing.
for stub in m dl pthread rt util; do
    "$LLVM_AR" crs "$SYSROOT/lib/lib$stub.a"
done

# ---------------------------------------------------------------------------
# The compiler wrapper bootstrap hands to CMake and to every `-sys` build
# script. It is `toolchains::SlopOS` written in shell, and it goes away the
# day a clang built from `toolchain/llvm/` is the one running the build.
#
# It compiles and links in two invocations, with two different triples, and
# that is the whole reason it exists. Compilation must name SlopOS, or the
# preprocessor defines `__linux__` and LLVM takes `/proc/self/exe` and
# `sched_getaffinity` paths this system has not got. Linking must not: the
# host clang has no SlopOS toolchain, so for that triple it hands the link to
# `gcc`, which would make a host GCC a build requirement
# `scripts/cxx_host_tools.sh` deliberately does not have and would put the
# host's library directories on the line. Naming a triple clang does have a
# toolchain for keeps `ld.lld` the linker, and `--sysroot` confines the search
# to this sysroot — `-lm` against the host's libm is then a link error rather
# than a binary that dies on SlopOS.
WRAPPER_DIR="$OUT/bin"
mkdir -p "$WRAPPER_DIR" "$OUT/find-root"

case "$SYSROOT$REPO_ROOT" in
    *[[:space:]]*) die "the wrapper cannot take a path containing whitespace: $REPO_ROOT" ;;
esac

write_wrapper() {
    local path="$1" compiler="$2" stdlib="$3" stdlib_libs="${4:-}"
    cat >"$path" <<WRAPPER
#!/bin/sh
set -e
# Generated by scripts/$SELF.sh — do not edit.
sysroot="$SYSROOT"
cflags="--target=$TARGET -D__slopos__ -nostdlibinc $stdlib -isystem \$sysroot/include"
cflags="\$cflags -Wno-unused-command-line-argument"

# Response files are expanded first: CMake writes one when a link line grows
# past the argument limit, and a scan that sees only \`@file\` classifies a
# link as a compile and hands the whole thing to the host driver.
expanded=""
for arg in "\$@"; do
    case "\$arg" in
        @*)
            file="\${arg#@}"
            [ -f "\$file" ] || { echo "\$0: no response file \$file" >&2; exit 1; }
            expanded="\$expanded \$(tr '\n' ' ' <"\$file")"
            ;;
        *) expanded="\$expanded \$arg" ;;
    esac
done
# shellcheck disable=SC2086
set -- \$expanded

# A caller's own \`--target\` is dropped rather than overridden. The \`cc\`
# crate appends one built from Cargo's \`CARGO_CFG_TARGET_*\`, which spells
# the environment as a fourth component — \`x86_64-unknown-slopos-slibc\` —
# and clang reads that as a version field and refuses the triple. Measured:
# it is what stopped \`compiler_builtins\`' build script.
#
# \`-Xlinker\`, \`-Xassembler\` and \`-Xpreprocessor\` take an operand that is
# a flag for *that* tool, so it is carried through without being read as one
# of ours: \`-Xlinker -E\` is \`--export-dynamic\`, not \`clang -E\`.
linking=1
shared=0
static=0
carry=0
drop=0
remaining=\$#
while [ "\$remaining" -gt 0 ]; do
    arg="\$1"
    shift
    remaining=\$((remaining - 1))
    if [ "\$drop" -eq 1 ]; then
        drop=0
        continue
    fi
    if [ "\$carry" -eq 1 ]; then
        carry=0
        set -- "\$@" "\$arg"
        continue
    fi
    case "\$arg" in
        --target=*) continue ;;
        -target)
            drop=1
            continue
            ;;
        -Xlinker | -Xassembler | -Xpreprocessor) carry=1 ;;
        -c | -S | -E | -M | -MM | -fsyntax-only | -### | --version | --help | -dumpmachine | -dumpversion | -print-* | --print-*)
            linking=0
            ;;
        -shared) shared=1 ;;
        -static) static=1 ;;
    esac
    set -- "\$@" "\$arg"
done

if [ "\$linking" -eq 0 ]; then
    exec $compiler \$cflags "\$@"
fi

# A link invocation may still carry sources — CMake's \`try_compile\` is
# exactly that shape. Each is compiled for SlopOS first; what reaches the
# link is objects only. Anything that is neither a source, an input file nor
# a linker argument reaches both phases, because \`-O2\`, \`-g\` and \`-flto\`
# all mean something to each.
objects=""
link_args=""
compile_args=""
output=""
sources=""
skip=0
carry=0
for arg in "\$@"; do
    if [ "\$skip" -eq 1 ]; then
        output="\$arg"
        skip=0
        continue
    fi
    if [ "\$carry" -eq 1 ]; then
        carry=0
        link_args="\$link_args \$arg"
        continue
    fi
    case "\$arg" in
        -o) skip=1 ;;
        -o*) output="\${arg#-o}" ;;
        -Xlinker)
            carry=1
            link_args="\$link_args \$arg"
            ;;
        -l* | -L* | -Wl,* | -shared | -static | -rdynamic | -pie | -no-pie)
            link_args="\$link_args \$arg"
            ;;
        -*)
            compile_args="\$compile_args \$arg"
            link_args="\$link_args \$arg"
            ;;
        *.c | *.cc | *.cpp | *.cxx | *.C | *.s | *.S) sources="\$sources \$arg" ;;
        *.o | *.a | *.so | *.so.*) link_args="\$link_args \$arg" ;;
        *)
            compile_args="\$compile_args \$arg"
            link_args="\$link_args \$arg"
            ;;
    esac
done
[ "\$skip" -eq 0 ] || { echo "\$0: -o with no operand" >&2; exit 1; }

tmp="\$(mktemp -d)"
trap 'rm -rf "\$tmp"' EXIT INT TERM
for source in \$sources; do
    obj="\$tmp/\$(printf '%s' "\$source" | tr '/.' '__').o"
    $compiler \$cflags \$compile_args -c "\$source" -o "\$obj"
    objects="\$objects \$obj"
done
[ -n "\$output" ] || output=a.out

# Objects ahead of \`-l\` and \`.a\`: archive resolution is order-sensitive, and
# a \`try_compile\` with \`CMAKE_REQUIRED_LIBRARIES\` is one source plus one
# \`-l\`.
set -- --target=$HOST_TRIPLE --sysroot="\$sysroot" --gcc-toolchain="\$sysroot" -fuse-ld=lld -nostdlib \\
    -Wno-unused-command-line-argument -L"\$sysroot/lib" \\
    \$objects \$link_args -o "\$output" -Wl,--eh-frame-hdr
if [ "\$shared" -eq 0 ]; then
    set -- -no-pie "\$@" -Wl,--image-base=0x400000 "\$sysroot/lib/crt0.o"
fi
if [ "\$static" -eq 0 ]; then
    set -- "\$@" -Wl,-z,now
    [ "\$shared" -eq 1 ] || set -- "\$@" -Wl,--dynamic-linker=/lib/ld-slopos.so.1
fi
set -- "\$@" $stdlib_libs -lc "\$sysroot/lib/libbuiltins.a"
status=0
$compiler "\$@" || status=\$?
rm -rf "\$tmp"
trap - EXIT INT TERM
exit "\$status"
WRAPPER
    chmod +x "$path"
}

# `--print-abi-flags` and not a literal: the rune-table flag decides the width
# and bits of `ctype_base::mask`, a type passed by value, so a consumer that
# omits it disagrees with the runtime about it — and libc++'s own `__locale`
# then names glibc's `_ISalpha` and does not compile at all.
CXX_ABI_FLAGS="$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"
write_wrapper "$WRAPPER_DIR/$TARGET-clang" "$CLANG" ""
write_wrapper "$WRAPPER_DIR/$TARGET-clang++" "$CLANGXX" \
    "-nostdinc++ -isystem $SYSROOT/include/c++/v1 $CXX_ABI_FLAGS" "-lc++"

# ---------------------------------------------------------------------------
# bootstrap.toml. `target` carries the build triple as well as the host one:
# `--host` alone would default `target` to SlopOS and drop the Linux std the
# stage-1 compiler is built against.
# ---------------------------------------------------------------------------
CONFIG="$OUT/bootstrap.toml"
cat >"$CONFIG" <<CONFIG_END
# Generated by scripts/$SELF.sh — do not edit.
# Regenerated on every run, so there is no stale file for a change notice
# to be about.
change-id = "ignore"
[build]
build = "$HOST_TRIPLE"
build-dir = "$RUSTC_BUILD"
host = ["$TARGET"]
target = ["$HOST_TRIPLE", "$TARGET"]
extended = true
tools = ["cargo", "src"]
docs = false
submodules = false
# Empty rather than bootstrap's "built from a source tarball": the version
# string is hashed into every crate's StableCrateId, so a kernel this compiler
# builds matches the host's only if both name themselves alike.
description = ""
vendor = false
# jemalloc is a C library nobody has ported here, and it is the default
# allocator for a unix host.
allocator = "system"
[build.tool.cargo]
default-features = false

[llvm]
download-ci-llvm = false
clang = true
link-shared = true
targets = "X86"
ninja = true
# A cross find_package searches the *host*: without an empty find root,
# FindZLIB and friends take /usr/include, and a wchar.h that reaches glibc's
# mbstate_t collides with slibc's. Measured: it is what stopped the cross
# LLVM.
build-config = { CMAKE_FIND_ROOT_PATH = "$OUT/find-root", CMAKE_FIND_ROOT_PATH_MODE_INCLUDE = "ONLY", CMAKE_FIND_ROOT_PATH_MODE_LIBRARY = "ONLY", CMAKE_FIND_ROOT_PATH_MODE_PROGRAM = "NEVER", LLVM_ENABLE_ZLIB = "OFF", LLVM_ENABLE_ZSTD = "OFF", LLVM_ENABLE_TERMINFO = "OFF", LLVM_ENABLE_LIBXML2 = "OFF", LLVM_ENABLE_LIBEDIT = "OFF", LLVM_ENABLE_LIBPFM = "OFF", LLVM_ENABLE_BACKTRACES = "OFF", LLVM_ENABLE_CRASH_OVERRIDES = "OFF" }

[rust]
channel = "nightly"
lld = true
rpath = true
# The pinned libc fork is upstream's release plus one module, and a newer
# rustc lints it; denying would make that fork's warnings this build's
# problem.
deny-warnings = false

[target.$TARGET]
cc = "$WRAPPER_DIR/$TARGET-clang"
cxx = "$WRAPPER_DIR/$TARGET-clang++"
ar = "$LLVM_AR"
ranlib = "$LLVM_AR"
linker = "$WRAPPER_DIR/$TARGET-clang"
crt-static = false

[install]
# Both, and both under the build directory: bootstrap asserts it can write
# sysconfdir as well as prefix, and that one defaults to an absolute /etc no
# ordinary user owns.
prefix = "$OUT/install.partial"
sysconfdir = "etc"
CONFIG_END

if [ "$DRY_RUN" -eq 1 ]; then
    echo "$SELF: dry run — $CONFIG"
    (cd "$SRC" && python3 x.py install --config "$CONFIG" --dry-run "${XPY_ARGS[@]}")
    exit 0
fi

# Completed under another name and renamed last: a build that stops part way
# must not leave a prefix the dev disk would take for a toolchain.
PREFIX="$OUT/install.partial"
rm -rf "$PREFIX"
(cd "$SRC" && python3 x.py install --config "$CONFIG" --jobs "$JOBS" "${XPY_ARGS[@]}")

# `x.py install` ships no clang, and a Linux std a SlopOS-hosted compiler has
# no use for. The target sysroot goes into the same prefix, so the clang
# config can name `<CFGDIR>/..` and the tree carries one sysroot, not two.
LLVM_DIR="$RUSTC_BUILD/$TARGET/llvm"
CLANG_BIN="$(cd "$LLVM_DIR/bin" && ls clang-[0-9]*)" ||
    die "no clang in $LLVM_DIR/bin — was llvm.clang dropped from the config?"
rm -rf "$PREFIX/lib/rustlib/$HOST_TRIPLE"
cp -a "$LLVM_DIR/bin/$CLANG_BIN" "$PREFIX/bin/"
cp -a "$LLVM_DIR"/lib/libclang-cpp.so* "$PREFIX/lib/"
cp -a "$LLVM_DIR/lib/clang" "$PREFIX/lib/"
cp -a "$SYSROOT/lib/." "$PREFIX/lib/"
cp -a "$SYSROOT/include" "$PREFIX/"
ln -sfn "$CLANG_BIN" "$PREFIX/bin/clang"
ln -sfn clang "$PREFIX/bin/clang++"
ln -sfn clang "$PREFIX/bin/cc"
ln -sfn clang++ "$PREFIX/bin/c++"
# rust-lld and llvm-tools find their own copy of libLLVM through
# `$ORIGIN/../lib`, and it needs the C++ runtime from the same directory.
ln -sfn ../../../libc++.so "$PREFIX/lib/rustlib/$TARGET/lib/libc++.so"
ln -sfn "../lib/rustlib/$TARGET/bin/rust-lld" "$PREFIX/bin/ld.lld"
printf '%s\n' '--sysroot=<CFGDIR>/..' >"$PREFIX/bin/$TARGET.cfg"
printf '%s\n' "@$TARGET.cfg" >"$PREFIX/bin/$TARGET-clang.cfg"
printf '%s\n' "@$TARGET.cfg" $CXX_ABI_FLAGS >"$PREFIX/bin/$TARGET-clang++.cfg"

rm -rf "$OUT/install"
mv "$PREFIX" "$OUT/install"
PREFIX="$OUT/install"

if [ -n "$STAGE" ]; then
    rm -rf "$STAGE"
    mkdir -p "$STAGE"
    cp -a "$PREFIX/." "$STAGE/"
    echo "$SELF: staged the toolchain at $STAGE — pass it as TOOLCHAIN_STAGE to scripts/build_devdisk.sh"
fi

echo "$SELF: built the $TARGET toolchain into $PREFIX"
