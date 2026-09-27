#!/usr/bin/env bash
set -euo pipefail

# Cross-build the Rust toolchain that runs on SlopOS.
#
# Usage: bootstrap_slopos_toolchain.sh [--dry-run] [--no-pgo | --pgo] [--sources-only]
#                                      [--stage <dir>] [-- <x.py args>]
#
# One bootstrap invocation, `--build=x86_64-unknown-linux-gnu
# --host=x86_64-unknown-slopos`, producing rustc, cargo, rust-lld, clang and
# libLLVM for SlopOS out of `third_party/slopos-rustc-src`. Nothing in that
# sentence is novel — it is how every cross-hosted Rust distribution is
# produced — and everything in it depends on what this tree already has: a
# built-in target rustc can resolve, a C library and a C++ runtime for the
# triple, and a dynamic loader.
#
# Two things bootstrap cannot work out for itself, and this script supplies
# both:
#
#   * A C and C++ compiler for the target. `[target.<triple>].cc` is a program
#     name with no room for flags, and the host clang has no SlopOS toolchain
#     to find `crt0.o` and `-lc` with — `toolchains::SlopOS` is in the port,
#     which only a clang built *from* this tree carries. So the wrapper
#     `scripts/make_slopos_cross.sh` writes stands in, exactly as Motor OS's
#     `motor-clang` does, and completing an
#     executable link is the half that matters: CMake's `try_compile` probes
#     link, and a probe that fails to link is a capability LLVM then builds
#     without.
#   * An LLVM for the host triple. `llvm.download-ci-llvm` serves the build
#     triple only, so LLVM is built from source for SlopOS, with clang and lld
#     in the same pass.
#
# cargo's network features link the C libraries `scripts/build_recipes.sh`
# builds (zlib, nghttp2, OpenSSL, curl, libssh2, libgit2). Their shared
# libraries and headers join the target sysroot, so the wrapper links them
# with nothing but its own search path — as `libz-sys` probes `-lz` — and
# they reach the install beside cargo. Each `-sys` crate is pointed at them
# for the SlopOS target alone; `libnghttp2-sys` has no such option and
# compiles its bundled copy, which `curl-sys` links only when it builds its
# own libcurl. `scripts/check_bootstrap_config.sh` holds the installed cargo
# to the recipes.
#
# `--dry-run` runs bootstrap's own dry run: it validates the config, resolves
# `--host` through the compiler's built-in target list, and walks the step
# graph without compiling anything. That is the half of this a gate can
# afford, and it is what `scripts/check_bootstrap_config.sh` runs.
#
# `--pgo` builds the compiler as a Rust release is: ThinLTO and one codegen
# unit for rustc's crates, ThinLTO for LLVM
# (`scripts/lib/rustc_build_settings.sh`), and profile-guided optimisation of
# both, with the profiles `scripts/make_toolchain_profile.sh` gathers from a
# Linux-hosted build of the same sources compiling this repository's kernel;
# every crate is then compiled through `scripts/rustc_neutral_metadata.sh`,
# so the SlopOS build names its symbols as the profiled Linux one does. The
# profiles are cached under `<build dir>/slopos-pgo` and regenerated only when
# an input to them changes, which the first time and after a compiler patch
# costs a Linux LLVM built twice, a stage1 and a stage2 compiler, and two
# kernel builds on instrumented compilers. It is opt-in until a SlopOS-hosted
# compiler built that way has been through `just test-devdisk`; without it
# (`--no-pgo`, the default) the configuration is the plain one.
#
# `--sources-only` stages the source subtrees below and exits: what the
# profile build needs from this script.
#
# Environment:
#   SLOPOS_SYSROOT   - the target sysroot (default: builddir/slopos-sysroot,
#                      assembled by scripts/make_slopos_cross.sh)
#   SLOPOS_RECIPES_DIR - the recipes' build tree (default:
#                      <build dir>/slopos-recipes)
#   BUILD_DIR        - where artifacts go (default: builddir)
#   SLOPOS_TOOLCHAIN_OUT - the wrapper and config directory
#                      (default: <build dir>/slopos-toolchain)
#   SLOPOS_TOOLCHAIN_PGO - 1 is `--pgo` (default: 0)
#   BOOTSTRAP_JOBS   - -j for x.py (default: nproc)

SELF="bootstrap_slopos_toolchain"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"
. "$SCRIPT_DIR/lib/tree_sync.sh"
. "$SCRIPT_DIR/lib/rustc_build_settings.sh"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

