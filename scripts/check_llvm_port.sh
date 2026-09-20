#!/usr/bin/env bash
set -euo pipefail

# Hold the SlopOS LLVM port to compiling LLVM's own portability surface.
#
# Usage: check_llvm_port.sh [--require] [--self-test]
#
# The subject is `LLVMSupport`, and it is the whole of what a host port has to
# answer for: `Unix/Path.inc`, `Unix/Process.inc`, `Unix/Program.inc` and
# `Unix/Signals.inc` are the files that name a libc, `raw_ostream.cpp` and
# `ConvertUTF.cpp` are the ones that name a C++ standard library, and the rest
# of LLVM is portable C++ over them. A run that compiles those compiles a
# toolchain's OS-facing half; the ~1300 remaining libraries are CPU time, not
# information.
#
# Two things can break it and neither fails to compile on the host:
#
#   * `toolchain/llvm/*.patch` stops describing the tree — the two places
#     LLVM dispatches on the OS, `<endian.h>` and `statvfs`, fall back to a
#     BSD spelling SlopOS has not got.
#   * slibc loses an entry point or a header. The libc surface this needs is
#     wider than the appliance's: `<inttypes.h>`, `<endian.h>`, `<sysexits.h>`
#     and `<wctype.h>` exist because these files include them.
#
# `skipped` without a materialised source tree or a staged C++ runtime, since
# a checkout that has cross-built neither still has a consistent pin; the CI
# step that has both passes `--require`. `--self-test` grades the skip and
# `--require` paths on every host and the rejection wherever there is a tree
# to plant one in, so a checkout without one still exercises what it can.

SELF="check_llvm_port"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

REQUIRE=0
SELF_TEST=0
for arg in "$@"; do
    case "$arg" in
    --require) REQUIRE=1 ;;
    --self-test) SELF_TEST=1 ;;
    *) die "unknown argument: $arg" ;;
    esac
done

skip() {
    [ "$REQUIRE" -eq 0 ] || die "$1"
    echo "$SELF: skipped — $1"
    exit 0
}

PIN="$REPO_ROOT/toolchain/cxx/PIN"
LLVM_VERSION="$(sed -n 's/^llvm_version=\(.*\)$/\1/p' "$PIN" | head -n 1)"
SOURCE="${LLVM_SRC_DIR:-$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src}"
CXX_DIR="$REPO_ROOT/third_party/slopos-cxx"
BUILD="${BUILD_DIR:-$REPO_ROOT/builddir}/gates/llvm-port"
TARGET="x86_64-unknown-slopos"
# `LLVMSupport` is the OS-facing half; `LLVMTargetParser` is where
# `Triple::SlopOS` lives, and without it four of the port's six files compile
# nowhere. Clang's two are still held by nothing but `git apply --check`:
# reaching `SlopOSTargetInfo` means building clang, which is a different
# order of cost from this gate's minute.
TARGETS="LLVMSupport LLVMTargetParser"

CLANGXX=""
CXXFLAGS=()
SKIP_REASON=""

# What a run needs that a checkout may legitimately not have. `llvm-tblgen` is
# in here rather than in `prepare` so the self-test can tell the difference
# between a case it declined to grade and one it silently exited out of.
inputs_ready() {
    local tool candidate found
    if [ ! -d "$SOURCE/llvm/lib/Support" ]; then
        SKIP_REASON="no llvm sources — run scripts/make_slopos_llvm_src.sh"
        return 1
    fi
    if [ ! -d "$CXX_DIR/include/c++/v1" ]; then
        SKIP_REASON="no C++ runtime — run a tests userland build"
        return 1
    fi

    local tools
    tools="$("$SCRIPT_DIR/cxx_host_tools.sh")" ||
        die "no host LLVM toolchain — see scripts/cxx_host_tools.sh"
    eval "$tools"
    for tool in cmake ninja; do
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
    done

    # LLVM's own build reaches for a `llvm-tblgen` that runs on the host; this
    # builds none, so one is borrowed. The generated `.inc` files are data
    # tables rather than code, which is why a host tool of the same major
    # serves — and why the major is checked rather than read off the name.
    if [ -z "${LLVM_TBLGEN:-}" ]; then
        for candidate in "$(dirname "$CLANG")/llvm-tblgen" \
            "llvm-tblgen-$CXX_HOST_MAJOR" llvm-tblgen; do
            found="$(command -v "$candidate" 2>/dev/null || true)"
            [ -n "$found" ] || continue
            case "$("$found" --version 2>/dev/null |
                sed -n 's/.*LLVM version \([0-9]*\).*/\1/p' | head -n 1)" in
            "$CXX_HOST_MAJOR")
                LLVM_TBLGEN="$found"
                break
                ;;
            esac
        done
    fi
    if [ -z "${LLVM_TBLGEN:-}" ] || [ ! -x "$LLVM_TBLGEN" ]; then
        SKIP_REASON="no llvm-tblgen for LLVM $CXX_HOST_MAJOR on this host"
        return 1
    fi
    return 0
}

