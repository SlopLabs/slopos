#!/usr/bin/env bash
# Copy one file — a kernel the guest built — off the dev disk.
#
# Usage: export_devdisk.sh <path_on_volume> <image_path> <out>
#
# The guest must have shut down: debugfs knows nothing of SlopOS's
# `/.journal`, and a mounted volume reads clean while it idles. Source moves
# between the machines by git, not through here.
set -euo pipefail

SELF="export_devdisk"
USAGE="usage: export_devdisk.sh <path_on_volume> <image_path> <out>"
FILE="${1:?$USAGE}"
IMAGE="${2:?$USAGE}"
OUT="${3:?$USAGE}"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

[ -f "$IMAGE" ] || die "$IMAGE does not exist"
command -v debugfs >/dev/null && command -v dumpe2fs >/dev/null && command -v e2fsck >/dev/null ||
    die "debugfs, dumpe2fs and e2fsck (e2fsprogs) are not installed"
state="$({ dumpe2fs -h "$IMAGE" 2>/dev/null || true; } | sed -n 's/^Filesystem state:[[:space:]]*//p')"
[ "$state" = "clean" ] ||
    die "$IMAGE is not clean (state: ${state:-unreadable}); boot it once so its log replays, shut the guest down, then export"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

e2fsck -fn "$IMAGE" >"$TMP/e2fsck.log" 2>&1 ||
    die "$IMAGE does not pass e2fsck -fn, so nothing is read from it:
$(tail -n 20 "$TMP/e2fsck.log")"

# debugfs exits 0 whatever it prints. A failed chown is the one error that
# loses nothing: a non-root host user gets the guest's bytes without its uid.
debugfs_run() {
    debugfs -R "$1" "$IMAGE" >"${2:-/dev/null}" 2>"$TMP/debugfs.log"
    if grep -v -e '^debugfs [0-9]' -e 'while changing ownership of' "$TMP/debugfs.log" >/dev/null; then
        die "debugfs could not $1:
$(grep -v '^debugfs [0-9]' "$TMP/debugfs.log")"
    fi
}

case "$FILE" in /*) ;; *) FILE="/$FILE" ;; esac
case "$FILE" in *'"'*) die "$FILE: a name with a double quote cannot be dumped" ;; esac
debugfs_run "stat \"$FILE\"" "$TMP/stat"
grep -q 'Type: regular' "$TMP/stat" || die "$FILE on $IMAGE is not a regular file"
debugfs_run "dump \"$FILE\" \"$TMP/file\""
mv -f "$TMP/file" "$OUT"
echo "$SELF: wrote $OUT ($(wc -c <"$OUT") bytes) from $FILE"