DRY_RUN=0
SOURCES_ONLY=0
PGO="${SLOPOS_TOOLCHAIN_PGO:-0}"
STAGE=""
XPY_ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --sources-only)
            SOURCES_ONLY=1
            shift
            ;;
        --pgo)
            PGO=1
            shift
            ;;
        --no-pgo)
            PGO=0
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
case "$PGO" in
    0 | 1) ;;
    *) die "SLOPOS_TOOLCHAIN_PGO must be 0 or 1, not $PGO" ;;
esac

TARGET="x86_64-unknown-slopos"
HOST_TRIPLE="x86_64-unknown-linux-gnu"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
SRC="$REPO_ROOT/$TP_RUSTC_SRC_REL"
OUT="${SLOPOS_TOOLCHAIN_OUT:-$BUILD_DIR/slopos-toolchain}"
SYSROOT="${SLOPOS_SYSROOT:-$BUILD_DIR/slopos-sysroot}"
# Physical: bootstrap installs the SlopOS libLLVM by mapping the host
# `llvm-config --libfiles` paths, which are physical, out of this directory,
# and through a symlinked build directory the mapping misses and ships the
# Linux one.
RUSTC_BUILD="$(mkdir -p "$BUILD_DIR/slopos-rustc-build" && cd -P "$BUILD_DIR/slopos-rustc-build" && pwd)"
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
# first, because the patches do not apply twice, and then carries the result
# over the previous tree by content, so the build directory recompiles only
# the files a patch edit changed.
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
        ts_set_aside "$SRC/src/llvm-project"
        echo "$SELF: extracting rustc's llvm-project (about 1.4 GB on disk)..." >&2
        tar -xf "$TARBALL" -C "$SRC" --strip-components=1 'rustc-nightly-src/src/llvm-project' ||
            die "failed to unpack src/llvm-project"
        LLVM_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_LLVM_RUSTC_OVERLAY_REL/")" ||
            die "the llvm port did not apply to $TP_RUSTC_SRC_REL/$TP_LLVM_RUSTC_TREE_REL"
        [ "$LLVM_PATCHES" != "0" ] ||
            die "no patches under $TP_LLVM_RUSTC_OVERLAY_REL/ — an unported LLVM has no SlopOS triple"
        printf '%s\n' "$LLVM_STAMP_WANT" >"$LLVM_STAMP"
        ts_carry_over "$SRC/src/llvm-project" || die "could not carry src/llvm-project over the previous tree"
        # Bootstrap keys its LLVM stamp on the llvm-project commit, which a
        # tarball does not carry, so it would never rebuild a changed port;
        # its lld stamp is on existence alone.
        rm -f "$RUSTC_BUILD"/*/llvm/.llvm-stamp "$RUSTC_BUILD"/*/lld/.lld-stamp
    fi

    if [ "$(cat "$LIBRARY_STAMP" 2>/dev/null)" != "$STD_STAMP_WANT" ]; then
        ts_set_aside "$SRC/library"
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
        ts_carry_over "$SRC/library" || die "could not carry library/ over the previous tree"
    fi
fi
if [ "$SOURCES_ONLY" -eq 1 ]; then
    [ "$DRY_RUN" -eq 0 ] || die "--sources-only stages sources, which a dry run never does"
    exit 0
fi

# ---------------------------------------------------------------------------
# The target sysroot and the compiler wrapper bootstrap hands to CMake and to
# every `-sys` build script, from `scripts/make_slopos_cross.sh`.
# ---------------------------------------------------------------------------
WRAPPER_DIR="$OUT/bin"
mkdir -p "$OUT/find-root"
BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_slopos_cross.sh" "$SYSROOT" "$WRAPPER_DIR" ||
    die "could not assemble the target sysroot and compiler wrapper"
CXX_ABI_FLAGS="$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"

# ---------------------------------------------------------------------------
# The recipes; a dry run compiles nothing and only names their prefix.
# `curl-sys` asks `curl-config --features` whether the libcurl pkg-config
# found has HTTP2, and `curl-config` is a PATH lookup; the one here answers
# for the SlopOS target and hands any other build script the next one on
# PATH, so the build triple's cargo never reads the recipe's.
# ---------------------------------------------------------------------------
RECIPES_DIR="${SLOPOS_RECIPES_DIR:-$BUILD_DIR/slopos-recipes}"
RECIPES_PREFIX="$RECIPES_DIR/prefix"
if [ "$DRY_RUN" -eq 0 ]; then
    command -v pkg-config >/dev/null 2>&1 ||
        die "pkg-config is required: the -sys crates find the recipes through it, and build their bundled copies without it"
    BUILD_DIR="$BUILD_DIR" SLOPOS_RECIPES_DIR="$RECIPES_DIR" "$SCRIPT_DIR/build_recipes.sh" ||
        die "the recipes did not build"
    cp -a "$RECIPES_PREFIX"/lib/lib*.so* "$SYSROOT/lib/"
    cp -a "$RECIPES_PREFIX/include/." "$SYSROOT/include/"
