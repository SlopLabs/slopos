#!/usr/bin/env bash
set -euo pipefail

# Build the C++ runtime for `x86_64-unknown-slopos`, on the Linux host or in
# the guest.
#
# Usage: make_slopos_cxx.sh <sysroot_lib_dir>
#        make_slopos_cxx.sh --print-stamp <sysroot_lib_dir>
#        make_slopos_cxx.sh --print-abi-flags
#        make_slopos_cxx.sh --fetch-source
#
# `--fetch-source` fetches the pinned llvm-project tarball into third_party/
# unless it is there, verifies it, and prints its path.
#
# `<sysroot_lib_dir>` holds `libc.so`, which the runtime links: the C++ library
# must reach the C library through the shared one, because two copies of a libc
# in one process is two allocators, two `errno`s and two object tables.
#
# Emits third_party/slopos-cxx/:
#   include/c++/v1/   the C++ headers a cross build compiles against
#   lib/libc++.so     libc++ and libc++abi in one object
#   lib/libc++.a      the same two archives, for a static C++ program
#   licenses/         both projects' license texts, to ship beside the object
#
# Idempotent: a stamp over toolchain/cxx/PIN, the host compiler's version,
# this file, the CMake modules it configures with (toolchain/cmake and
# toolchain/cxx/FindPython3.cmake), slibc/include and `libbuiltins.a` makes a
# warm run a few milliseconds. `just clean` does not remove the result; the
# build is minutes and the inputs are pinned.
#
# Five things about this build are not upstream's defaults and all are load
# bearing.
#
#   * Localization, wide characters and the random device are ON. Every one of
#     them was off, and `llvm/lib/Support/raw_os_ostream.cpp` reaches `<ios>`,
#     `ConvertUTF.cpp` reaches `std::wstring` and `LockFileManager.cpp` reaches
#     `std::random_device`, so LLVM did not compile at all. What they cost the
#     C library is measured in `plans/self-hosting.md`.
#   * `_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` takes libc++'s own classification
#     table and its own `ctype_base::mask` bits. The alternative is the glibc
#     road, where the table is `__ctype_b_loc()`'s and the bits are `_ISspace`
#     and friends — a second classification of the same 128 characters, with a
#     `#error` for any platform that supplies neither.
#   * `_LIBCPP_USING_GETENTROPY` rather than the `/dev/urandom` default, which
#     is the device SlopOS's devfs does not have.
#   * CMake links nothing. Clang has no toolchain for an unknown OS, so its
#     driver hands the link to `gcc`, which would then supply the host's crt
#     objects and the host's libc. The runtimes are therefore built as static
#     archives with `CMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY`, and the one
#     shared object is linked here, by `ld.lld`, on a line this file states.
#   * `CMAKE_SYSTEM_NAME` is `SlopOS`, from the platform module in
#     toolchain/cmake, which the cmake recipe's patch installs into CMake too.
#     It sets `UNIX`, which libc++abi gates `cxa_thread_atexit.cpp` on: a
#     system CMake knows nothing about drops `__cxa_thread_atexit`, and every
#     `thread_local` with a destructor then fails to link with nothing to say
#     why.

SELF="make_slopos_cxx"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

# `--print-stamp` answers "what should the stamp be", which is how
# `check_cxx_pin.sh` holds a built tree to its inputs without a second copy of
# the digest to drift from this one.
# `_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE` changes `ctype_base::mask` and the
# twelve class bits in it, so a consumer compiled without it disagrees with
# this runtime about a type it passes by value. `--print-abi-flags` is how
# `build_userland.sh` and the gates take it from here rather than restating it.
ABI_FLAGS="-D_LIBCPP_PROVIDES_DEFAULT_RUNE_TABLE"

PRINT_STAMP=0
FETCH_ONLY=0
case "${1:-}" in
    --print-abi-flags)
        echo "$ABI_FLAGS"
        exit 0
        ;;
    --print-stamp)
        PRINT_STAMP=1
        shift
        ;;
    --fetch-source) FETCH_ONLY=1 ;;
esac

PIN="$REPO_ROOT/toolchain/cxx/PIN"
[ -f "$PIN" ] || die "missing toolchain/cxx/PIN"

pin_value() {
    sed -n "s/^$1=\\(.*\\)$/\\1/p" "$PIN" | head -n 1
}

LLVM_VERSION="$(pin_value llvm_version)"
LLVM_URL="${LLVM_URL:-$(pin_value llvm_url)}"
LLVM_SHA256="$(pin_value llvm_sha256)"
[ -n "$LLVM_VERSION" ] && [ -n "$LLVM_SHA256" ] ||
    die "toolchain/cxx/PIN is missing a pinned value"

