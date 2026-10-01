# The boot disk's layout, as boot-core states it, and where a built disk's
# partitions lie, read from its GPT with sfdisk.

BOOTDISK_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# bootdisk_tool: the path of tools/bootdisk, built for this machine.
bootdisk_tool() {
    local target="${CARGO_TARGET_DIR:-$BOOTDISK_REPO/builddir/target}"
    case "$target" in /*) ;; *) target="$PWD/$target" ;; esac
    (cd "$BOOTDISK_REPO" && CARGO_TARGET_DIR="$target" ${CARGO:-cargo} build --locked --release --quiet -p slopos-bootdisk) ||
        return 1
    printf '%s\n' "$target/release/bootdisk"
}

# bootdisk_layout: set ESP_TYPE, BOOT_TYPE and the rest of the layout's names.
bootdisk_layout() {
    local tool layout
    tool="$(bootdisk_tool)" || return 1
    layout="$("$tool" layout)" || return 1
    eval "$layout"
}

# bootdisk_partition <image> <type GUID>: "<start> <size>", in bytes, of the
# one partition of that type; fails when there is none or more than one.
bootdisk_partition() {
    local image="$1" type="$2" sector found
    sector="$(sfdisk --dump "$image" | sed -n 's/^sector-size: *\([0-9]*\)$/\1/p')"
    [ -n "$sector" ] || { echo "bootdisk: $image carries no partition table" >&2; return 1; }
    found="$(sfdisk --dump "$image" | awk -v type="$type" -v sector="$sector" '
        tolower($0) ~ "type=" tolower(type) {
            start = $0; sub(/.*start= */, "", start); sub(/,.*/, "", start)
            size = $0; sub(/.*size= */, "", size); sub(/,.*/, "", size)
            printf "%.0f %.0f\n", start * sector, size * sector
        }')"
    case "$(printf '%s' "$found" | grep -c .)" in
        1) printf '%s\n' "$found" ;;
        0) echo "bootdisk: $image has no partition of type $type" >&2; return 1 ;;
        *) echo "bootdisk: $image has more than one partition of type $type" >&2; return 1 ;;
    esac
}

# bootdisk_partition_sha256 <image> <type GUID>: the SHA-256 of that partition.
bootdisk_partition_sha256() {
    local start size
    read -r start size < <(bootdisk_partition "$1" "$2") || return 1
    dd if="$1" bs=1M iflag=skip_bytes,count_bytes skip="$start" count="$size" status=none |
        sha256sum | cut -d' ' -f1
}