fi
CURL_CONFIG_DIR="$OUT/curl-config"
mkdir -p "$CURL_CONFIG_DIR"
cat >"$CURL_CONFIG_DIR/curl-config" <<CURL_CONFIG
#!/bin/sh
# Generated by scripts/$SELF.sh — do not edit.
[ "\${TARGET:-}" = "$TARGET" ] && exec "$RECIPES_PREFIX/bin/curl-config" "\$@"
IFS=:
for dir in \$PATH; do
    [ "\$dir" = "$CURL_CONFIG_DIR" ] || [ ! -x "\$dir/curl-config" ] || exec "\$dir/curl-config" "\$@"
done
exit 127
CURL_CONFIG
chmod +x "$CURL_CONFIG_DIR/curl-config"
TARGET_ENV="$(printf '%s' "$TARGET" | tr 'a-z-' 'A-Z_')"
TARGET_SUFFIX="${TARGET//-/_}"

# ---------------------------------------------------------------------------
# With `--pgo`: the PGO profiles, gathered by
# `scripts/make_toolchain_profile.sh`, which returns at once while their
# stamp describes the tree (a dry run names them without making them:
# bootstrap only reads them once it compiles), the release settings, and the
# build triple compiled by the host's clang and archived by LLVM's tools,
# because LLVM's ThinLTO needs both. Without it the configuration is the plain
# one this script always wrote.
# ---------------------------------------------------------------------------
PGO_DIR="$BUILD_DIR/slopos-pgo"
RUSTC_PROFILE="$PGO_DIR/rustc.profdata"
LLVM_PROFILE="$PGO_DIR/llvm.profdata"
PLAIN=plain
PGO_CONFIG=""
TARGET_AR="$LLVM_AR"
TARGET_RANLIB="$LLVM_AR"
if [ "$PGO" -eq 1 ]; then
    if [ "$DRY_RUN" -eq 0 ]; then
        BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_toolchain_profile.sh" ||
            die "no PGO profiles — see above, or build without --pgo"
    fi
    eval "$(rbs_llvm_archivers "$LLVM_AR")" || die "no llvm-ranlib beside $LLVM_AR"
    PLAIN=""
    TARGET_AR="$RBS_AR"
    TARGET_RANLIB="$RBS_RANLIB"
    PGO_CONFIG="[pgo.rustc]
use = \"$RUSTC_PROFILE\"
[pgo.llvm]
use = \"$LLVM_PROFILE\"

[target.$HOST_TRIPLE]
cc = \"$CLANG\"
cxx = \"$CLANGXX\"
ar = \"$RBS_AR\"
ranlib = \"$RBS_RANLIB\""
fi

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
$(rbs_build_settings)

[llvm]
clang = true
$(rbs_llvm_settings "$OUT/find-root" $PLAIN)

[rust]
$(rbs_rust_settings $PLAIN)

$PGO_CONFIG

[target.$TARGET]
cc = "$WRAPPER_DIR/$TARGET-clang"
cxx = "$WRAPPER_DIR/$TARGET-clang++"
ar = "$TARGET_AR"
ranlib = "$TARGET_RANLIB"
linker = "$WRAPPER_DIR/$TARGET-clang"
crt-static = false
# Link-time only, so what the compiler generates is untouched.
# \`-Bsymbolic-functions\`: a call from one of librustc_driver's crates to
# another went through the PLT to a symbol the loader resolved back into the
# same object — 10,363 of its 11,137 \`JUMP_SLOT\`s. Upstream gets the same
# from \`-Zdefault-visibility=protected\`, which bootstrap ties to linking with
# its own lld. \`-z pack-relative-relocs\`: relative relocations as \`DT_RELR\`
# bitmaps rather than 24-byte \`RELA\` entries, which slibc's loader applies.
rustflags = ["-Clink-arg=-Wl,-Bsymbolic-functions", "-Clink-arg=-Wl,-z,pack-relative-relocs"]

[install]
# Both, and both under the build directory: bootstrap asserts it can write
# sysconfdir as well as prefix, and that one defaults to an absolute /etc no
# ordinary user owns.
prefix = "$OUT/install.partial"
sysconfdir = "etc"
CONFIG_END

