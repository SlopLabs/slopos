#!/usr/bin/env bash
set -euo pipefail

# Build a UEFI boot disk in the layout every SlopOS disk has, as boot-core
# states it:
#
#   ESP           Limine and its configuration under \EFI\SlopOS\, and the
#                 removable-media copy an ESP SlopOS created carries
#   SlopOS boot   FAT32: a directory per slot under /boot, each a kernel and
#                 the base image it boots with
#   SlopOS root   the ext4 image BOOTDISK_ROOT_IMAGE names, when it names one
#   SlopOS crash  raw, zero
#
# The configuration names no default: the guest tries a slot with
# LoaderEntryOneShot and commits it with LoaderEntryDefault, and nothing it
# does writes the ESP.
#
# Usage: build_bootdisk.sh <out.img> <kernel.elf> <base.cpio> <cmdline>
#
# Environment:
#   LIMINE_DIR - path to Limine directory (default: third_party/limine)
#   BOOTDISK_ROOT_IMAGE - an ext4 image the disk carries as its root
#     partition; every slot boots with it as root=PARTUUID=
#   BOOTDISK_PANIC_ENTRY=1 - add slot bad, holding the same system with a
#     command line that panics and resets it: what `just test-install` rolls
#     back from
#   QEMU_FB_WIDTH, QEMU_FB_HEIGHT, QEMU_FB_AUTO, QEMU_FB_AUTO_POLICY,
#   QEMU_FB_AUTO_OUTPUT - framebuffer resolution, as build_iso.sh takes them

SELF="build_bootdisk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/bootdisk.sh
. "$SCRIPT_DIR/lib/bootdisk.sh"

USAGE="Usage: build_bootdisk.sh <out.img> <kernel.elf> <base.cpio> <cmdline>"
if [ "$#" -ne 4 ]; then
    echo "$USAGE" >&2
    exit 2
fi
OUTPUT="$1"
KERNEL="$2"
BASE="$3"
CMDLINE="$4"
ROOT_IMAGE="${BOOTDISK_ROOT_IMAGE:-}"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"

missing=""
for tool in sfdisk mkfs.fat mmd mcopy truncate dd; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "$SELF: missing host tools:$missing (sfdisk: util-linux or fdisk, mkfs.fat: dosfstools, mmd/mcopy: mtools)" >&2
    exit 1
fi

for input in "$KERNEL" "$BASE" ${ROOT_IMAGE:+"$ROOT_IMAGE"}; do
    if [ ! -f "$input" ]; then
        echo "$SELF: $input not found. Build it first." >&2
        exit 1
    fi
done

"$SCRIPT_DIR/ensure_limine.sh"
BOOTDISK="$(bootdisk_tool)"
bootdisk_layout

fb_w="${QEMU_FB_WIDTH:-1920}"
fb_h="${QEMU_FB_HEIGHT:-1080}"
if [ "${QEMU_FB_AUTO:-0}" != "0" ] && [ -x "$SCRIPT_DIR/detect_qemu_resolution.sh" ]; then
    detected="$(QEMU_FB_WIDTH="$fb_w" QEMU_FB_HEIGHT="$fb_h" \
        QEMU_FB_AUTO_POLICY="${QEMU_FB_AUTO_POLICY:-primary}" \
        QEMU_FB_AUTO_OUTPUT="${QEMU_FB_AUTO_OUTPUT:-}" \
        "$SCRIPT_DIR/detect_qemu_resolution.sh")" || true
    if [ -n "$detected" ]; then
        fb_w="${detected%% *}"
        fb_h="${detected##* }"
    fi
fi

SECTOR=512
MIB=1048576
sectors() { echo $(( $1 * MIB / SECTOR )); }
mtools_path() { printf '::%s' "${1//\\//}"; }
# Make every directory above the UEFI path $2 on the FAT image $1.
mmd_parents() {
    local dir="" part
    local -a parts
    IFS='\' read -ra parts <<<"${2#\\}"
    unset 'parts[-1]'
    for part in "${parts[@]}"; do
        dir="$dir/$part"
        mdir -i "$1" "::$dir" >/dev/null 2>&1 || mmd -i "$1" "::$dir"
    done
}

ESP_START=$ALIGN_MIB
BOOT_START=$(( ESP_START + NEW_ESP_MIB ))
ROOT_START=$(( BOOT_START + BOOT_MIB ))
ROOT_MIB=0
if [ -n "$ROOT_IMAGE" ]; then
    ROOT_MIB=$(( ( $(wc -c <"$ROOT_IMAGE") + MIB - 1 ) / MIB ))
fi
CRASH_START=$(( ROOT_START + ROOT_MIB ))
# The tail holds the backup GPT header and entry array.
IMAGE_MIB=$(( CRASH_START + CRASH_MIB + ALIGN_MIB ))

OUT_DIR="$(dirname "$OUTPUT")"
mkdir -p "$OUT_DIR"
# Beside the output rather than in /tmp, which is often a RAM-backed tmpfs.
STAGING="$(mktemp -d "${OUT_DIR}/.bootdisk.XXXXXX")"
TMP_OUTPUT="${OUTPUT}.tmp"
trap 'rm -rf "$STAGING"; rm -f "$TMP_OUTPUT"' EXIT INT TERM

