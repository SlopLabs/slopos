#!/usr/bin/env bash
set -euo pipefail

# Gather the PGO profiles the SlopOS-hosted compiler is optimised with.
#
# Usage: make_toolchain_profile.sh [--force] [--optimized-host]
#
# What rust-lang/rust's opt-dist does for a release, with this repository's
# kernel build as the workload. The profiles come from a Linux-hosted build of
# `third_party/slopos-rustc-src` — the same sources, and the same settings
# (`scripts/lib/rustc_build_settings.sh`), as the compiler
# `scripts/bootstrap_slopos_toolchain.sh` cross-builds for SlopOS — in its own
# build directory, in three steps:
#
#   1. LLVM built with IR instrumentation (`pgo.llvm.generate`), and a stage1
#      compiler over it. The workload runs on that compiler, and the host's
#      `llvm-profdata` merges what libLLVM wrote into `llvm.profdata`: the
#      instrumented objects were compiled by the host's clang, and the
#      profile is read back by the host's clang, compiling the SlopOS LLVM.
#   2. LLVM rebuilt with that profile, and a stage2 compiler built with
#      `-Cprofile-generate` over it (`pgo.rustc.generate`). The workload runs
#      again, and the build's own `llvm-profdata` — rustc's LLVM, which wrote
#      the raw profiles and reads the merged one — makes `rustc.profdata`.
#   3. `--optimized-host` only: the stage2 compiler rebuilt with both
#      profiles, which is the Linux-hosted twin of the SlopOS one — what a
#      host kernel build is timed with to see what the profiles buy. Its
#      sysroot is printed last.
#
# The workload is the guest's own (`selfhost_test`): `scripts/build_kernel.sh`
# for the dev and then the tests kernel into one empty target directory, with
# the stage's own cargo (the stage0 one x.py downloads) and the vendored
# sources. A stage sysroot's `lib/rustlib/src/rust` is the patched source
# tree, which is what `-Zbuild-std` reads.
#
# A profile keys each function by its symbol and a hash of its control flow.
# The symbols match only because both builds compile every crate through
# `scripts/rustc_neutral_metadata.sh`; LLVM's C++ symbols match by
# themselves, but not where a signature or an inlined body is the C++
# library's, which is libstdc++ here and libc++ on SlopOS.
#
# The profiles land in `<build dir>/slopos-pgo`, each with a stamp over what
# it depends on: the compiler tree's three source stamps, the host clang, the
# shared settings, `PROFILE_FLOW` below, and for rustc's also the wrapper and
# LLVM's profile. A run whose stamps match is a no-op. The kernel sources are
# not an input: a profile a few kernel commits old describes the same
# compiler work, and regenerating it for every commit would be hours each
# time. `--force` regenerates regardless.
#
# Environment:
#   BUILD_DIR        - where artifacts go (default: builddir)
#   BOOTSTRAP_JOBS   - -j for x.py (default: nproc)

SELF="make_toolchain_profile"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"
. "$SCRIPT_DIR/lib/rustc_build_settings.sh"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

FORCE=0
OPTIMIZED_HOST=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --optimized-host) OPTIMIZED_HOST=1 ;;
        *) die "unknown argument: $arg" ;;
    esac
done

HOST_TRIPLE="x86_64-unknown-linux-gnu"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
SRC="$REPO_ROOT/$TP_RUSTC_SRC_REL"
OUT="$BUILD_DIR/slopos-pgo"
PGO_BUILD="$BUILD_DIR/slopos-pgo-build"
JOBS="${BOOTSTRAP_JOBS:-$(nproc)}"
LLVM_PROFILE="$OUT/llvm.profdata"
RUSTC_PROFILE="$OUT/rustc.profdata"
WRAPPER="$SCRIPT_DIR/rustc_neutral_metadata.sh"

command -v python3 >/dev/null 2>&1 || die "python3 is required to run x.py"
# The source subtrees a real build needs, staged as the SlopOS build stages
# them: one tree serves both.
BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/bootstrap_slopos_toolchain.sh" --sources-only

CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"
eval "$(rbs_llvm_archivers "$LLVM_AR")" || die "no llvm-ranlib beside $LLVM_AR"
# The host's profile merger must be the host clang's LLVM: it reads the raw
# format that clang's runtime wrote and writes an indexed one that clang
# reads back.
HOST_PROFDATA="${LLVM_AR/llvm-ar/llvm-profdata}"
command -v "$HOST_PROFDATA" >/dev/null 2>&1 || die "no $HOST_PROFDATA beside $LLVM_AR"
profdata_major="$("$HOST_PROFDATA" --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sed -n 1p)"
[ "${profdata_major%%.*}" = "$CXX_HOST_MAJOR" ] ||
    die "$HOST_PROFDATA is LLVM $profdata_major, and $CLANG is $CXX_HOST_MAJOR"