# With `--pgo`, every rustc bootstrap runs names its crate without the target
# triple (see the script), so a symbol here is spelled as in the profiled
# Linux build.
if [ "$PGO" -eq 1 ]; then
    export RUSTC_WRAPPER="$SCRIPT_DIR/rustc_neutral_metadata.sh"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "$SELF: dry run — $CONFIG"
    (cd "$SRC" && python3 x.py install --config "$CONFIG" --dry-run "${XPY_ARGS[@]}")
    exit 0
fi

# What bootstrap does not see change. Cargo does not fingerprint the
# wrapper, so a build directory whose crates were named otherwise is cleared
# of every Rust stage; bootstrap keys LLVM's stamp on a commit a tarball does
# not carry, so the SlopOS LLVM and lld are rebuilt when a setting, the
# profile or the host clang they were built with changes. The build triple's
# LLVM is a build tool whose code reaches nothing shipped, and is left alone.
# A plain build of a directory no `--pgo` build touched does nothing here.
if [ "$PGO" -eq 1 ]; then
    rbs_invalidate_rust "$RUSTC_BUILD" "$HOST_TRIPLE" "$RUSTC_WRAPPER"
    rbs_invalidate_llvm "$RUSTC_BUILD" "$TARGET" "$CLANG" "$(rbs_llvm_settings "$OUT/find-root")" \
        "$(sha256sum <"$LLVM_PROFILE")"
else
    rbs_invalidate_rust "$RUSTC_BUILD" "$HOST_TRIPLE" ""
    rbs_forget_llvm "$RUSTC_BUILD" "$TARGET"
fi

# Nor does cargo see the recipes change: a `-sys` build script keeps the
# answer pkg-config gave it, and pkg-config names no file for cargo to watch,
# so a rebuilt prefix would ship beside a cargo linked against the last one.
# The SlopOS tools are cleared whenever the recipes' stamps are not the ones
# they were built against.
RECIPES_STAMP="$(BUILD_DIR="$BUILD_DIR" SLOPOS_RECIPES_DIR="$RECIPES_DIR" \
    "$SCRIPT_DIR/build_recipes.sh" --print-stamp | sha256sum)"
if [ "$(cat "$RUSTC_BUILD/.slopos-recipes" 2>/dev/null)" != "$RECIPES_STAMP" ]; then
    if compgen -G "$RUSTC_BUILD/*/stage[1-9]-tools/$TARGET" >/dev/null; then
        echo "$SELF: the recipes changed since cargo was built; clearing the $TARGET tools" >&2
    fi
    rm -rf "$RUSTC_BUILD"/*/stage[1-9]-tools/"$TARGET"
    printf '%s\n' "$RECIPES_STAMP" >"$RUSTC_BUILD/.slopos-recipes"
fi

# Completed under another name and renamed last: a build that stops part way
# must not leave a prefix the dev disk would take for a toolchain.
PREFIX="$OUT/install.partial"
rm -rf "$PREFIX"
# `DT_RELR` for LLVM's objects too. bootstrap reads `LDFLAGS_<triple>` for
# the CMake builds of that triple alone, where `llvm.ldflags` would reach the
# build triple's LLVM and put a `GLIBC_ABI_DT_RELR` requirement on it.
#
# The recipes, for the SlopOS target's build scripts: the pkg-config crate
# and openssl-sys read target-suffixed variables, so the build triple keeps
# the host's libraries. `LIBGIT2_NO_VENDOR` and `LIBSSH2_SYS_USE_PKG_CONFIG`
# have no such form and are set for the whole build; only SlopOS's cargo
# builds libgit2-sys or libssh2-sys, and `LIBGIT2_NO_VENDOR` makes a libgit2
# pkg-config cannot find a failed build rather than a bundled copy.
(cd "$SRC" && env "LDFLAGS_${TARGET_SUFFIX}=-Wl,-z,pack-relative-relocs" \
    "PKG_CONFIG_ALLOW_CROSS_${TARGET_SUFFIX}=1" \
    "PKG_CONFIG_LIBDIR_${TARGET_SUFFIX}=$RECIPES_PREFIX/lib/pkgconfig" \
    "PKG_CONFIG_PATH_${TARGET_SUFFIX}=" \
    "${TARGET_ENV}_OPENSSL_DIR=$RECIPES_PREFIX" \
    LIBGIT2_NO_VENDOR=1 LIBSSH2_SYS_USE_PKG_CONFIG=1 \
    PATH="$CURL_CONFIG_DIR:$PATH" \
    python3 x.py install --config "$CONFIG" --jobs "$JOBS" "${XPY_ARGS[@]}")

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
