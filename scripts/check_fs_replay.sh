#!/usr/bin/env bash
# Hold the host's e2fsck to replaying what a SlopOS boot committed before the
# machine died under it, as `just test-rude-exit` leaves the tests image.
#
# Usage: check_fs_replay.sh <image> <path> <payload file>
#        check_fs_replay.sh --self-test
#
# Four assertions, on a copy of the image:
#   1. It is not at rest: it needs recovery and its journal holds a
#      transaction. A boot that ended cleanly proves nothing here.
#   2. <path> is not reachable through the home locations, so what reaches it
#      lives only in the journal.
#   3. `e2fsck -E journal_only` replays the journal, after which `e2fsck -fn`
#      passes and the image is at rest.
#   4. <path> then holds exactly the payload.
#
# Exit codes: 0 replayed, 1 an assertion failed, 2 the inputs were unusable.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/ext4.sh
. "$SCRIPT_DIR/lib/ext4.sh"

QUIET=0

say() {
    [ "$QUIET" = 1 ] || echo "check_fs_replay: $*" >&2
}

check_replay() {
    local image="$1" path="$2" payload="$3" work header unrest got rc=0
    if [ ! -s "$image" ] || [ ! -f "$payload" ]; then
        say "$image or $payload is missing"
        return 2
    fi
    work="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$work'" RETURN
    cp --sparse=always "$image" "$work/image"

    header="$(dumpe2fs -h "$work/image" 2>/dev/null)" || {
        say "$image carries no readable superblock"
        return 2
    }
    if [ -z "$(ext4_unrest "$work/image")" ]; then
        say "$image is at rest: the boot ended cleanly, so there is nothing to replay"
        return 1
    fi
    # The flag reaches the medium before the journal goes live, so a live
    # journal under a clear flag is the ordering broken.
    if ! echo "$header" | grep -q '^Filesystem features:.*\bneeds_recovery\b' ||
       ! echo "$header" | grep -Eq '^Journal start:[[:space:]]*[1-9]'; then
        say "$image is not a volume that needs recovery over a live journal"
        return 1
    fi
    if debugfs -R "stat \"$path\"" "$work/image" 2>/dev/null | grep -q '^Inode:'; then
        say "$path is reachable before replay, so the journal is not what carries it"
        return 1
    fi

    e2fsck -E journal_only -y "$work/image" >"$work/replay.log" 2>&1 || rc=$?
    if [ "$rc" -ge 4 ]; then
        say "e2fsck could not replay the journal (exit $rc):"
        [ "$QUIET" = 1 ] || tail -n 20 "$work/replay.log" >&2
        return 1
    fi
    if ! e2fsck -fn "$work/image" >"$work/check.log" 2>&1; then
        say "the replayed image does not pass e2fsck -fn:"
        [ "$QUIET" = 1 ] || tail -n 20 "$work/check.log" >&2
        return 1
    fi
    unrest="$(ext4_unrest "$work/image")"
    if [ -n "$unrest" ]; then
        say "the replayed image is not at rest ($unrest)"
        return 1
    fi
    debugfs -R "dump \"$path\" \"$work/got\"" "$work/image" >/dev/null 2>&1 || true
    if [ ! -f "$work/got" ] || ! cmp -s "$work/got" "$payload"; then
        got="$( [ -f "$work/got" ] && wc -c <"$work/got" || echo absent)"
        say "$path after replay is not the payload ($got bytes)"
        return 1
    fi
    say "OK — e2fsck replayed $image's journal and $path holds the payload"
}

