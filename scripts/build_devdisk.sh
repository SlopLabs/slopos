#!/usr/bin/env bash
set -euo pipefail

# Build the dev disk: the volume a cross-built toolchain lands on.
#
# Usage: build_devdisk.sh <image_path> <build_dir>
#
# The layout is a target sysroot — `lib/`, `include/`, `include/c++/v1/`,
# `licenses/` — with whatever `TOOLCHAIN_STAGE` holds copied over the top, so a
# `bin/` carrying rustc, cargo, clang and rust-lld lands beside the libraries
# those programs link against. A guest that mounts this volume has everything
# `x86_64-unknown-slopos` needs to compile and link a program, which the ISO's
# read-only appliance root deliberately does not carry.
#
# The image is `VERITY=off PRESERVE_FS_IMAGE=1`, which is the whole difference
# between this and the shipped root: a dev disk is a workbench, so the guest
# writes to it and a rebuild must not discard what the guest wrote. The volume
# therefore holds a *fresh* sysroot only on the run that creates it; after that
# `build_fs_image.sh` preserves what is there. Discard one with
# `rm -f <image> <image>.stamp`.
#
# Image building itself is `build_fs_image.sh`'s: this script stages a
# directory, hands it over as `FS_POPULATE_DIR`, and then writes a
# `SLOPOS-DEVDISK` marker at the volume root, which `devdisk_test` reads back
# in the guest.
#
# Environment:
#   DEV_DISK_SIZE - volume size (default: 2G). A cross-built rustc plus cargo
#                   is ~1 GB of `bin/` and `lib/rustlib/`, and the sysroot
#                   below is ~19 MB.
#   DEV_DISK_INODE_RATIO - bytes of volume per inode (default: 16384, the
#                   mke2fs default). The C++ headers alone are ~1000 files.
#   TOOLCHAIN_STAGE - a directory holding a cross-built toolchain, copied over
#                   the staged sysroot. Unset stages the sysroot alone, which
#                   is what a run before the toolchain exists wants.

SELF="build_devdisk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

IMAGE_PATH="${1:?Usage: build_devdisk.sh <image_path> <build_dir>}"
BUILD_DIR="${2:?Usage: build_devdisk.sh <image_path> <build_dir>}"

DEV_DISK_SIZE="${DEV_DISK_SIZE:-2G}"
DEV_DISK_INODE_RATIO="${DEV_DISK_INODE_RATIO:-16384}"

USERLAND_TARGET="${USERLAND_TARGET:-x86_64-unknown-slopos}"
RELEASE_DIR="${BUILD_DIR}/target/${USERLAND_TARGET}/release"
CXX_DIR="${REPO_ROOT}/third_party/slopos-cxx"
STAGE="${BUILD_DIR}/devdisk-stage"
MARKER="SLOPOS-DEVDISK"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

missing() {
    echo "$SELF: $1 is missing" >&2
    echo "  The dev disk is a target sysroot; without it the volume is an empty" >&2
    echo "  workbench that fails at link time inside the guest instead of here." >&2
    echo "  Build it:   $2" >&2
    exit 1
}

# `libc.a` is the one staged library `build_userland.sh` leaves in cargo's
# output directory rather than copying to the build directory, so both are
# searched and the error names the build directory either way.
#
# Answers through a global rather than on stdout: `missing` inside a command
# substitution exits only the subshell, and the caller would carry on staging
# an empty path.
staged_lib() {
    local name="$1" candidate
    for candidate in "${BUILD_DIR}/${name}" "${RELEASE_DIR}/${name}"; do
        if [ -f "$candidate" ]; then
            STAGED_LIB="$candidate"
            return 0
        fi
    done
    missing "${BUILD_DIR}/${name}" "scripts/build_userland.sh --test '${BUILD_DIR}'"
}

rm -rf "$STAGE"
mkdir -p "$STAGE/lib" "$STAGE/include/c++/v1"

for lib in libc.so libc.a libbuiltins.a crt0.o libc++.so; do
    staged_lib "$lib"
    cp -a "$STAGED_LIB" "$STAGE/lib/$lib"
done
[ -f "$CXX_DIR/lib/libc++.a" ] ||
    missing "$CXX_DIR/lib/libc++.a" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/lib/libc++.a" "$STAGE/lib/libc++.a"

[ -d "${REPO_ROOT}/slibc/include" ] || die "slibc/include is not a directory"
cp -a "${REPO_ROOT}/slibc/include/." "$STAGE/include/"

