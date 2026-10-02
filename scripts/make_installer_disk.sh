#!/usr/bin/env bash
set -euo pipefail

# A disk `just test-installer` installs onto: 16 GiB, sparse, and as one of
# three machines would have it.
#
# Usage: make_installer_disk.sh blank|foreign|reuse <out.img>
#
#   blank    nothing on it.
#   foreign  another system's: an ESP holding its loader under \EFI\other\,
#            a data partition, and the rest free.
#   reuse    a partition named installer-test-root holding a Linux filesystem,
#            for the root, and the rest free.
#
# For `foreign`, `<out.img>.foreign/` keeps what the other system's files and
# partition held, which the check compares the disk with afterwards.

SELF="make_installer_disk"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
USAGE="usage: $SELF.sh blank|foreign|reuse <out.img>"
KIND="${1:?$USAGE}"
OUT="${2:?$USAGE}"
SIZE=16G
LINUX_FS="0fc63daf-8483-4772-8e79-3d69d8477de4"
ESP_MIB=100
DATA_MIB=512
REUSED_MIB=10240

for tool in sfdisk mkfs.fat mmd mcopy mke2fs truncate dd sha256sum; do
    command -v "$tool" >/dev/null || { echo "$SELF: $tool is required" >&2; exit 1; }
done
. "$SCRIPT_DIR/lib/bootdisk.sh"
bootdisk_layout

rm -rf "$OUT" "$OUT.foreign"
truncate -s "$SIZE" "$OUT"
mib() { echo $(( $1 * 2048 )); }
place() {
    dd if="$1" of="$OUT" bs=1M seek="$2" conv=notrunc,sparse status=none
}

case "$KIND" in
    blank) ;;
    foreign)
        {
            printf 'label: gpt\n'
            printf 'start=%s, size=%s, type=%s, name="EFI system partition"\n' \
                "$(mib 1)" "$(mib $ESP_MIB)" "$ESP_TYPE"
            printf 'start=%s, size=%s, type=%s, name="foreign data"\n' \
                "$(mib $(( 1 + ESP_MIB )))" "$(mib $DATA_MIB)" "$LINUX_FS"
        } | sfdisk --quiet --no-reread --no-tell-kernel "$OUT"
        keep="$OUT.foreign"
        mkdir -p "$keep"
        head -c 65536 /dev/urandom >"$keep/BOOTX64.EFI"
        printf 'set timeout=5\n' >"$keep/grub.cfg"
        esp="$keep/esp.img"
        truncate -s "${ESP_MIB}M" "$esp"
        mkfs.fat -F 32 -s 1 -n OTHER "$esp" >/dev/null
        export MTOOLS_SKIP_CHECK=1
        mmd -i "$esp" ::/EFI ::/EFI/other
        mcopy -i "$esp" "$keep/BOOTX64.EFI" "$keep/grub.cfg" ::/EFI/other/
        place "$esp" 1
        rm "$esp"
        data="$keep/data.img"
        truncate -s "${DATA_MIB}M" "$data"
        head -c $(( 4 << 20 )) /dev/urandom | dd of="$data" conv=notrunc status=none
        place "$data" $(( 1 + ESP_MIB ))
        sha256sum <"$data" | cut -d' ' -f1 >"$keep/data.sha256"
        rm "$data"
        ;;
    reuse)
        printf 'label: gpt\nstart=%s, size=%s, type=%s, name="installer-test-root"\n' \
            "$(mib 1)" "$(mib $REUSED_MIB)" "$LINUX_FS" |
            sfdisk --quiet --no-reread --no-tell-kernel "$OUT"
        old="$OUT.part"
        truncate -s "${REUSED_MIB}M" "$old"
        mke2fs -q -F -t ext4 -L previous "$old"
        place "$old" 1
        rm "$old"
        ;;
    *)
        echo "$USAGE" >&2
        exit 2
        ;;
esac
echo "$SELF: wrote $OUT ($KIND)"
