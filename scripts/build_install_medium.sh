#!/usr/bin/env bash
set -euo pipefail

# The install medium's archive: what the live system serves at /media/install
# beside the kernel and base it booted, carried by the loader as its `install`
# module.
#
# Usage: build_install_medium.sh <out.cpio> [<toolchain>]
#
# Always Limine's loader, its licence and NOTICE.md, under boot/, which the
# installer puts on the ESP and beside the slots; and under sources/ the pinned
# tarball and recipe of each recipe the base takes programs from, with the
# scripts that build them, the source of what the medium distributes of them.
# Given a toolchain, the payload as well: the toolchain at usr/local, the
# manifest an install records for it at var/lib/slopos/trees/usr_local, and at
# src/ a `--vendored` clone of HEAD whose origin is GitHub, which the installer
# points at whatever remote the user names, with every recipe's tarball in its
# third_party/recipes/.
#
# Environment:
#   LIMINE_DIR       Limine's binaries (default: third_party/limine)
#   CARGO            cargo command, split on blanks (default: cargo)
#   CARGO_TARGET_DIR where the packer is built (default: <out dir>/target)

SELF="build_install_medium"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
USAGE="usage: $SELF.sh <out.cpio> [<toolchain>]"
REMOTE="https://github.com/SlopLabs/slopos"

OUT="${1:?$USAGE}"
TOOLCHAIN="${2:-}"
[ $# -le 2 ] || { echo "$USAGE" >&2; exit 2; }
LIMINE_DIR="${LIMINE_DIR:-$REPO_ROOT/third_party/limine}"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

[ -z "$TOOLCHAIN" ] || [ -x "$TOOLCHAIN/bin/rustc" ] || die "$TOOLCHAIN holds no toolchain; run just toolchain"
"$SCRIPT_DIR/ensure_limine.sh"
. "$SCRIPT_DIR/lib/bootdisk.sh"
bootdisk_layout

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
STAGE="$(mktemp -d "$OUT_DIR/.medium.XXXXXX")"
trap 'rm -rf "$STAGE"; rm -f "$OUT.tmp"' EXIT INT TERM

boot="$STAGE/$(dirname "$MEDIUM_LOADER")"
mkdir -p "$boot"
cp "$LIMINE_DIR/BOOTX64.EFI" "$STAGE/$MEDIUM_LOADER"
cp "$LIMINE_DIR/LICENSE" "$STAGE/$MEDIUM_LOADER_LICENSE"
cp "$REPO_ROOT/NOTICE.md" "$STAGE/$MEDIUM_NOTICE"
trees=("$boot=$(dirname "$MEDIUM_LOADER")")

. "$SCRIPT_DIR/lib/base.sh"
mkdir -p "$STAGE/sources"
for recipe in $BASE_RECIPES; do
    cp "$("$SCRIPT_DIR/build_recipes.sh" --fetch-source "$recipe")" "$STAGE/sources/"
    mkdir -p "$STAGE/sources/$recipe"
    cp "$REPO_ROOT/toolchain/recipes/$recipe/"* "$STAGE/sources/$recipe/"
done
mkdir -p "$STAGE/sources/scripts/lib"
for script in build_recipes.sh make_slopos_cross.sh cxx_host_tools.sh make_slopos_cxx.sh \
    lib/rustc_build_settings.sh; do
    cp "$SCRIPT_DIR/$script" "$STAGE/sources/scripts/$script"
done
trees+=("$STAGE/sources=sources")

if [ -n "$TOOLCHAIN" ]; then
    manifests="$STAGE/var/lib/slopos/trees"
    mkdir -p "$manifests"
    python3 "$SCRIPT_DIR/fs_tree.py" manifest "$TOOLCHAIN" >"$manifests/usr_local"
    "$SCRIPT_DIR/stage_workspace.sh" "$STAGE/src" --vendored --remote "$REMOTE"
    mkdir -p "$STAGE/src/slopos/third_party/recipes"
    "$SCRIPT_DIR/build_recipes.sh" --fetch-source | while IFS= read -r tarball; do
        cp "$tarball" "$STAGE/src/slopos/third_party/recipes/"
    done
    trees+=("$STAGE/var=var" "$TOOLCHAIN=usr/local" "$STAGE/src=src")
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$OUT_DIR/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$PWD/$TARGET_DIR" ;; esac
(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" ${CARGO:-cargo} build --locked --release --quiet -p slopos-initramfs)
"$TARGET_DIR/release/initramfs" tree "$OUT.tmp" "${trees[@]}"
mv "$OUT.tmp" "$OUT"
