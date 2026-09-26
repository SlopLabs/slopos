#!/usr/bin/env bash
set -euo pipefail

# Build a UEFI boot disk: a GPT image whose only partition is a FAT32 ESP
# carrying Limine and two kernel slots, `/boot/a` and `/boot/b`, which the
# guest installs into and switches between by rewriting `/limine.conf`.
#
# Usage: build_bootdisk.sh <out.img> <kernel.elf> <initramfs.cpio> <cmdline>
#
# Environment:
#   LIMINE_DIR - path to Limine directory (default: third_party/limine)
#   QEMU_FB_WIDTH, QEMU_FB_HEIGHT, QEMU_FB_AUTO, QEMU_FB_AUTO_POLICY,
#   QEMU_FB_AUTO_OUTPUT - framebuffer resolution, as build_iso.sh takes them

SELF="build_bootdisk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

USAGE="Usage: build_bootdisk.sh <out.img> <kernel.elf> <initramfs.cpio> <cmdline>"
if [ "$#" -ne 4 ]; then
    echo "$USAGE" >&2
    exit 2
fi
OUTPUT="$1"
KERNEL="$2"
INITRAMFS="$3"
CMDLINE="$4"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"

ESP_START_MIB=1
ESP_SIZE_MIB=512
SECTOR=512
ESP_START_SECTOR=$(( ESP_START_MIB * 1048576 / SECTOR ))
ESP_SECTORS=$(( ESP_SIZE_MIB * 1048576 / SECTOR ))
# One MiB of tail room holds the backup GPT header and entry array.
IMAGE_SIZE_MIB=$(( ESP_START_MIB + ESP_SIZE_MIB + 1 ))
ESP_TYPE_GUID="C12A7328-F81F-11D2-BA4B-00A0C93EC93B"

missing=""
for tool in sfdisk mkfs.fat mmd mcopy truncate dd; do
    command -v "$tool" >/dev/null 2>&1 || missing="$missing $tool"
done
if [ -n "$missing" ]; then
    echo "$SELF: missing host tools:$missing (sfdisk: util-linux, mkfs.fat: dosfstools, mmd/mcopy: mtools)" >&2
    exit 1
fi

for input in "$KERNEL" "$INITRAMFS"; do
    if [ ! -f "$input" ]; then
        echo "$SELF: $input not found. Build it first." >&2
        exit 1
    fi
done

"$SCRIPT_DIR/ensure_limine.sh"

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

OUT_DIR="$(dirname "$OUTPUT")"
mkdir -p "$OUT_DIR"
# Beside the output rather than in /tmp, which is often a RAM-backed tmpfs.
STAGING="$(mktemp -d "${OUT_DIR}/.bootdisk.XXXXXX")"
TMP_OUTPUT="${OUTPUT}.tmp"
trap 'rm -rf "$STAGING"; rm -f "$TMP_OUTPUT"' EXIT INT TERM

CONF="${STAGING}/limine.conf"
{
    printf 'timeout: 0\nserial: yes\nverbose: yes\ndefault_entry: slopos-a\n'
    for slot in a b; do
        printf '/slopos-%s\n' "$slot"
        printf '    protocol: limine\n'
        printf '    path: boot():/boot/%s/kernel.elf\n' "$slot"
        printf '    cmdline: %s\n' "${CMDLINE:+$CMDLINE }slot=$slot"
        printf '    module_path: boot():/boot/initramfs.cpio\n'
        printf '    module_string: initramfs\n'
        printf '    resolution: %sx%s\n' "$fb_w" "$fb_h"
    done
    # BOOTDISK_PANIC_ENTRY=1 adds a slot whose kernel panics and resets:
    # what `just test-install` rolls back from.
    if [ "${BOOTDISK_PANIC_ENTRY:-0}" = 1 ]; then
        printf '/slopos-bad\n'
        printf '    protocol: limine\n'
        printf '    path: boot():/boot/a/kernel.elf\n'
        printf '    cmdline: %s\n' "${CMDLINE:+$CMDLINE }slot=bad panic=reboot panic.boot=on"
        printf '    module_path: boot():/boot/initramfs.cpio\n'
        printf '    module_string: initramfs\n'
        printf '    resolution: %sx%s\n' "$fb_w" "$fb_h"
    fi
} > "$CONF"

ESP="${STAGING}/esp.img"
truncate -s "${ESP_SIZE_MIB}M" "$ESP"
mkfs.fat -F 32 -n SLOPOS-ESP -h "$ESP_START_SECTOR" "$ESP" >/dev/null

export MTOOLS_SKIP_CHECK=1
mmd -i "$ESP" ::/EFI ::/EFI/BOOT ::/boot ::/boot/a ::/boot/b
mcopy -i "$ESP" "$LIMINE_DIR/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
mcopy -i "$ESP" "$CONF" ::/limine.conf
mcopy -i "$ESP" "$KERNEL" ::/boot/a/kernel.elf
mcopy -i "$ESP" "$KERNEL" ::/boot/b/kernel.elf
mcopy -i "$ESP" "$INITRAMFS" ::/boot/initramfs.cpio
# Limine's BSD-2-Clause notice travels with the binary, as on the ISO.
mcopy -i "$ESP" "$LIMINE_DIR/LICENSE" ::/boot/LICENSE.limine
mcopy -i "$ESP" "$REPO_ROOT/NOTICE.md" ::/boot/NOTICE.md

rm -f "$TMP_OUTPUT"
truncate -s "${IMAGE_SIZE_MIB}M" "$TMP_OUTPUT"
printf 'label: gpt\nstart=%s, size=%s, type=%s, name="EFI system partition"\n' \
    "$ESP_START_SECTOR" "$ESP_SECTORS" "$ESP_TYPE_GUID" \
    | sfdisk --quiet "$TMP_OUTPUT"
dd if="$ESP" of="$TMP_OUTPUT" bs=1M seek="$ESP_START_MIB" conv=notrunc,sparse status=none

mv "$TMP_OUTPUT" "$OUTPUT"
trap - EXIT INT TERM
rm -rf "$STAGING"
echo "$SELF: wrote $OUTPUT (ESP ${ESP_SIZE_MIB} MiB, default_entry slopos-a)"