self_test() {
    local tmp pass=0 fail=0
    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '$tmp'" EXIT
    for tool in mke2fs debugfs e2fsck python3; do
        command -v "$tool" >/dev/null 2>&1 ||
            { echo "check_fs_replay --self-test: $tool is required" >&2; exit 2; }
    done
    QUIET=1

    ext4_mkfs_args
    truncate -s 16M "$tmp/base.img"
    mke2fs -F -q "${EXT4_MKFS_ARGS[@]}" "$tmp/base.img" >/dev/null 2>&1
    printf 'slopos-rude-exit-v1\n' >"$tmp/payload"
    printf 'something else\n' >"$tmp/other"
    cp "$tmp/base.img" "$tmp/home.img"
    debugfs -w -R "write \"$tmp/payload\" rude-exit" "$tmp/home.img" >/dev/null 2>&1

    cp "$tmp/home.img" "$tmp/stale.img"
    debugfs -w -R "ssv state 0" "$tmp/stale.img" >/dev/null 2>&1
    cp "$tmp/home.img" "$tmp/bad.img"
    debugfs -w -R "sif /lost+found links_count 7" "$tmp/bad.img" >/dev/null 2>&1

    # Each fixture journals, onto an image without the file, the blocks its
    # creation changed. Block 0 is left out, as debugfs rewrites the superblock
    # itself, except in `unrest`, which carries an unclean one.
    python3 - "$tmp" <<'EOF'
import sys
tmp = sys.argv[1]
bs = 4096
image = lambda name: open(f"{tmp}/{name}.img", "rb").read()
base, home, bad, stale = image("base"), image("home"), image("bad"), image("stale")
block = lambda img, i: img[i * bs:(i + 1) * bs]
def differ(img):
    return [i for i in range(1, len(base) // bs) if block(base, i) != block(img, i)]
def put(name, blocks, data):
    open(f"{tmp}/{name}", "w").write(",".join(map(str, blocks)))
    open(f"{tmp}/{name}.bin", "wb").write(b"".join(data(i) for i in blocks))
put("blocks", differ(home), lambda i: block(home, i))
put("damaging", differ(bad), lambda i: block(bad, i))
put("unrest", [0] + differ(home), lambda i: block(stale if i == 0 else home, i))
EOF
    journal() {
        cp "$1" "$2"
        printf 'jo -c\njw -b %s "%s"\njc\n' "$(cat "$tmp/$3")" "$tmp/$3.bin" |
            debugfs -w -f - "$2" >/dev/null 2>&1
    }
    journal "$tmp/base.img" "$tmp/live.img" blocks
    journal "$tmp/home.img" "$tmp/early.img" blocks
    journal "$tmp/base.img" "$tmp/damaging.img" damaging
    journal "$tmp/base.img" "$tmp/unrest.img" unrest
    cp "$tmp/live.img" "$tmp/unflagged.img"
    debugfs -w -R "feature -needs_recovery" "$tmp/unflagged.img" >/dev/null 2>&1

    expect() {
        local want="$1" what="$2" rc=0
        shift 2
        check_replay "$@" || rc=$?
        if [ "$rc" = "$want" ]; then
            echo "  ok    $what (exit $rc)"
            pass=$((pass + 1))
        else
            echo "  FAIL  $what: exit $rc, want $want" >&2
            fail=$((fail + 1))
        fi
    }
    expect 0 "a transaction only the journal holds replays" "$tmp/live.img" /rude-exit "$tmp/payload"
    expect 1 "an image at rest has nothing to replay" "$tmp/base.img" /rude-exit "$tmp/payload"
    expect 1 "a file already home proves nothing about the journal" "$tmp/early.img" /rude-exit "$tmp/payload"
    expect 1 "a replay that lands other bytes is refused" "$tmp/live.img" /rude-exit "$tmp/other"
    expect 1 "a live journal under a clear flag is refused" "$tmp/unflagged.img" /rude-exit "$tmp/payload"
    expect 1 "a replay that leaves damage is refused" "$tmp/damaging.img" /rude-exit "$tmp/payload"
    expect 1 "a replay that leaves the volume in use is refused" "$tmp/unrest.img" /rude-exit "$tmp/payload"
    expect 2 "a missing image is refused" "$tmp/absent.img" /rude-exit "$tmp/payload"

    if [ "$fail" -ne 0 ]; then
        echo "check_fs_replay: self-test FAILED — $fail of $((pass + fail)) checks" >&2
        exit 1
    fi
    echo "check_fs_replay: self-test OK — $pass checks, both directions"
}

if [ "${1:-}" = "--self-test" ]; then
    self_test
    exit 0
fi
[ "$#" -eq 3 ] || { echo "usage: check_fs_replay.sh <image> <path> <payload file> | --self-test" >&2; exit 2; }
check_replay "$1" "$2" "$3"
