#!/usr/bin/env bash
set -euo pipefail

# Build a diagnostic Limine BOOTX64.EFI that draws the steps of its handoff,
# any CPU exception and any page that changed behind its back straight into
# the framebuffer, for a machine without a serial port that stops between
# Limine's framebuffer clear and the kernel. tools/limine-diag/README.md
# says how to install it and read what it draws.
#
# The pinned Limine source release (tools/limine-diag/PIN) is fetched into
# third_party/ once. Offline environments may pre-populate that file, or point
# LIMINE_SRC_URL at a copy (a file:// URL works). The tree is extracted afresh
# into builddir/limine-diag/src on every run.
#
# The result is padded to the stock binary's file and image size: firmware
# places a loader by those, and every allocation Limine makes moves with where
# it lands. Leaves builddir/limine-diag/BOOTX64.EFI and, for addr2line,
# builddir/limine-diag/limine.elf.
#
# Usage: build_limine_diag.sh [--autoboot-as-editor] [--define NAME[=VALUE]]...
#                             [--no-size-match]
#   --autoboot-as-editor  also apply 0002: Enter, the timeout and a one-shot
#                         boot do what an unchanged e + F10 does first
#   --define NAME[=VALUE] pass -DNAME[=VALUE] to the build: the self-tests
#                         (SLOPOS_DIAG_TEST_FAULT, SLOPOS_DIAG_TEST_CORRUPT),
#                         SLOPOS_DIAG_E9_TRACE, and with --autoboot-as-editor
#                         SLOPOS_DIAG_MIMIC_NO_ALLOC or SLOPOS_DIAG_MIMIC_NO_FLUSH
#   --no-size-match       keep the binary at its natural size (shifts layout)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

DIAG_DIR="$REPO_ROOT/tools/limine-diag"
PIN="$DIAG_DIR/PIN"
OUT_DIR="${LIMINE_DIAG_OUT:-$REPO_ROOT/builddir/limine-diag}"
SRC_DIR="$OUT_DIR/src"

die() {
    echo "build_limine_diag: $*" >&2
    exit 1
}

pin() {
    sed -n "s/^$1=\\(.*\\)$/\\1/p" "$PIN" | head -n 1
}

AUTOBOOT_AS_EDITOR=0
SIZE_MATCH=1
DEFINES=()
while [ $# -gt 0 ]; do
    case "$1" in
        --autoboot-as-editor) AUTOBOOT_AS_EDITOR=1 ;;
        --no-size-match) SIZE_MATCH=0 ;;
        --define)
            [ $# -ge 2 ] || die "--define needs NAME[=VALUE]"
            case "$2" in
                [A-Za-z_]*) DEFINES+=("-D$2") ;;
                *) die "bad --define: $2" ;;
            esac
            shift ;;
        -h|--help) sed -n '4,/^SCRIPT_DIR=/{/^SCRIPT_DIR=/d;s/^# \{0,1\}//;p}' "$0"; exit 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
    shift
done

[ -f "$PIN" ] || die "missing $PIN"
VERSION="$(pin limine_version)"
URL="$(pin limine_url)"
SHA256="$(pin limine_sha256)"
STOCK_FILE_SIZE="$(pin stock_file_size)"
STOCK_IMAGE_SIZE="$(pin stock_image_size)"
[ -n "$VERSION" ] && [ -n "$URL" ] && [ -n "$SHA256" ] && [ -n "$STOCK_FILE_SIZE" ] && [ -n "$STOCK_IMAGE_SIZE" ] \
    || die "$PIN is missing a pinned value"

