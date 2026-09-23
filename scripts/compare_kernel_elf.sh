#!/usr/bin/env bash
# Hold two kernel ELFs to one loadable image and one symbol table: the identity
# check between two checkouts, or between a host build and a guest build.
#
# Usage: compare_kernel_elf.sh <a.elf> <b.elf>
#
# The image is `llvm-objcopy -O binary`, which keeps the allocated sections
# only, so the debug sections — which name source paths and are never loaded —
# are out of it. The symbol table is `llvm-nm -n` of each. On a difference it
# names the sections that differ and the first symbols that do.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
A="${1:?usage: compare_kernel_elf.sh <a.elf> <b.elf>}"
B="${2:?usage: compare_kernel_elf.sh <a.elf> <b.elf>}"
OBJCOPY="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objcopy)"
NM="$("$SCRIPT_DIR/llvm_tool.sh" llvm-nm)"
READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

"$OBJCOPY" -O binary "$A" "$TMP/a.bin"
"$OBJCOPY" -O binary "$B" "$TMP/b.bin"
"$NM" -n "$A" >"$TMP/a.nm"
"$NM" -n "$B" >"$TMP/b.nm"

alloc_sections() {
    "$READOBJ" --elf-output-style=GNU --sections --wide "$1" |
        sed -n 's/^ *\[ *[0-9]*\] //p' | awk '$7 ~ /A/ { print $1 }'
}

rc=0
if cmp -s "$TMP/a.bin" "$TMP/b.bin"; then
    echo "compare_kernel_elf: loadable image identical ($(wc -c <"$TMP/a.bin") bytes)"
else
    rc=1
    echo "compare_kernel_elf: loadable images differ ($(wc -c <"$TMP/a.bin") vs $(wc -c <"$TMP/b.bin") bytes)"
    for section in $(alloc_sections "$A"); do
        "$OBJCOPY" -O binary --only-section="$section" "$A" "$TMP/sa" 2>/dev/null || : >"$TMP/sa"
        "$OBJCOPY" -O binary --only-section="$section" "$B" "$TMP/sb" 2>/dev/null || : >"$TMP/sb"
        cmp -s "$TMP/sa" "$TMP/sb" ||
            echo "  $section: $(wc -c <"$TMP/sa") vs $(wc -c <"$TMP/sb") bytes, $(cmp -l "$TMP/sa" "$TMP/sb" 2>/dev/null | wc -l) differing"
    done
fi

if cmp -s "$TMP/a.nm" "$TMP/b.nm"; then
    echo "compare_kernel_elf: symbol table identical ($(wc -l <"$TMP/a.nm") symbols)"
else
    rc=1
    echo "compare_kernel_elf: symbol tables differ ($(wc -l <"$TMP/a.nm") vs $(wc -l <"$TMP/b.nm") symbols)"
    diff "$TMP/a.nm" "$TMP/b.nm" | sed -n '1,20p'
fi
exit "$rc"