# Both trees fresh, the flags built and cmake run. Split out because the
# self-test's probe compiles one file out of the same configuration the gate
# builds `LLVMSupport` from.
prepare() {
    local want_src want_cxx abi_flags common
    want_src="$("$SCRIPT_DIR/make_slopos_llvm_src.sh" --print-stamp)"
    [ -n "$want_src" ] || die "make_slopos_llvm_src.sh --print-stamp printed nothing"
    [ "$(cat "$SOURCE/.slopos-llvm-stamp" 2>/dev/null)" = "$want_src" ] ||
        die "the llvm source tree is stale — run scripts/make_slopos_llvm_src.sh"

    # The runtime too: a stale `third_party/slopos-cxx` compiles, it just
    # compiles against headers the sources no longer produce, and this gate
    # would report green for a C++ library nothing else in the tree accepts.
    want_cxx="$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-stamp "$REPO_ROOT/builddir")"
    [ -n "$want_cxx" ] || die "make_slopos_cxx.sh --print-stamp printed nothing"
    [ "$(cat "$CXX_DIR/.slopos-stamp" 2>/dev/null)" = "$want_cxx" ] ||
        die "the C++ runtime is stale — rebuild the tests userland"

    read -ra abi_flags <<<"$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"
    [ "${#abi_flags[@]}" -gt 0 ] ||
        die "make_slopos_cxx.sh --print-abi-flags printed nothing"
    # `__slopos__` on the command line rather than from the compiler: the port
    # teaches a *cross-built* clang to predefine it, and the host clang that
    # runs this build is not that clang. The first toolchain built from this
    # tree is what retires the flag.
    common=(--target="$TARGET" -nostdlibinc -isystem "$REPO_ROOT/slibc/include"
        -fPIC "${abi_flags[@]}" -D__slopos__)
    CXXFLAGS=(--target="$TARGET" -nostdlibinc -nostdinc++
        -isystem "$CXX_DIR/include/c++/v1" -isystem "$REPO_ROOT/slibc/include"
        -fPIC "${abi_flags[@]}" -D__slopos__)

    # cmake refuses an existing binary directory whose source directory moved,
    # so the version is the key rather than something to diagnose later.
    if [ "$(cat "$BUILD/.slopos-source" 2>/dev/null)" != "$SOURCE" ]; then
        rm -rf "$BUILD"
    fi
    mkdir -p "$BUILD"
    printf '%s\n' "$SOURCE" >"$BUILD/.slopos-source"

    cmake -G Ninja -S "$SOURCE/llvm" -B "$BUILD" -Wno-dev \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
        -DCMAKE_C_COMPILER="$CLANG" -DCMAKE_CXX_COMPILER="$CLANGXX" \
        -DCMAKE_ASM_COMPILER="$CLANG" \
        -DCMAKE_C_FLAGS="${common[*]}" -DCMAKE_CXX_FLAGS="${CXXFLAGS[*]}" \
        -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
        -DLLVM_TABLEGEN="$LLVM_TBLGEN" \
        -DLLVM_TARGETS_TO_BUILD=X86 \
        -DLLVM_HOST_TRIPLE="$TARGET" -DLLVM_DEFAULT_TARGET_TRIPLE="$TARGET" \
        -DLLVM_ENABLE_LIBCXX=ON -DLLVM_ENABLE_PIC=ON -DLLVM_ENABLE_THREADS=ON \
        -DLLVM_ENABLE_ZLIB=OFF -DLLVM_ENABLE_ZSTD=OFF -DLLVM_ENABLE_TERMINFO=OFF \
        -DLLVM_ENABLE_LIBXML2=OFF -DLLVM_ENABLE_LIBEDIT=OFF -DLLVM_ENABLE_LIBPFM=OFF \
        -DLLVM_INCLUDE_TESTS=OFF -DLLVM_INCLUDE_BENCHMARKS=OFF \
        -DLLVM_INCLUDE_EXAMPLES=OFF -DLLVM_INCLUDE_UTILS=OFF \
        -DLLVM_BUILD_TOOLS=OFF -DLLVM_INCLUDE_DOCS=OFF \
        -DLLVM_ENABLE_BACKTRACES=OFF -DLLVM_ENABLE_CRASH_OVERRIDES=OFF \
        >"$BUILD/configure.log" 2>&1 || {
        tail -n 30 "$BUILD/configure.log" >&2
        die "cmake configure failed; see $BUILD/configure.log"
    }
}