# What this script does to make a profile — its steps, its build settings
# beyond the shared ones, its workload. Bump it when any of them changes;
# an edit that changes none of them does not cost a regeneration.
PROFILE_FLOW=1
LLVM_PORT_STAMP="$(cat "$SRC/src/llvm-project/.slopos-port-stamp")"
LLVM_INPUTS="$({
    echo "flow $PROFILE_FLOW"
    tp_rustc_stamp "$REPO_ROOT"
    printf '%s\n' "$LLVM_PORT_STAMP"
    cat "$SRC/library/.slopos-std-stamp"
    "$CLANG" --version
    rbs_build_settings
    rbs_llvm_settings "$OUT/find-root"
    rbs_rust_settings
} | sha256sum)"
LLVM_INPUTS="${LLVM_INPUTS%% *}"

# rustc's inputs, given LLVM's profile: that decides the LLVM the rustc
# profile was gathered over, and the wrapper decides the names it records.
rustc_inputs() {
    printf '%s %s %s\n' "$LLVM_INPUTS" "$(sha256sum <"$WRAPPER")" "$(sha256sum <"$LLVM_PROFILE")"
}

fresh() {
    [ -f "$1" ] && [ "$(cat "$1.inputs" 2>/dev/null)" = "$2" ]
}

if [ "$FORCE" -eq 0 ] && [ "$OPTIMIZED_HOST" -eq 0 ] && fresh "$LLVM_PROFILE" "$LLVM_INPUTS" &&
    fresh "$RUSTC_PROFILE" "$(rustc_inputs)"; then
    echo "$SELF: the profiles in $OUT describe this tree"
    exit 0
fi
if [ "$FORCE" -eq 1 ]; then
    rm -f "$LLVM_PROFILE.inputs" "$RUSTC_PROFILE.inputs"
fi

mkdir -p "$OUT/find-root" "$OUT/bin"
CONFIG="$OUT/bootstrap.toml"
SETTINGS="$(rbs_llvm_settings "$OUT/find-root")"

# The C and C++ compiler of the build triple: the host's clang, minus the
# rustc profile flags. cc-rs hands a clang every `-Cprofile-generate` and
# `-Cprofile-use` in RUSTFLAGS, so the C and C++ that ends up inside
# librustc_driver — `llvm-wrapper` and a few crates' — would be instrumented
# by the host's LLVM into the same profile sections as rustc's LLVM
# instruments Rust, and the two write different records: the stage2
# compiler died at exit writing its profile (measured). LLVM's own
# instrumentation and profile arrive as other flags, or name other files, and
# pass.
for pair in "cc $CLANG" "c++ $CLANGXX"; do
    cat >"$OUT/bin/${pair%% *}" <<SHIM
#!/bin/sh
# Generated by scripts/$SELF.sh — do not edit.
for arg in "\$@"; do
    shift
    case "\$arg" in
        -fprofile-generate=$OUT/rustc-raw | -fprofile-use=$RUSTC_PROFILE) ;;
        *) set -- "\$@" "\$arg" ;;
    esac
