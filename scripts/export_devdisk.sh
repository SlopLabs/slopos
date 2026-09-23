#!/usr/bin/env bash
# Carry the guest's edits to SlopOS's source out of the dev disk as a patch,
# or one file — a kernel the guest built — out as itself.
#
# Usage: export_devdisk.sh <image_path> <patch_out>
#        export_devdisk.sh --file <path_on_volume> <image_path> <out>
#
# Diffs the top-level entries of `src/slopos` that its base commit tracks or
# its `.gitignore` keeps (so not `builddir/` or `third_party/`) against the
# commit in `src/slopos/.slopos-base`. `.cargo/config.toml` is left as
# committed: its vendor stanza is the seeding's. Apply with `git apply
# <patch_out>` on the base commit, or `git apply -3` on a later one.
#
# The guest must have shut down: debugfs knows nothing of SlopOS's
# `/.journal`, and a mounted volume reads clean while it idles.
set -euo pipefail

SELF="export_devdisk"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
USAGE="usage: export_devdisk.sh [--file <path_on_volume>] <image_path> <out>"
FILE=""
if [ "${1:-}" = "--file" ]; then
    FILE="${2:?$USAGE}"
    shift 2
fi
IMAGE="${1:?$USAGE}"
OUT="${2:?$USAGE}"

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
TREE="$TMP/slopos"
mkdir -p "$TREE"

# rdump follows a symlink the guest planted beside a same-named entry, which
# only a damaged directory can hold.
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

if [ -n "$FILE" ]; then
    case "$FILE" in /*) ;; *) FILE="/$FILE" ;; esac
    case "$FILE" in *'"'*) die "$FILE: a name with a double quote cannot be dumped" ;; esac
    debugfs_run "stat \"$FILE\"" "$TMP/stat"
    grep -q 'Type: regular' "$TMP/stat" || die "$FILE on $IMAGE is not a regular file"
    debugfs_run "dump \"$FILE\" \"$TMP/file\""
    mv -f "$TMP/file" "$OUT"
    echo "$SELF: wrote $OUT ($(wc -c <"$OUT") bytes) from $FILE"
    exit 0
fi

debugfs -R "dump /src/slopos/.slopos-base \"$TMP/base\"" "$IMAGE" 2>/dev/null
[ -s "$TMP/base" ] ||
    die "$IMAGE carries no src/slopos; a volume built before build_devdisk.sh seeded one has none to export"
BASE="$(cat "$TMP/base")"
git -C "$REPO_ROOT" cat-file -e "$BASE^{commit}" 2>/dev/null ||
    die "the tree was seeded from $BASE, which this checkout does not have"

# Borrows the checkout's objects without writing to it or applying its index or excludes.
OBJECTS="$(git -C "$REPO_ROOT" rev-parse --path-format=absolute --git-common-dir)/objects"
git init -q --bare "$TMP/git"
echo "$OBJECTS" >"$TMP/git/objects/info/alternates"
git_tree() {
    GIT_INDEX_FILE="$TMP/index" git -c core.excludesFile=/dev/null -c core.autocrlf=false \
        --git-dir="$TMP/git" --work-tree="$TREE" "$@"
}

debugfs -R "dump /src/slopos/.gitignore \"$TREE/.gitignore\"" "$IMAGE" 2>/dev/null
debugfs_run "ls -p /src/slopos" "$TMP/listing"
found=0
while IFS=/ read -r _ _ mode _ _ name _; do
    case "$name" in "" | . | .. | .slopos-base) continue ;; esac
    case "$name" in *'"'*) die "src/slopos/$name: a name with a double quote cannot be dumped" ;; esac
    found=1
    path="$name"
    [ "${mode:0:2}" = "04" ] && path="$name/"
    if git_tree check-ignore -q --no-index -- "$path" </dev/null &&
        [ -z "$(git_tree ls-tree --name-only "$BASE" -- "$name")" ]; then
        continue
    fi
    debugfs_run "rdump \"/src/slopos/$name\" \"$TREE\""
done <"$TMP/listing"
[ "$found" -eq 1 ] || die "src/slopos on $IMAGE holds nothing to export"

config="$TREE/.cargo/config.toml"
if [ -L "$TREE/.cargo" ] || [ -L "$config" ] || [ ! -f "$config" ] ||
    ! {
        git -C "$REPO_ROOT" show "$BASE:.cargo/config.toml"
        echo
        git -C "$REPO_ROOT" show "$BASE:.cargo/vendor.toml"
    } | cmp -s - "$config"; then
    echo "$SELF: the guest changed .cargo/config.toml; that change is left out of the patch" >&2
fi

git_tree read-tree "$BASE"
git_tree add -A
git_tree reset -q "$BASE" -- .cargo/config.toml
git_tree diff-index --cached -p --binary "$BASE" >"$OUT"

if [ -s "$OUT" ]; then
    echo "$SELF: $(git_tree diff-index --cached --shortstat "$BASE" | sed "s/^ //") against $BASE"
    echo "$SELF: wrote $OUT — apply it with: git apply $OUT"
else
    echo "$SELF: the guest changed nothing since $BASE"
fi
