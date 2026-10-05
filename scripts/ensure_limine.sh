#!/usr/bin/env bash
set -euo pipefail

# Ensure the Limine bootloader SlopOS ships is built.
#
# Limine is built from the pinned release tarball with toolchain/limine's
# patches (toolchain/limine/PIN says why) into third_party/limine, holding the
# files the ISO, the boot disks and the install medium take: BOOTX64.EFI,
# BOOTIA32.EFI, limine-bios.sys, limine-bios-cd.bin, limine-uefi-cd.bin and
# LICENSE. A stamp over the pin, the patches, the build scripts and the tools
# that built it decides whether it is rebuilt; a warm run is a hash.
#
# Needs make, nasm (the x86 ports), mtools (limine-uefi-cd.bin) and an LLVM
# toolchain: clang, ld.lld, llvm-objcopy, llvm-objdump and llvm-readelf, picked
# as scripts/lib/limine.sh says. The tarball is fetched into third_party/ once;
# offline, put it there or point LIMINE_SRC_URL at a copy.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "ensure_limine: $*" >&2
    exit 1
}

# shellcheck source=lib/limine.sh
. "$SCRIPT_DIR/lib/limine.sh"

DEFAULT_DIR="${REPO_ROOT}/third_party/limine"
LIMINE_DIR="${LIMINE_DIR:-$DEFAULT_DIR}"
case "$LIMINE_DIR" in /*) ;; *) LIMINE_DIR="$PWD/$LIMINE_DIR" ;; esac
STAMP="$LIMINE_DIR/.slopos-limine-stamp"

limine_load_pin
limine_tools mtools

WANT="$({
    cat "$LIMINE_PIN" "${LIMINE_PATCHES[@]}" "$SCRIPT_DIR/ensure_limine.sh" "$SCRIPT_DIR/lib/limine.sh"
    "$LIMINE_CC" --version | head -n 1
    "$LIMINE_LD" --version | head -n 1
    nasm -v
} | limine_sha256_of)"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
    exit 0
fi

# Only the default location is replaced; a directory the caller named holds
# someone else's files unless this script built them.
if [ -d "$LIMINE_DIR" ] && [ -n "$(ls -A "$LIMINE_DIR" 2>/dev/null)" ] \
    && [ "$LIMINE_DIR" != "$DEFAULT_DIR" ] && [ ! -f "$STAMP" ]; then
    die "$LIMINE_DIR holds files this script did not build; remove it or name another LIMINE_DIR"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/limine-build.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
SRC="$WORK/src"
limine_patched_tree "$SRC"

echo "Building Limine v${LIMINE_VERSION} with ${#LIMINE_PATCHES[@]} patch(es) ($("$LIMINE_CC" --version | head -n 1))..." >&2
limine_configure "$SRC" "$WORK/configure.log" \
    --enable-bios --enable-bios-cd --enable-uefi-ia32 --enable-uefi-x86-64 --enable-uefi-cd
make -C "$SRC" -j"$(nproc 2>/dev/null || echo 4)" > "$WORK/make.log" 2>&1 \
    || { tail -n 30 "$WORK/make.log" >&2; die "build failed"; }

OUT="$WORK/out"
mkdir -p "$OUT"
for f in BOOTX64.EFI BOOTIA32.EFI limine-bios.sys limine-bios-cd.bin limine-uefi-cd.bin; do
    [ -f "$SRC/bin/$f" ] || die "the build made no $f"
    cp "$SRC/bin/$f" "$OUT/$f"
done
cp "$SRC/COPYING" "$OUT/LICENSE"
printf '%s\n' "$WANT" > "$OUT/.slopos-limine-stamp"

rm -rf "$LIMINE_DIR"
mkdir -p "$(dirname "$LIMINE_DIR")"
cp -a "$OUT" "$LIMINE_DIR"
echo "Limine v${LIMINE_VERSION} ready in $LIMINE_DIR" >&2