rm -f "$TMP_OUTPUT"
truncate -s "${IMAGE_MIB}M" "$TMP_OUTPUT"
{
    printf 'label: gpt\n'
    printf 'start=%s, size=%s, type=%s, name="%s"\n' \
        "$(sectors "$ESP_START")" "$(sectors "$NEW_ESP_MIB")" "$ESP_TYPE" "$ESP_NAME"
    printf 'start=%s, size=%s, type=%s, name="%s"\n' \
        "$(sectors "$BOOT_START")" "$(sectors "$BOOT_MIB")" "$BOOT_TYPE" "$BOOT_NAME"
    if [ -n "$ROOT_IMAGE" ]; then
        printf 'start=%s, size=%s, type=%s, name="%s"\n' \
            "$(sectors "$ROOT_START")" "$(sectors "$ROOT_MIB")" "$ROOT_TYPE" "$ROOT_NAME"
    fi
    printf 'start=%s, size=%s, type=%s, name="%s"\n' \
        "$(sectors "$CRASH_START")" "$(sectors "$CRASH_MIB")" "$CRASH_TYPE" "$CRASH_NAME"
} | sfdisk --quiet --no-reread --no-tell-kernel "$TMP_OUTPUT"
BOOT_UUID="$(sfdisk --part-uuid "$TMP_OUTPUT" 2)"
ROOT_UUID=""
if [ -n "$ROOT_IMAGE" ]; then
    ROOT_UUID="$(sfdisk --part-uuid "$TMP_OUTPUT" 3)"
fi

read -ra slots <<<"$SLOTS"
slot_args=()
for slot in "${slots[@]}"; do
    slot_args+=(--slot "$slot:slot=$slot")
done
if [ "${BOOTDISK_PANIC_ENTRY:-0}" = 1 ]; then
    slots+=(bad)
    slot_args+=(--slot "bad:slot=bad panic=reboot panic.boot=on")
fi
CONF="${STAGING}/limine.conf"
"$BOOTDISK" limine-conf --boot "$BOOT_UUID" ${ROOT_UUID:+--root "$ROOT_UUID"} \
    --resolution "${fb_w}x${fb_h}" --serial "${slot_args[@]}" -- "$CMDLINE" >"$CONF"

export MTOOLS_SKIP_CHECK=1

# An ESP this script creates is SlopOS's alone, so it carries the
# removable-media path too: what a firmware with no entry for the disk boots.
ESP="${STAGING}/esp.img"
truncate -s "${NEW_ESP_MIB}M" "$ESP"
mkfs.fat -F 32 -n "$ESP_LABEL" -h "$(sectors "$ESP_START")" "$ESP" >/dev/null
for loader in "$LOADER" "$FALLBACK_LOADER"; do
    mmd_parents "$ESP" "$loader"
    mcopy -i "$ESP" "$LIMINE_DIR/BOOTX64.EFI" "$(mtools_path "$loader")"
done
for config in "$LOADER_CONFIG" "$FALLBACK_CONFIG"; do
    mcopy -i "$ESP" "$CONF" "$(mtools_path "$config")"
done
# Limine's BSD-2-Clause notice travels with the binary, as on the ISO.
mcopy -i "$ESP" "$LIMINE_DIR/LICENSE" "$(mtools_path "$LOADER_DIR")/LICENSE.limine"

BOOT="${STAGING}/boot.img"
truncate -s "${BOOT_MIB}M" "$BOOT"
mkfs.fat -F 32 -n "$BOOT_LABEL" -h "$(sectors "$BOOT_START")" "$BOOT" >/dev/null
mmd -i "$BOOT" "::$SLOTS_DIR"
for slot in "${slots[@]}"; do
    mmd -i "$BOOT" "::$SLOTS_DIR/$slot"
    mcopy -i "$BOOT" "$KERNEL" "::$SLOTS_DIR/$slot/$KERNEL_FILE"
    mcopy -i "$BOOT" "$BASE" "::$SLOTS_DIR/$slot/$BASE_FILE"
done
mcopy -i "$BOOT" "$REPO_ROOT/NOTICE.md" "::$SLOTS_DIR/NOTICE.md"

place() {
    dd if="$1" of="$TMP_OUTPUT" bs=1M seek="$2" conv=notrunc,sparse status=none
}
place "$ESP" "$ESP_START"
place "$BOOT" "$BOOT_START"
if [ -n "$ROOT_IMAGE" ]; then
    place "$ROOT_IMAGE" "$ROOT_START"
fi

mv "$TMP_OUTPUT" "$OUTPUT"
trap - EXIT INT TERM
rm -rf "$STAGING"
echo "$SELF: wrote $OUTPUT (ESP ${NEW_ESP_MIB} MiB, boot ${BOOT_MIB} MiB${ROOT_IMAGE:+, root ${ROOT_MIB} MiB}, crash ${CRASH_MIB} MiB)"
