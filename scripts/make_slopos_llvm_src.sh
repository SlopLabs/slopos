#!/usr/bin/env bash
set -euo pipefail

# Materialise the pinned llvm-project sources with the SlopOS port applied.
#
# Usage: make_slopos_llvm_src.sh [--print-stamp]
#
# Emits third_party/llvm-project-<version>.src/ — the same tree
# `make_slopos_cxx.sh` builds the C++ runtime out of, extended to the whole of
# `llvm/` and `clang/` and carrying `toolchain/llvm/*.patch`. One tree rather
# than two: the patch touches neither `libcxx` nor `libcxxabi`, so a runtime
# built before or after it is the same runtime, and a second copy of a 1.5 GB
# source tree buys nothing.
#
# Idempotent: a stamp over `toolchain/cxx/PIN` and the patch series makes a
# warm run milliseconds. A stamp mismatch re-extracts rather than trying to
# unapply, because `patch -R` on a tree someone has half-edited is how a
# source tree becomes quietly wrong.

SELF="make_slopos_llvm_src"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

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

PATCH_DIR="$REPO_ROOT/toolchain/llvm"
TARBALL="$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src.tar.xz"
SOURCE="$REPO_ROOT/third_party/llvm-project-${LLVM_VERSION}.src"
STAMP="$SOURCE/.slopos-llvm-stamp"

stamp_want() {
    {
        cat "$PIN"
        for patch in "$PATCH_DIR"/*.patch; do
            sha256sum <"$patch"
        done
    } | sha256sum | cut -d' ' -f1
}

WANT="$(stamp_want)"
if [ "${1:-}" = "--print-stamp" ]; then
    echo "$WANT"
    exit 0
fi
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
    echo "$SELF: $(basename "$SOURCE") up to date (stamp $WANT)"
    exit 0
fi

if [ ! -f "$TARBALL" ]; then
    echo "$SELF: fetching llvm-project $LLVM_VERSION sources..." >&2
    mkdir -p "$(dirname "$TARBALL")"
    curl -L --fail --show-error "$LLVM_URL" -o "$TARBALL.part" || die "could not fetch $LLVM_URL
       An offline checkout pre-populates third_party/ with
       $(basename "$TARBALL"), or points LLVM_URL at a local copy."
    mv "$TARBALL.part" "$TARBALL"
fi
HAVE="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
[ "$HAVE" = "$LLVM_SHA256" ] || die "checksum mismatch for $(basename "$TARBALL")
       expected: $LLVM_SHA256 (toolchain/cxx/PIN)
       actual:   $HAVE"

echo "$SELF: extracting llvm-project $LLVM_VERSION (about 1.5 GB on disk)..." >&2
rm -rf "$SOURCE" "$SOURCE.part"
mkdir -p "$SOURCE.part"
tar -xf "$TARBALL" -C "$SOURCE.part" --strip-components=1 \
    "llvm-project-${LLVM_VERSION}.src/cmake" \
    "llvm-project-${LLVM_VERSION}.src/libcxx" \
    "llvm-project-${LLVM_VERSION}.src/libcxxabi" \
    "llvm-project-${LLVM_VERSION}.src/runtimes" \
    "llvm-project-${LLVM_VERSION}.src/third-party" \
    "llvm-project-${LLVM_VERSION}.src/llvm" \
    "llvm-project-${LLVM_VERSION}.src/clang"

for patch in "$PATCH_DIR"/*.patch; do
    patch -p1 -d "$SOURCE.part" -s <"$patch" ||
        die "$(basename "$patch") does not apply to llvm-project $LLVM_VERSION"
done

mv "$SOURCE.part" "$SOURCE"
echo "$WANT" >"$STAMP"
echo "$SELF: materialised $(basename "$SOURCE") with $(ls "$PATCH_DIR"/*.patch | wc -l) patch(es) (stamp $WANT)"
