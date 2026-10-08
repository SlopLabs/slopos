#!/usr/bin/env bash
set -euo pipefail

# The install medium's archive: what the live system serves at /media/install
# beside the kernel and base it booted, carried by the loader as its `install`
# module.
#
# Usage: build_install_medium.sh <out.cpio> [<toolchain>]
#
# Always Limine's loader, its licence and notices, and NOTICE.md, under boot/,
# which the installer puts on the ESP and beside the slots, with the GPT disk
# GUID chosen for this medium, also left beside the archive as
# <out.cpio>.disk-guid for scripts/build_iso.sh to build the image with; under sources/
# the pinned tarball and recipe of each recipe the base takes programs from,
# with the scripts that build them, the source of what the medium distributes
# of them.
# Given a toolchain, the payload as well, an ext4 volume left beside the
# archive as <out.cpio>.payload, which build_iso.sh appends to the image as a
# partition the kernel mounts: the toolchain at usr/local, the manifest an
# install records for it at var/lib/slopos/trees/usr_local, and at src/ a
# `--vendored` clone of HEAD whose origin is GitHub, which the installer
# points at whatever remote the user names, with every recipe's tarball in its
# third_party/recipes/. The volume is in the profile, shrunk with resize2fs -M
# and given ext4's read-only feature, which mke2fs refuses at creation, so no
# system that mounts it writes it.
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
. "$SCRIPT_DIR/lib/ext4.sh"

OUT_DIR="$(dirname "$OUT")"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
STAGE="$(mktemp -d "$OUT_DIR/.medium.XXXXXX")"
trap 'rm -rf "$STAGE"; rm -f "$OUT.tmp" "$OUT.disk-guid.tmp" "$OUT.payload.tmp"' EXIT INT TERM

# The toolchain and the staged clone, every path a file of its own as
# fs_tree.py writes it, in a volume sized for them and then shrunk.
payload_volume() {
    local image="$1" bytes entries inodes said block blocks
    read -r bytes entries < <(find -H "$TOOLCHAIN" "$STAGE/src" -printf '%s\n' |
        awk '{ total += $1 } END { print total, NR }')
    inodes=$((entries + entries / 10 + 1024))
    rm -f "$image"
    truncate -s "$(((bytes + entries * 4096 + inodes * 256) * 21 / 20 + (64 << 20)))" "$image"
    ext4_mkfs_args
    mke2fs -q -F "${EXT4_MKFS_ARGS[@]}" -J size=4 -m 0 -N "$inodes" "$image"
    python3 "$SCRIPT_DIR/fs_tree.py" install "$image" "$TOOLCHAIN" /usr/local \
        --manifests "$STAGE/manifests" --name usr_local
    python3 "$SCRIPT_DIR/fs_tree.py" install "$image" "$STAGE/src" /src
    # 1: debugfs left counts e2fsck corrected.
    e2fsck -fp "$image" >/dev/null || [ $? -eq 1 ] || die "e2fsck refused the payload before it was shrunk"
    said="$(resize2fs -M "$image" 2>&1)" || die "resize2fs could not shrink the payload: $said"
    block="$(dumpe2fs -h "$image" 2>/dev/null | sed -n 's/^Block size:[[:space:]]*//p')"
    blocks="$(dumpe2fs -h "$image" 2>/dev/null | sed -n 's/^Block count:[[:space:]]*//p')"
    truncate -s "$((block * blocks))" "$image"
    tune2fs -O read-only "$image" >/dev/null
    e2fsck -fn "$image" >/dev/null 2>&1 || die "e2fsck refuses the payload"
    ext4_meets_profile "$image" || die "the payload is not in the profile"
    [ -z "$(ext4_unrest "$image")" ] || die "the payload is not at rest: $(ext4_unrest "$image")"
}

boot="$STAGE/$(dirname "$MEDIUM_LOADER")"
mkdir -p "$boot"
cp "$LIMINE_DIR/BOOTX64.EFI" "$STAGE/$MEDIUM_LOADER"
cp "$LIMINE_DIR/LICENSE" "$STAGE/$MEDIUM_LOADER_LICENSE"
cp "$LIMINE_DIR/3RDPARTY.md" "$STAGE/$MEDIUM_LOADER_NOTICES"
cp "$REPO_ROOT/NOTICE.md" "$STAGE/$MEDIUM_NOTICE"
disk_guid="$(python3 -c 'import uuid; print(uuid.uuid4())')"
printf '%s\n' "$disk_guid" >"$STAGE/$MEDIUM_DISK_GUID"
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
    "$SCRIPT_DIR/stage_workspace.sh" "$STAGE/src" --vendored --remote "$REMOTE"
    mkdir -p "$STAGE/src/slopos/third_party/recipes"
    "$SCRIPT_DIR/build_recipes.sh" --fetch-source | while IFS= read -r tarball; do
        cp "$tarball" "$STAGE/src/slopos/third_party/recipes/"
    done
    payload_volume "$OUT.payload.tmp"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$OUT_DIR/target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$PWD/$TARGET_DIR" ;; esac
(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" ${CARGO:-cargo} build --locked --release --quiet -p slopos-initramfs)
"$TARGET_DIR/release/initramfs" tree "$OUT.tmp" "${trees[@]}"
printf '%s\n' "$disk_guid" >"$OUT.disk-guid.tmp"
# An archive is never left beside another build's GUID or payload: until all
# are in place there is no GUID, which build_iso.sh refuses.
rm -f "$OUT.disk-guid" "$OUT.payload"
mv "$OUT.tmp" "$OUT"
[ -z "$TOOLCHAIN" ] || mv "$OUT.payload.tmp" "$OUT.payload"
mv "$OUT.disk-guid.tmp" "$OUT.disk-guid"
