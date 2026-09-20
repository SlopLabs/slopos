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
# job that materialises both passes `--require`.

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
    if [ "$REQUIRE" -eq 1 ]; then
        die "$1"
    fi
    echo "$SELF: skipped — $1"
    exit 0
}

PIN="$REPO_ROOT/toolchain/cxx/PIN"
LLVM_VERSION="$(sed -n 's/^llvm_version=\(.*\)$/\1/p' "$PIN" | head -n 1)"
SOURCE="$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src"
CXX_DIR="$REPO_ROOT/third_party/slopos-cxx"
BUILD="${BUILD_DIR:-$REPO_ROOT/builddir}/gates/llvm-port"
TARGET="x86_64-unknown-slopos"

[ -d "$SOURCE/llvm/lib/Support" ] ||
    skip "no llvm sources — run scripts/make_slopos_llvm_src.sh"
[ -d "$CXX_DIR/include/c++/v1" ] ||
    skip "no C++ runtime — run a tests userland build"
[ "$(cat "$SOURCE/.slopos-llvm-stamp" 2>/dev/null)" = \
    "$("$SCRIPT_DIR/make_slopos_llvm_src.sh" --print-stamp)" ] ||
    die "the llvm source tree is stale — run scripts/make_slopos_llvm_src.sh"

CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"
for tool in cmake ninja; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

ABI_FLAGS="$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"
# `__slopos__` on the command line rather than from the compiler: the port
# teaches a *cross-built* clang to predefine it, and the host clang that runs
# this build is not that clang. The first toolchain built from this tree is
# what retires the flag.
COMMON="--target=$TARGET -nostdlibinc -isystem $REPO_ROOT/slibc/include -fPIC"
COMMON="$COMMON $ABI_FLAGS -D__slopos__"
CXXFLAGS="--target=$TARGET -nostdlibinc -nostdinc++ -isystem $CXX_DIR/include/c++/v1"
CXXFLAGS="$CXXFLAGS -isystem $REPO_ROOT/slibc/include -fPIC $ABI_FLAGS -D__slopos__"

configure() {
    cmake -G Ninja -S "$SOURCE/llvm" -B "$BUILD" -Wno-dev \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_SYSTEM_NAME=Linux -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
        -DCMAKE_C_COMPILER="$CLANG" -DCMAKE_CXX_COMPILER="$CLANGXX" \
        -DCMAKE_ASM_COMPILER="$CLANG" \
        -DCMAKE_C_FLAGS="$COMMON" -DCMAKE_CXX_FLAGS="$CXXFLAGS" \
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
        >"$BUILD/configure.log" 2>&1
}

# LLVM's own build reaches for a `llvm-tblgen` that runs on the host; this
# builds none, so one is borrowed. The generated `.inc` files are data tables,
# not code, which is why a host tool of the same major serves.
LLVM_TBLGEN="${LLVM_TBLGEN:-$(command -v "llvm-tblgen-$CXX_HOST_MAJOR" || true)}"
if [ -z "$LLVM_TBLGEN" ] && [ -x "/usr/lib/llvm-$CXX_HOST_MAJOR/bin/llvm-tblgen" ]; then
    LLVM_TBLGEN="/usr/lib/llvm-$CXX_HOST_MAJOR/bin/llvm-tblgen"
fi
[ -n "$LLVM_TBLGEN" ] && [ -x "$LLVM_TBLGEN" ] ||
    skip "no llvm-tblgen for LLVM $CXX_HOST_MAJOR on this host"

mkdir -p "$BUILD"
configure || {
    tail -n 30 "$BUILD/configure.log" >&2
    die "cmake configure failed; see $BUILD/configure.log"
}

ninja -C "$BUILD" LLVMSupport >"$BUILD/build.log" 2>&1 || {
    grep -h 'error:' "$BUILD/build.log" | sed 's/.*error: /  /' | sort -u | head -n 20 >&2
    die "LLVMSupport does not compile for $TARGET; see $BUILD/build.log"
}

if [ "$SELF_TEST" -eq 1 ]; then
    # A check that has never been observed to reject has not been observed to
    # work. `-U__slopos__` is the port's own effect removed: `<endian.h>`
    # becomes `<machine/endian.h>` and `statvfs.f_flag` becomes `f_flags`.
    PROBE="$BUILD/self-test.o"
    if "$CLANGXX" $CXXFLAGS -U__slopos__ -std=c++17 -fno-exceptions -fno-rtti \
        -I "$BUILD/include" -I "$SOURCE/llvm/include" \
        -I "$BUILD/lib/Support" -I "$SOURCE/llvm/lib/Support" \
        -c "$SOURCE/llvm/lib/Support/Path.cpp" -o "$PROBE" >/dev/null 2>&1; then
        die "self-test: Path.cpp compiled with the port's macro removed"
    fi
    rm -f "$PROBE"
    echo "$SELF: self-test ok"
fi

echo "$SELF: LLVMSupport compiles for $TARGET against slibc and libc++"