TARGET="x86_64-unknown-slopos"
OUT="$REPO_ROOT/third_party/slopos-cxx"
STAMP="$OUT/.slopos-stamp"
TARBALL="$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src.tar.xz"
SOURCE="$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src"
BUILD="${BUILD_DIR:-$REPO_ROOT/builddir}/cxx-build"

# Verified before it is cached: a corrupt-but-complete download promoted to
# the real name is one every later run then dies on.
fetch_source() {
    local have
    if [ ! -f "$TARBALL" ]; then
        echo "$SELF: fetching llvm-project $LLVM_VERSION sources..." >&2
        mkdir -p "$(dirname "$TARBALL")"
        curl -L --fail --show-error "$LLVM_URL" -o "$TARBALL.part" || die "could not fetch $LLVM_URL
       An offline checkout pre-populates third_party/ with
       $(basename "$TARBALL"), or points LLVM_URL at a local copy."
        have="$(sha256sum "$TARBALL.part" | cut -d' ' -f1)"
        [ "$have" = "$LLVM_SHA256" ] || {
            rm -f "$TARBALL.part"
            die "checksum mismatch for the fetched $(basename "$TARBALL")
       expected: $LLVM_SHA256 (toolchain/cxx/PIN)
       actual:   $have"
        }
        mv "$TARBALL.part" "$TARBALL"
    fi
    have="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
    [ "$have" = "$LLVM_SHA256" ] || die "checksum mismatch for $(basename "$TARBALL")
       expected: $LLVM_SHA256 (toolchain/cxx/PIN)
       actual:   $have"
}

if [ "$FETCH_ONLY" -eq 1 ]; then
    fetch_source
    echo "$TARBALL"
    exit 0
fi

SYSROOT_LIB="${1:?usage: make_slopos_cxx.sh [--print-stamp] <sysroot_lib_dir>}"
SYSROOT_LIB="$(cd "$SYSROOT_LIB" && pwd)"
for library in libc.so libbuiltins.a; do
    [ -f "$SYSROOT_LIB/$library" ] ||
        die "no $library in $SYSROOT_LIB — build the userland first"
done

# ---------------------------------------------------------------------------
# Host tools. Resolved by `cxx_host_tools.sh` against the pin's floor, and
# resolved for `--print-stamp` too: the compiler is an input to the artifact,
# so the stamp names it and a host compiler upgrade rebuilds the runtime
# rather than leaving one object built by a compiler that is no longer here.
# ---------------------------------------------------------------------------
# Assigned before it is eval'd: `eval "$(cmd)"` discards the substitution's
# exit status, so a host with no usable toolchain would reach the stamp with
# every tool variable unset.
CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"

if [ "$PRINT_STAMP" -eq 0 ]; then
    for tool in cmake ninja; do
        command -v "$tool" >/dev/null 2>&1 ||
            die "$tool is required to build the C++ runtime"
    done
fi

# The pin's values, the C headers and `libbuiltins.a`. Deliberately not
# `libc.so`: the runtime is *compiled* against the headers and only *linked*
# against that library, so rebuilding it for every change to slibc's
# implementation would put three minutes on the interactive loop for nothing,
# and a libc that loses a symbol the runtime needs is what
# `scripts/check_cxx_pin.sh` is for. `libbuiltins.a` is different in kind:
# its members are copied *into* the object linked below.
#
# The pin's values and not its prose, and only the lines this build reads: a
# comment, or the llvm port's checksum, must not invalidate a three-minute
# build it cannot change the result of.
stamp_want() {
    {
        sed -n 's/^\(llvm_[a-z_]*\|clang_[a-z_]*\)=/\1=/p' "$PIN"
        # The compiler, by its own version string: two hosts at different
        # majors produce different objects from these same sources, and the
        # probes `build_userland.sh` compiles must come from the one that
        # built the runtime they link.
        "$CLANGXX" --version | sed -n 1p
        # This file, because the cmake line below decides what is in the
        # archives as much as the pin does. Contents only: `sha256sum FILE`
        # prints the path it was given, which would make the stamp depend on
        # whether the caller invoked this script relatively or absolutely.
        sha256sum <"${BASH_SOURCE[0]}"
        (cd "$REPO_ROOT/toolchain/cmake" && find . -type f | sort | xargs sha256sum)
        sha256sum <"$REPO_ROOT/toolchain/cxx/FindPython3.cmake"
        # Names as well as contents: two headers with swapped bodies, or a
        # header added empty, leave a content-only digest unchanged.
        (cd "$REPO_ROOT/slibc/include" && find . -type f | sort | xargs sha256sum)
        sha256sum <"$SYSROOT_LIB/libbuiltins.a"
    } | sha256sum | cut -d' ' -f1
}

WANT="$(stamp_want)"
if [ "$PRINT_STAMP" -eq 1 ]; then
    echo "$WANT"
    exit 0
fi
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
    echo "$SELF: third_party/slopos-cxx up to date (stamp $WANT)"
    exit 0
