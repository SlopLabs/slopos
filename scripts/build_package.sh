#!/bin/sh
# Build the package laid out in a directory, checked as an install checks it
# and its programs held to what every program outside the base is, and sign
# its sidecar when a key is named.
#
# Usage: build_package.sh <dir> <out-dir> <libc.so>
#
# Environment:
#   PACKAGE_KEY       - an SSH Ed25519 private key; signs <name>.meta into
#                       <name>.meta.sig, as `pkgctl install-signed` takes it
#   CARGO             - cargo command, split on blanks (default: cargo)
#   CARGO_TARGET_DIR  - where the tool is built (default: builddir/target)
set -eu

if [ $# -ne 3 ]; then
    echo "usage: build_package.sh <dir> <out-dir> <libc.so>" >&2
    exit 2
fi
DIR="$1"
OUT="$2"
LIBC="$3"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/builddir/target}"
case "$TARGET_DIR" in
/*) ;;
*) TARGET_DIR="$(pwd)/$TARGET_DIR" ;;
esac

(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" $CARGO build --locked --release --quiet -p slopos-package -p slopos-xtask)
# Every ELF file outside lib/ is a program, as the image check holds it.
find "$DIR" \( -path "$DIR/lib" -o -path "$DIR/.package" \) -prune -o -type f -exec sh -c '
    for file; do
        if [ "$(head -c 4 "$file" | od -An -c | tr -d " ")" = 177ELF ]; then
            printf "%s\0" "$file"
        fi
    done' sh {} + | xargs -0r "$TARGET_DIR/release/xtask" programs
report="$("$TARGET_DIR/release/package" "$DIR" "$OUT" "$LIBC")"
printf '%s\n' "$report"
sidecar="$(printf '%s\n' "$report" | sed -n 's/^sidecar //p')"
if [ -n "${PACKAGE_KEY:-}" ]; then
    ssh-keygen -Y sign -q -f "$PACKAGE_KEY" -n package@slopos -O hashalg=sha512 "$sidecar"
    echo "signed $sidecar.sig with $PACKAGE_KEY"
fi