[ -d "$CXX_DIR/include/c++/v1" ] ||
    missing "$CXX_DIR/include/c++/v1" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/include/c++/v1/." "$STAGE/include/c++/v1/"

[ -d "$CXX_DIR/licenses" ] ||
    missing "$CXX_DIR/licenses" "scripts/make_slopos_cxx.sh '${RELEASE_DIR}'"
cp -a "$CXX_DIR/licenses" "$STAGE/licenses"

if [ -n "${TOOLCHAIN_STAGE:-}" ]; then
    [ -d "$TOOLCHAIN_STAGE" ] ||
        die "TOOLCHAIN_STAGE='$TOOLCHAIN_STAGE' is not a directory"
    # Hardlinks when the stage shares a filesystem with the build directory: a
    # cross-built toolchain is ~1 GB and this runs on every dev-disk build.
    cp -alf "$TOOLCHAIN_STAGE/." "$STAGE/" 2>/dev/null ||
        cp -af "$TOOLCHAIN_STAGE/." "$STAGE/"
fi

echo "$SELF: staged $(du -sh "$STAGE" | cut -f1) for $IMAGE_PATH"

FS_IMAGE_SIZE="$DEV_DISK_SIZE" \
FS_INODE_RATIO="$DEV_DISK_INODE_RATIO" \
VERITY=off \
PRESERVE_FS_IMAGE=1 \
FS_POPULATE_DIR="$STAGE" \
    "$SCRIPT_DIR/build_fs_image.sh" "$IMAGE_PATH" "$BUILD_DIR"

# The inventory is measured on the finished volume, never on the stage. A
# preserved image is not repopulated, and `build_fs_image.sh` refreshes
# `/lib/libc.so` on one anyway, so a marker written from the stage would
# state the size of a library the volume does not hold and fail the guest
# test for a discrepancy that is the marker's own.
image_size() {
    debugfs -R "stat /$1" "$IMAGE_PATH" 2>/dev/null |
        sed -n 's/^User:.*Size: \([0-9]\{1,\}\).*/\1/p' | head -n 1
}

image_holds_dir() {
    debugfs -R "stat /$1" "$IMAGE_PATH" 2>/dev/null | grep -q 'Type: directory'
}

# A preserved dev disk that predates a library is the one case this cannot
# describe: naming it is the whole point, and the fix is the developer's to
# take, as `build_fs_image.sh` does with every image it will not delete.
stale() {
    echo "" >&2
    echo "$SELF: $IMAGE_PATH does not carry $1" >&2
    echo "  It was preserved from an earlier build that staged a different" >&2
    echo "  sysroot, so the marker cannot describe it." >&2
    echo "  Discard it: rm -f '$IMAGE_PATH' '$IMAGE_PATH.stamp'" >&2
    exit 1
}

MARKER_FILE="${BUILD_DIR}/devdisk-marker.txt"
{
    echo "$MARKER 1"
    echo "target ${USERLAND_TARGET}"
    for rel in include include/c++/v1 licenses; do
        image_holds_dir "$rel" || stale "$rel"
        printf 'dir %s\n' "$rel"
    done
    # Subdirectories get a `dir` line rather than a recursive walk: a
    # `TOOLCHAIN_STAGE` lands `lib/rustlib`, which is most of the volume and
    # thousands of files, and one `debugfs` call each would cost more than
    # the image build. Without the line a dev disk whose toolchain never
    # arrived passes every structural check.
    for dir in lib bin; do
        [ -d "$STAGE/$dir" ] || continue
        for entry in "$STAGE/$dir"/*; do
            rel="$dir/$(basename "$entry")"
            if [ -d "$entry" ]; then
                image_holds_dir "$rel" || stale "$rel"
                printf 'dir %s\n' "$rel"
                continue
            fi
            [ -f "$entry" ] || continue
            size="$(image_size "$rel")"
            [ -n "$size" ] || stale "$rel"
            printf 'file %s %s\n' "$size" "$rel"
        done
    done
} >"$MARKER_FILE"

debugfs -w -R "rm /$MARKER" "$IMAGE_PATH" >/dev/null 2>&1 || true
debugfs -w -R "write $MARKER_FILE $MARKER" "$IMAGE_PATH" >/dev/null

echo "$SELF: $(grep -c '^file ' "$MARKER_FILE") files inventoried in /$MARKER"