done
exec ${pair#* } "\$@"
SHIM
    chmod +x "$OUT/bin/${pair%% *}"
done

# `$1` is the [pgo] tables of the step. Everything else is the same in all
# three, so a step rebuilds what its PGO settings change and nothing more.
write_config() {
    cat >"$CONFIG" <<CONFIG_END
# Generated by scripts/$SELF.sh — do not edit.
change-id = "ignore"
[build]
build = "$HOST_TRIPLE"
build-dir = "$PGO_BUILD"
host = ["$HOST_TRIPLE"]
target = ["$HOST_TRIPLE"]
$(rbs_build_settings)

[llvm]
# Only libLLVM is profiled: nothing in the workload runs clang.
clang = false
$SETTINGS

[rust]
$(rbs_rust_settings)

$1

[target.$HOST_TRIPLE]
cc = "$OUT/bin/cc"
cxx = "$OUT/bin/c++"
ar = "$RBS_AR"
ranlib = "$RBS_RANLIB"
# \`profiler_builtins\`, which \`-Cprofile-generate\` links.
profiler = true
# No self-contained rust-lld as the compilers' default linker, as the SlopOS
# build's Linux compiler has none (the default applies only to a Linux
# host): this LLVM's lld lacks the zlib std's debug-section compression asks
# for, and the switch is compiled into rustc. Bootstrap's own links go
# through the host's clang and lld instead, because under ThinLTO it compiles
# \`llvm-wrapper\` to bitcode, which binutils' \`ld\` cannot read; the SlopOS
# build's wrapper links with lld already.
default-linker-linux-override = "off"
linker = "$CLANG"
rustflags = ["-Clink-arg=-fuse-ld=lld"]
CONFIG_END
}

xpy() {
    (cd "$SRC" && RUSTC_WRAPPER="$WRAPPER" python3 x.py "$@" --config "$CONFIG" --jobs "$JOBS")
}

# The guest's build, on the compiler in sysroot `$1`; both kernels' output
# in `<out>/workload.log`.
workload() {
    local sysroot="$1" work="$OUT/workload" features
    rm -rf "$work"
    mkdir -p "$work/cargo-home"
    : >"$work.log"
    sed "s|^directory = \"|directory = \"$REPO_ROOT/|" "$REPO_ROOT/.cargo/vendor.toml" >"$work/cargo-home/config.toml"
    for features in "" "slopos-testing/qemu-exit kernel/tests"; do
        (cd "$REPO_ROOT" && env -u KERNEL_RELEASE -u KERNEL_SAFESTACK -u KERNEL_RUSTFLAGS \
            -u RUSTC_WRAPPER -u RUSTFLAGS -u LLVM_PROFILE_FILE \
            CARGO="$STAGE/stage0/bin/cargo" RUSTC_BOOTSTRAP=1 CARGO_HOME="$work/cargo-home" RUSTC="$sysroot/bin/rustc" \
            RUST_TARGET=targets/x86_64-slos.json \
            scripts/build_kernel.sh "$work" "$work/target" "$features") >>"$work.log" 2>&1 || {
            tail -n 30 "$work.log" >&2
            die "the workload failed on $sysroot/bin/rustc — see $work.log"
        }
    done
}

# Merge the raw profiles in `$2` with `$1` into `$3`, and report their size.
merge() {
    local raw
    raw="$(find "$2" -name '*.profraw' | wc -l)"
    [ "$raw" -gt 0 ] || die "the workload wrote no profiles into $2"
    "$1" merge -o "$3.tmp" "$2" || die "$1 could not merge $2"
    mv -f "$3.tmp" "$3"
    echo "$SELF: $3 from $raw raw profiles: $("$1" show "$3" | sed -n 's/^Total functions: //p') functions" >&2
}

rbs_invalidate_rust "$PGO_BUILD" "$HOST_TRIPLE" "$WRAPPER"
STAGE="$PGO_BUILD/$HOST_TRIPLE"

# ---------------------------------------------------------------------------
# 1. LLVM's profile, from an instrumented libLLVM under a stage1 compiler.
# ---------------------------------------------------------------------------
if ! fresh "$LLVM_PROFILE" "$LLVM_INPUTS"; then
    rbs_invalidate_llvm "$PGO_BUILD" "$HOST_TRIPLE" "$CLANG" "$SETTINGS" "$LLVM_PORT_STAMP" generate
    write_config "[pgo.llvm]
generate = \"$OUT/llvm-raw\""
    xpy build --stage 1 library
    # What the build itself ran — tablegen, the std compile — is not the
    # workload.
    rm -rf "$OUT/llvm-raw"
    workload "$STAGE/stage1"
    merge "$HOST_PROFDATA" "$OUT/llvm-raw" "$LLVM_PROFILE"
    rm -rf "$OUT/llvm-raw"
    printf '%s\n' "$LLVM_INPUTS" >"$LLVM_PROFILE.inputs"
fi

LLVM_PROFILE_SUM="$(sha256sum <"$LLVM_PROFILE")"
RUSTC_INPUTS="$(rustc_inputs)"
USE_LLVM="[pgo.llvm]
use = \"$LLVM_PROFILE\""

# ---------------------------------------------------------------------------
# 2. rustc's profile, from an instrumented stage2 compiler over the optimised
#    LLVM.
# ---------------------------------------------------------------------------
if ! fresh "$RUSTC_PROFILE" "$RUSTC_INPUTS"; then
    rbs_invalidate_llvm "$PGO_BUILD" "$HOST_TRIPLE" "$CLANG" "$SETTINGS" "$LLVM_PORT_STAMP" "use $LLVM_PROFILE_SUM"
    write_config "$USE_LLVM
[pgo.rustc]
generate = \"$OUT/rustc-raw\""
    xpy build --stage 2 library
    rm -rf "$OUT/rustc-raw"
    workload "$STAGE/stage2"
    merge "$STAGE/llvm/bin/llvm-profdata" "$OUT/rustc-raw" "$RUSTC_PROFILE"
    rm -rf "$OUT/rustc-raw"
    printf '%s\n' "$RUSTC_INPUTS" >"$RUSTC_PROFILE.inputs"
fi

# ---------------------------------------------------------------------------
# 3. The Linux-hosted compiler built as the SlopOS one is.
# ---------------------------------------------------------------------------
if [ "$OPTIMIZED_HOST" -eq 1 ]; then
    rbs_invalidate_llvm "$PGO_BUILD" "$HOST_TRIPLE" "$CLANG" "$SETTINGS" "$LLVM_PORT_STAMP" "use $LLVM_PROFILE_SUM"
    write_config "$USE_LLVM
[pgo.rustc]
use = \"$RUSTC_PROFILE\""
    xpy build --stage 2 library
    echo "$SELF: optimized host compiler: $STAGE/stage2"
fi

echo "$SELF: $LLVM_PROFILE and $RUSTC_PROFILE"
