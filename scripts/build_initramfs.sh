#!/bin/sh
# Build a boot module's base image: a newc cpio of the userland binaries, the
# C library and the assets they read.
#
# Usage: build_initramfs.sh <out.cpio> <build_dir> <bin1> [bin2] ...
#
# POSIX sh, run on the host and in the guest alike; the packing is
# tools/initramfs, built for whichever machine runs this. Its argv matches
# build_fs_image.sh's, so the RAM root and the ext2 disk image are populated
# from the same binary list.
#
# Environment:
#   CARGO             - cargo command, split on blanks (default: cargo)
#   CARGO_TARGET_DIR  - where the tool is built (default: <build_dir>/target)
#   COREUTILS_LINKS, EXTRA_SHARED_OBJECTS, SLOPOS_BUILD_TAG
#                     - read by tools/initramfs; see its documentation
set -eu

if [ $# -lt 2 ]; then
    echo "usage: build_initramfs.sh <out.cpio> <build_dir> <bin1> [bin2] ..." >&2
    exit 2
fi
OUT="$1"
BUILD_DIR="$2"
shift 2
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TARGET_DIR="${CARGO_TARGET_DIR:-$BUILD_DIR/target}"
case "$TARGET_DIR" in
/*) ;;
*) TARGET_DIR="$(pwd)/$TARGET_DIR" ;;
esac

(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" $CARGO build --locked --release --quiet -p slopos-initramfs)
"$TARGET_DIR/release/initramfs" "$REPO_ROOT" "$OUT" "$BUILD_DIR" "$@"
