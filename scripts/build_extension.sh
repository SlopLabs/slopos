#!/bin/sh
# Pack the extension laid out in a directory, checked as an install checks it
# and its programs held to what every program outside the base is, and sign
# its sidecar when a key is named.
#
# Usage: build_extension.sh <dir> <out-dir> <libc.so>
#
# Environment:
#   EXTENSION_KEY     - an SSH Ed25519 private key; signs <name>.meta into
#                       <name>.meta.sig, as `extctl install-signed` takes it
#   CARGO             - cargo command, split on blanks (default: cargo)
#   CARGO_TARGET_DIR  - where the tool is built (default: builddir/target)
set -eu

if [ $# -ne 3 ]; then
    echo "usage: build_extension.sh <dir> <out-dir> <libc.so>" >&2
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

(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$TARGET_DIR" $CARGO build --locked --release --quiet -p slopos-extension)
programs=""
for sub in bin libexec; do
    [ -d "$DIR/$sub" ] || continue
    for file in $(find "$DIR/$sub" -type f | sort); do
        if [ "$(head -c 4 "$file" | od -An -c | tr -d ' ')" = '177ELF' ]; then
            programs="$programs $file"
        fi
    done
done
if [ -n "$programs" ]; then
    # shellcheck disable=SC2086
    "$REPO_ROOT/scripts/check_loose_programs.sh" $programs
fi
report="$("$TARGET_DIR/release/extension" "$DIR" "$OUT" "$LIBC")"
printf '%s\n' "$report"
if [ -n "${EXTENSION_KEY:-}" ]; then
    sidecar="$(printf '%s\n' "$report" | sed -n 's/^sidecar //p')"
    rm -f "$sidecar.sig"
    ssh-keygen -Y sign -q -f "$EXTENSION_KEY" -n extension@slopos -O hashalg=sha512 "$sidecar"
    echo "signed $sidecar.sig with $EXTENSION_KEY"
fi