# The loader the diagnostics stand in for is the one ensure_limine.sh installs.
LOADER_VERSION="$(sed -n 's/^LIMINE_VERSION="\${LIMINE_VERSION:-\([^}]*\)}"$/\1/p' "$SCRIPT_DIR/ensure_limine.sh")"
[ -n "$LOADER_VERSION" ] || die "cannot read LIMINE_VERSION from scripts/ensure_limine.sh"
if [ "$LOADER_VERSION" != "$VERSION" ] || [ "${LIMINE_VERSION:-$VERSION}" != "$VERSION" ]; then
    die "tools/limine-diag/PIN is Limine $VERSION but the loader is ${LIMINE_VERSION:-$LOADER_VERSION} (scripts/ensure_limine.sh): re-pin and re-cut the patches together"
fi

for tool in clang ld.lld llvm-objcopy llvm-objdump llvm-readelf nasm make patch od; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found"
done

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

# Little-endian 32-bit field of a file at a byte offset.
u32_at() {
    od -An -tu4 -j "$2" -N4 "$1" | tr -d ' '
}

pe_image_size() {
    local lfanew
    lfanew="$(u32_at "$1" 60)"
    u32_at "$1" $((lfanew + 24 + 56))
}

file_size() {
    wc -c < "$1" | tr -d ' '
}

STOCK_FILE_SIZE=$((STOCK_FILE_SIZE))
STOCK_IMAGE_SIZE=$((STOCK_IMAGE_SIZE))
STOCK_EFI="$REPO_ROOT/third_party/limine/BOOTX64.EFI"
if [ -f "$STOCK_EFI" ] && grep -aq "Limine ${VERSION} " "$STOCK_EFI"; then
    if [ "$(file_size "$STOCK_EFI")" != "$STOCK_FILE_SIZE" ] || [ "$(pe_image_size "$STOCK_EFI")" != "$STOCK_IMAGE_SIZE" ]; then
        die "$STOCK_EFI is $(file_size "$STOCK_EFI") bytes, image $(pe_image_size "$STOCK_EFI"); PIN says $STOCK_FILE_SIZE, $STOCK_IMAGE_SIZE"
    fi
fi

TARBALL="$REPO_ROOT/third_party/limine-${VERSION}.tar.xz"
if [ ! -f "$TARBALL" ] || [ "$(sha256_of "$TARBALL")" != "$SHA256" ]; then
    mkdir -p "$REPO_ROOT/third_party"
    echo "Fetching Limine v${VERSION} sources..." >&2
    curl -L --fail --progress-bar "${LIMINE_SRC_URL:-$URL}" -o "$TARBALL.part"
    actual="$(sha256_of "$TARBALL.part")"
    if [ "$actual" != "$SHA256" ]; then
        rm -f "$TARBALL.part"
        die "checksum mismatch for limine-${VERSION}.tar.xz: expected $SHA256, got $actual"
    fi
    mv "$TARBALL.part" "$TARBALL"
fi

rm -rf "$SRC_DIR"
mkdir -p "$SRC_DIR"
tar -xf "$TARBALL" -C "$SRC_DIR" --strip-components=1

PATCHES=("$DIAG_DIR/0001-slopos-handoff-diagnostics.patch")
if [ "$AUTOBOOT_AS_EDITOR" = 1 ]; then
    PATCHES+=("$DIAG_DIR/0002-slopos-autoboot-as-editor.patch")
fi
for p in "${PATCHES[@]}"; do
    patch -d "$SRC_DIR" -p1 -s --no-backup-if-mismatch < "$p" || die "$(basename "$p") does not apply"
done

echo "Configuring Limine v${VERSION} (UEFI x86-64, LLVM toolchain)..." >&2
(cd "$SRC_DIR" && ./configure \
    CC_FOR_TARGET="${CC_FOR_TARGET:-clang}" \
    LD_FOR_TARGET="${LD_FOR_TARGET:-ld.lld}" \
    OBJCOPY_FOR_TARGET="${OBJCOPY_FOR_TARGET:-llvm-objcopy}" \
    OBJDUMP_FOR_TARGET="${OBJDUMP_FOR_TARGET:-llvm-objdump}" \
    READELF_FOR_TARGET="${READELF_FOR_TARGET:-llvm-readelf}" \
    --enable-uefi-x86-64 > "$OUT_DIR/configure.log" 2>&1) \
    || die "configure failed, see $OUT_DIR/configure.log"