run_gate() {
    inputs_ready || skip "$SKIP_REASON"
    prepare
    ninja -C "$BUILD" $TARGETS >"$BUILD/build.log" 2>&1 || {
        grep -h 'error:' "$BUILD/build.log" | sed 's/.*error: /  /' |
            sort -u | head -n 20 >&2
        die "$TARGETS does not compile for $TARGET; see $BUILD/build.log"
    }
    echo "$SELF: $TARGETS compile for $TARGET against slibc and libc++"
}

self_test() {
    local scratch failed=0 probe
    # Not under `$BUILD`: `prepare` deletes that whole tree when cmake's
    # recorded source directory does not match, which on a cold build dir is
    # every time — and a probe whose output directory has gone fails to
    # compile, which reads as the rejection this is supposed to observe.
    scratch="$(mktemp -d)"
    trap 'rm -rf "$scratch"' EXIT INT TERM

    if LLVM_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" >/dev/null 2>&1; then
        echo "  case no-source-tree: skipped rather than failed"
    else
        echo "$SELF --self-test: a checkout with no source tree was failed" >&2
        failed=1
    fi

    if LLVM_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" --require \
        >/dev/null 2>&1; then
        echo "$SELF --self-test: --require passed with no source tree" >&2
        failed=1
    else
        echo "  case require-no-source-tree: failed rather than skipped"
    fi

    if inputs_ready; then
        prepare
        probe="$scratch/probe.o"
        # `-U__slopos__` is the port's own effect removed: `<endian.h>` becomes
        # `<machine/endian.h>` and `statvfs.f_flag` becomes `f_flags`.
        compile() {
            "$CLANGXX" "${CXXFLAGS[@]}" "$@" -std=c++17 -fno-exceptions -fno-rtti \
                -I "$BUILD/include" -I "$SOURCE/llvm/include" \
                -I "$SOURCE/llvm/lib/Support" \
                -c "$SOURCE/llvm/lib/Support/Path.cpp" -o "$probe" >/dev/null 2>&1
        }
        # The positive control first: without it a probe that fails for an
        # unrelated reason reads as a working check.
        if compile; then
            echo "  case ported-tree: Path.cpp compiles with the port applied"
        else
            echo "$SELF --self-test: Path.cpp does not compile with the port applied" >&2
            failed=1
        fi
        if compile -U__slopos__; then
            echo "$SELF --self-test: Path.cpp compiled with the port's macro removed" >&2
            failed=1
        else
            echo "  case unported-tree: rejected Path.cpp without __slopos__"
        fi

        # The Triple half, which `-U__slopos__` says nothing about: the patch
        # appends the enumerator and moves `LastOSType` onto it, so before it
        # the last one was `Vulkan`. Asserting each in turn plants the removal
        # of that hunk exactly.
        triple() {
            cat >"$scratch/triple.cpp" <<EOF
#include "llvm/TargetParser/Triple.h"
using llvm::Triple;
static_assert(Triple::$1 == Triple::LastOSType, "last OS");
EOF
            "$CLANGXX" "${CXXFLAGS[@]}" -std=c++17 -fno-exceptions -fno-rtti \
                -I "$BUILD/include" -I "$SOURCE/llvm/include" \
                -c "$scratch/triple.cpp" -o "$probe" >/dev/null 2>&1
        }
        if triple SlopOS; then
            echo "  case ported-triple: SlopOS is the last OS the Triple names"
        else
            echo "$SELF --self-test: the patched Triple does not end at SlopOS" >&2
            failed=1
        fi
        if triple Vulkan; then
            echo "$SELF --self-test: the Triple still ends where it did unported" >&2
            failed=1
        else
            echo "  case unported-triple: rejected the pre-port last OS"
        fi
    else
        echo "  cases ported-tree, unported-tree, ported-triple, unported-triple: skipped — $SKIP_REASON"
    fi

    rm -rf "$scratch"
    trap - EXIT INT TERM
    if [ "$failed" -ne 0 ]; then
        echo "$SELF: SELF-TEST FAILED — the gate does not catch what it claims to" >&2
        return 1
    fi
    echo "$SELF: self-test OK"
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
else
    run_gate
fi