fi

if [ ! -d "$SOURCE/runtimes" ]; then
    fetch_source
    rm -rf "$SOURCE" "$SOURCE.part"
    mkdir -p "$SOURCE.part"
    # CMake's tar, because the guest's reads no xz.
    top="llvm-project-${LLVM_VERSION}.src"
    (cd "$SOURCE.part" && cmake -E tar xf "$TARBALL" -- "$top/cmake" "$top/libcxx" \
        "$top/libcxxabi" "$top/runtimes" "$top/third-party" "$top/llvm/cmake")
    mv "$SOURCE.part/$top" "$SOURCE"
    rmdir "$SOURCE.part"
fi

# The runtimes' configure runs `llvm/cmake/config.guess`, which names a SlopOS
# machine only with the port's hunk. Idempotent, so it holds for a tree unpacked
# here and for one `scripts/make_slopos_llvm_src.sh` has already patched.
for patch in "$REPO_ROOT"/toolchain/llvm/*.patch; do
    (cd "$SOURCE" && GIT_CEILING_DIRECTORIES="$(dirname "$SOURCE")" \
        git apply -p1 --include='llvm/cmake/*' --reverse --check "$patch" >/dev/null 2>&1) ||
        (cd "$SOURCE" && GIT_CEILING_DIRECTORIES="$(dirname "$SOURCE")" \
            git apply -p1 --include='llvm/cmake/*' --whitespace=nowarn "$patch") ||
        die "$(basename "$patch") does not apply to $SOURCE/llvm/cmake"
done

# ---------------------------------------------------------------------------
# Configure and build the two archives.
#
# `-nostdlibinc` and not `-nostdinc`: the compiler's own freestanding headers
# (`stddef.h`, `stdint.h`, `stdarg.h`) are the compiler's to provide, and only
# the *system* include path is slibc's.
#
# The `_HAS_*_LIB` cache entries are pre-seeded because a static-library
# try-compile links nothing, so every `check_library_exists` comes back true:
# left alone, the link line grows `-lpthread -lrt -latomic -lgcc_s` and the
# build hard-requires `__cxa_thread_atexit_impl` on the strength of a test that
# never ran. Each one below is a fact about slibc, not a probe result.
# ---------------------------------------------------------------------------
FLAGS="--target=$TARGET -nostdlibinc -isystem $REPO_ROOT/slibc/include -fPIC"
MODULES="$REPO_ROOT/toolchain/cmake"
command -v python3 >/dev/null 2>&1 || MODULES="$MODULES;$REPO_ROOT/toolchain/cxx"
FLAGS="$FLAGS $ABI_FLAGS -D_LIBCPP_USING_GETENTROPY"
rm -rf "$BUILD"
mkdir -p "$BUILD"

cmake -G Ninja -S "$SOURCE/runtimes" -B "$BUILD" -Wno-dev \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_SYSTEM_NAME=SlopOS \
    -DCMAKE_MODULE_PATH="$MODULES" \
    -DCMAKE_SYSTEM_PROCESSOR=x86_64 \
    -DCMAKE_C_COMPILER="$CLANG" \
    -DCMAKE_CXX_COMPILER="$CLANGXX" \
    -DCMAKE_ASM_COMPILER="$CLANG" \
    -DCMAKE_C_FLAGS="$FLAGS" \
    -DCMAKE_CXX_FLAGS="$FLAGS" \
    -DCMAKE_ASM_FLAGS="$FLAGS" \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_INSTALL_PREFIX="$BUILD/install" \
    -DLLVM_DEFAULT_TARGET_TRIPLE="$TARGET" \
    -DLLVM_ENABLE_RUNTIMES="libcxxabi;libcxx" \
    -DLLVM_INCLUDE_TESTS=OFF \
    -DLIBCXX_CXX_ABI=libcxxabi \
    -DLIBCXX_ENABLE_SHARED=OFF -DLIBCXX_ENABLE_STATIC=ON \
    -DLIBCXXABI_ENABLE_SHARED=OFF -DLIBCXXABI_ENABLE_STATIC=ON \
    -DLIBCXX_ENABLE_ABI_LINKER_SCRIPT=OFF \
    -DLIBCXXABI_USE_LLVM_UNWINDER=OFF \
    -DLIBCXXABI_ENABLE_EXCEPTIONS=ON \
    -DLIBCXXABI_ENABLE_NEW_DELETE_DEFINITIONS=ON \
    -DLIBCXX_ENABLE_NEW_DELETE_DEFINITIONS=OFF \
    -DLIBCXX_ENABLE_THREADS=ON -DLIBCXX_HAS_PTHREAD_API=ON \
    -DLIBCXXABI_ENABLE_THREADS=ON -DLIBCXXABI_HAS_PTHREAD_API=ON \
    -DLIBCXX_ENABLE_LOCALIZATION=ON \
    -DLIBCXX_ENABLE_WIDE_CHARACTERS=ON \
    -DLIBCXX_ENABLE_RANDOM_DEVICE=ON \
    -DLIBCXX_ENABLE_FILESYSTEM=ON \
    -DLIBCXX_ENABLE_TIME_ZONE_DATABASE=OFF \
    -DLIBCXX_HAS_MUSL_LIBC=OFF \
    -DLIBCXX_INCLUDE_BENCHMARKS=OFF -DLIBCXX_INCLUDE_TESTS=OFF \
    -DLIBCXX_INCLUDE_DOCS=OFF -DLIBCXXABI_INCLUDE_TESTS=OFF \
    -DLIBCXXABI_HAS_CXA_THREAD_ATEXIT_IMPL=1 \
    -DLIBCXX_HAS_PTHREAD_LIB=0 -DLIBCXX_HAS_RT_LIB=0 -DLIBCXX_HAS_ATOMIC_LIB=0 \
    -DLIBCXX_HAS_GCC_S_LIB=0 -DLIBCXX_HAS_GCC_LIB=0 -DLIBCXX_HAS_M_LIB=0 \
    -DLIBCXXABI_HAS_PTHREAD_LIB=0 -DLIBCXXABI_HAS_DL_LIB=0 \
    -DLIBCXXABI_HAS_GCC_S_LIB=0 -DLIBCXXABI_HAS_GCC_LIB=0 \
    >"$BUILD/configure.log" 2>&1 ||
    { tail -n 40 "$BUILD/configure.log" >&2; die "cmake configure failed; see $BUILD/configure.log"; }

ninja -C "$BUILD" cxxabi cxx >"$BUILD/build.log" 2>&1 ||
    { tail -n 40 "$BUILD/build.log" >&2; die "libc++ build failed; see $BUILD/build.log"; }

ABI_ARCHIVE="$BUILD/lib/libc++abi.a"
CXX_ARCHIVE="$BUILD/lib/libc++.a"
for archive in "$ABI_ARCHIVE" "$CXX_ARCHIVE"; do
    [ -f "$archive" ] || die "the build emitted no $(basename "$archive")"
done

# ---------------------------------------------------------------------------
# One shared object, both libraries.
#
#   --eh-frame-hdr  the PT_GNU_EH_FRAME the unwinder's frame finder looks for.
#                   Without it a throw from this object finds no FDE and
#                   terminates with nothing to say.
#   -z now          eager binding, which is the only kind the loader does.
#   -z relro        with eager binding nothing writes a GOT slot afterwards,
#                   so the whole segment seals.
#   no -Bsymbolic   `operator new` is replaceable by the program that links
#                   this, and binding libc++'s own calls to it locally is
#                   exactly what would stop that working.
# ---------------------------------------------------------------------------
rm -rf "$OUT"
mkdir -p "$OUT/lib"

#   libbuiltins.a  compiler-rt's 128-bit helpers, which x86-64 codegen calls
#                  and no libgcc supplies here. `libc.so` cannot publish them:
#                  rustc gives a cdylib a version script that localises
#                  everything but the crate's own exports. An archive last on
#                  the line yields only the members still undefined by the
#                  point it is read, which is how every other platform takes
#                  compiler-rt.
"$LD_LLD" -shared -o "$OUT/lib/libc++.so" \
    --soname=libc++.so --eh-frame-hdr -z now -z relro \
    --whole-archive "$ABI_ARCHIVE" "$CXX_ARCHIVE" --no-whole-archive \
    -L "$SYSROOT_LIB" -lc "$SYSROOT_LIB/libbuiltins.a"

# The same pair as an archive, for a C++ program that links no shared object.
rm -f "$OUT/lib/libc++.a"
MRI="$BUILD/libc++.mri"
{
    printf 'create %s\n' "$OUT/lib/libc++.a"
    printf 'addlib %s\n' "$ABI_ARCHIVE"
    printf 'addlib %s\n' "$CXX_ARCHIVE"
    printf 'save\nend\n'
} >"$MRI"
"$LLVM_AR" -M <"$MRI"

mkdir -p "$OUT/include"
cp -r "$BUILD/include/c++" "$OUT/include/c++"

# Apache-2.0 §4(a) asks each copy of the work to carry the license, the same
# reason the OFL fonts ship theirs beside the `.ttf`.
mkdir -p "$OUT/licenses"
for project in libcxx libcxxabi; do
    cp "$SOURCE/$project/LICENSE.TXT" "$OUT/licenses/$project-LICENSE.TXT"
done

echo "$WANT" >"$STAMP"
echo "$SELF: built third_party/slopos-cxx from llvm-project $LLVM_VERSION with LLVM $CXX_HOST_MAJOR (stamp $WANT)"