JOBS="$(nproc 2>/dev/null || echo 4)"
EFI="$SRC_DIR/bin/BOOTX64.EFI"

build() {
    # A changed define reaches only the file that reads it.
    touch "$SRC_DIR/common/lib/slopos_diag.c"
    make -C "$SRC_DIR" -j"$JOBS" \
        CPPFLAGS_FOR_TARGET="${DEFINES[*]:-} -DSLOPOS_DIAG_STOCK_IMAGE_SIZE=$STOCK_IMAGE_SIZE $*" \
        > "$OUT_DIR/make.log" 2>&1 || die "build failed, see $OUT_DIR/make.log"
}

echo "Building..." >&2
build -DSLOPOS_DIAG_PAD_RODATA=0 -DSLOPOS_DIAG_PAD_BSS=0
FILE_SIZE="$(file_size "$EFI")"
IMAGE_SIZE="$(pe_image_size "$EFI")"

if [ "$SIZE_MATCH" = 1 ]; then
    # .rodata padding grows the file and the image alike, .bss only the image.
    pad_ro=$((STOCK_FILE_SIZE - FILE_SIZE))
    pad_bss=$((STOCK_IMAGE_SIZE - IMAGE_SIZE - pad_ro))
    if [ "$pad_ro" -lt 0 ] || [ "$pad_bss" -lt 0 ] || [ $((pad_ro % 4096)) -ne 0 ] || [ $((pad_bss % 4096)) -ne 0 ]; then
        die "the diagnostic binary (file $FILE_SIZE, image $IMAGE_SIZE) does not fit stock's (file $STOCK_FILE_SIZE, image $STOCK_IMAGE_SIZE); --no-size-match builds it anyway, with a shifted layout"
    fi
    build -DSLOPOS_DIAG_PAD_RODATA=$pad_ro -DSLOPOS_DIAG_PAD_BSS=$pad_bss
    FILE_SIZE="$(file_size "$EFI")"
    IMAGE_SIZE="$(pe_image_size "$EFI")"
    [ "$FILE_SIZE" = "$STOCK_FILE_SIZE" ] && [ "$IMAGE_SIZE" = "$STOCK_IMAGE_SIZE" ] \
        || die "padding missed: file $FILE_SIZE, image $IMAGE_SIZE; stock file $STOCK_FILE_SIZE, image $STOCK_IMAGE_SIZE"
fi

cp "$EFI" "$OUT_DIR/BOOTX64.EFI"
cp "$SRC_DIR/common-uefi-x86-64/limine.elf" "$OUT_DIR/limine.elf"

variant="handoff diagnostics"
[ "$AUTOBOOT_AS_EDITOR" = 1 ] && variant="$variant + autoboot-as-editor"
[ "${#DEFINES[@]}" -gt 0 ] && variant="$variant, ${DEFINES[*]}"
printf 'Limine %s %s\n' "$VERSION" "$variant"
printf '  %s  (file %d bytes, image 0x%x%s)\n' "$OUT_DIR/BOOTX64.EFI" "$FILE_SIZE" "$IMAGE_SIZE" \
    "$([ "$SIZE_MATCH" = 1 ] && echo ', stock-sized')"
printf '  sha256 %s\n' "$(sha256_of "$OUT_DIR/BOOTX64.EFI")"
printf '  addr2line -f -e %s <hex after LIMINE+>\n' "$OUT_DIR/limine.elf"
echo "Install: back up \\EFI\\SlopOS\\BOOTX64.EFI on the ESP, copy this file over it and boot as usual;"
echo "         tools/limine-diag/README.md has the steps, how to read the screen and how to restore."
