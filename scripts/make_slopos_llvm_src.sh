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

# The pin's `llvm_*` values, the patch series, and this script — a
# materialiser is an input to the tree it builds, because the `tar -x` list
# below decides what ends up in one. The pin's prose and its `clang_*` lines
# are left out: neither can change what is extracted.
stamp_want() {
    {
        sed -n 's/^\(llvm_[a-z_]*\)=/\1=/p' "$PIN"
        sha256sum <"${BASH_SOURCE[0]}"
        for patch in "$PATCH_DIR"/*.patch; do
            sha256sum <"$patch"
        done
    } | sha256sum | cut -d' ' -f1
}

# Every patch is what `toolchain/cxx/PIN` says it is, checked before it is
# applied rather than only by the separate gate: a tree materialised from an
# edited patch is otherwise stamped self-consistently and accepted.
check_pinned() {
    local patch rel want have
    for patch in "$PATCH_DIR"/*.patch; do
        rel="toolchain/llvm/$(basename "$patch")"
        want="$(sed -n "s|^patch_sha256=$rel:\\(.*\\)$|\\1|p" "$PIN" | head -n 1)"
        [ -n "$want" ] ||
            die "$rel has no \`patch_sha256=$rel:<sha256>\` line in toolchain/cxx/PIN"
        have="$(sha256sum <"$patch" | cut -d' ' -f1)"
        [ "$have" = "$want" ] || die "$rel does not match its pin
       expected: $want (toolchain/cxx/PIN)
       actual:   $have"
    done
}

check_pinned

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
    # Verified before it is cached: a corrupt-but-complete download promoted
    # to the real name is one every later run then dies on.
    HAVE="$(sha256sum "$TARBALL.part" | cut -d' ' -f1)"
    [ "$HAVE" = "$LLVM_SHA256" ] || {
        rm -f "$TARBALL.part"
        die "checksum mismatch for the fetched $(basename "$TARBALL")
       expected: $LLVM_SHA256 (toolchain/cxx/PIN)
       actual:   $HAVE"
    }
    mv "$TARBALL.part" "$TARBALL"
fi
HAVE="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
[ "$HAVE" = "$LLVM_SHA256" ] || die "checksum mismatch for $(basename "$TARBALL")
       expected: $LLVM_SHA256 (toolchain/cxx/PIN)
       actual:   $HAVE"

# The old tree stands until the new one is whole: a `tar` or `patch` failure
# then leaves what was there rather than nothing.
echo "$SELF: extracting llvm-project $LLVM_VERSION (about 1.5 GB on disk)..." >&2
rm -rf "$SOURCE.part"
mkdir -p "$SOURCE.part"
tar -xf "$TARBALL" -C "$SOURCE.part" --strip-components=1 \
    "llvm-project-${LLVM_VERSION}.src/cmake" \
    "llvm-project-${LLVM_VERSION}.src/libcxx" \
    "llvm-project-${LLVM_VERSION}.src/libcxxabi" \
    "llvm-project-${LLVM_VERSION}.src/runtimes" \
    "llvm-project-${LLVM_VERSION}.src/third-party" \
    "llvm-project-${LLVM_VERSION}.src/llvm" \
    "llvm-project-${LLVM_VERSION}.src/clang"

# `git apply`, not `patch(1)`, for the reason `scripts/lib/toolchain_pin.sh`
# gives: `patch` applies with fuzz and is never reverse-checked, so a hunk can
# land at the wrong offset and still exit 0. The tree lives inside this
# repository, so `GIT_CEILING_DIRECTORIES` is what stops `git apply` resolving
# the patch's paths against the repository root and silently changing nothing.
CEILING="$(cd "$(dirname "$SOURCE")" && pwd)"
for patch in "$PATCH_DIR"/*.patch; do
    (cd "$SOURCE.part" && GIT_CEILING_DIRECTORIES="$CEILING" git apply -p1 "$patch") ||
        die "$(basename "$patch") does not apply to llvm-project $LLVM_VERSION"
    (cd "$SOURCE.part" && GIT_CEILING_DIRECTORIES="$CEILING" \
        git apply -p1 --reverse --check "$patch" >/dev/null 2>&1) ||
        die "$(basename "$patch") reported success but is not applied"
done

rm -rf "$SOURCE"
mv "$SOURCE.part" "$SOURCE"
echo "$WANT" >"$STAMP"
echo "$SELF: materialised $(basename "$SOURCE") with $(ls "$PATCH_DIR"/*.patch | wc -l) patch(es) (stamp $WANT)"
