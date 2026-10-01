#!/usr/bin/env bash
# Hold an image SlopOS wrote to what e2fsprogs says a well-formed ext4
# filesystem is.
#
# This is the one oracle for on-disk correctness that SlopOS does not have to
# write itself. e2fsprogs is already a hard build dependency (`mke2fs` and
# `debugfs` build every image), so the check costs nothing new, and an image
# SlopOS wrote that `e2fsck` rejects is a bug in SlopOS.
#
# Two assertions, because `e2fsck -fn` exits 0 on a sound filesystem whose
# superblock says it is in use or whose journal still holds transactions: that
# is state, not damage, yet it is what says whether the last boot shut down in
# order:
#
#   1. `e2fsck -fn` exits 0 — no structural inconsistency.
#   2. The volume is at rest: `Filesystem state: clean`, no `needs_recovery`,
#      an empty journal — a boot that mounted this image also ran the final
#      sync and `mark_clean`.
#
# A run that panicked deliberately leaves the image dirty, so a failure here
# after a panicking boot is the gate reporting the truth rather than a false
# positive.
#
# The verity trailer needs no special handling: `e2fsck` objects when the
# superblock claims *more* blocks than the device holds, never fewer, and an
# image with 16 KiB of trailer appended checks clean.
#
#     scripts/check_fs_image.sh [image]
#     scripts/check_fs_image.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/ext4.sh
. "$SCRIPT_DIR/lib/ext4.sh"

DEFAULT_IMAGE="${REPO_ROOT}/fs/assets/ext2-tests.img"

# macOS: e2fsprogs is keg-only under Homebrew, as in build_fs_image.sh.
if [ "$(uname -s)" = "Darwin" ]; then
    BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
    export PATH="${BREW_PREFIX}/opt/e2fsprogs/sbin:${BREW_PREFIX}/opt/e2fsprogs/bin:${PATH}"
fi

SELF_TEST=0
IMAGE=""
case "${1:-}" in
    --self-test) SELF_TEST=1 ;;
    "")          IMAGE="$DEFAULT_IMAGE" ;;
    -*)          echo "usage: check_fs_image.sh [--self-test] [image]" >&2; exit 2 ;;
    *)           IMAGE="$1" ;;
esac

for tool in e2fsck dumpe2fs; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "check_fs_image: $tool is required (install e2fsprogs)" >&2
        exit 2
    fi
done

# ---------------------------------------------------------------------------
# The check. Exit 0 clean, 1 rejected, 2 nothing to check.
# ---------------------------------------------------------------------------
check_image() {
    local image="$1" quiet="${2:-0}"

    # Refusing to report OK on an absent image is the point: a recipe that ran
    # before the image was built must fail, not pass vacuously.
    if [ ! -f "$image" ]; then
        [ "$quiet" = 1 ] || echo "check_fs_image: no image at $image — nothing to check," >&2
        [ "$quiet" = 1 ] || echo "  so refusing to report OK. Build one with 'just _fs-image-tests'." >&2
        return 2
    fi
    if [ ! -s "$image" ]; then
        [ "$quiet" = 1 ] || echo "check_fs_image: $image is empty" >&2
        return 2
    fi

    local fsck_out fsck_rc=0
    fsck_out="$(e2fsck -fn "$image" 2>&1)" || fsck_rc=$?
    if [ "$fsck_rc" -ne 0 ]; then
        if [ "$quiet" != 1 ]; then
            echo "check_fs_image: e2fsck rejected $image (exit $fsck_rc):" >&2
            echo "$fsck_out" | sed 's/^/    /' >&2
            echo "  An image SlopOS wrote that e2fsck rejects is a bug in SlopOS." >&2
        fi
        return 1
    fi

    local unrest
    unrest="$(ext4_unrest "$image")"
    if [ "$unrest" = "unreadable" ]; then
        [ "$quiet" = 1 ] || echo "check_fs_image: dumpe2fs reported no filesystem state for $image" >&2
        return 2
    fi
    if [ -n "$unrest" ]; then
        if [ "$quiet" != 1 ]; then
            echo "check_fs_image: $image is structurally sound but not at rest ($unrest)." >&2
            echo "  The last boot to mount it never reached the final sync, so mark_clean" >&2
            echo "  never ran — a write it reported as durable may only be in its journal." >&2
        fi
        return 1
    fi

    [ "$quiet" = 1 ] || echo "check_fs_image: OK — $image passes e2fsck -fn and is at rest"
    return 0
}

# ---------------------------------------------------------------------------
# Self-test. Both directions: the forms the gate must reject, and the form it
# must accept. A check that has never been observed to reject has not been
# observed to work.
# ---------------------------------------------------------------------------
if [ "$SELF_TEST" = 1 ]; then
    if ! command -v mke2fs >/dev/null 2>&1 || ! command -v debugfs >/dev/null 2>&1; then
        echo "check_fs_image --self-test: mke2fs and debugfs are required" >&2
        exit 2
    fi

    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    failures=0

    expect() {
        local want="$1" label="$2" image="$3" got=0
        check_image "$image" 1 || got=$?
        if [ "$got" = "$want" ]; then
            echo "  ok    $label (exit $got)"
        else
            echo "  FAIL  $label — expected exit $want, got $got" >&2
            failures=$((failures + 1))
        fi
    }

    ext4_mkfs_args
    truncate -s 32M "$tmp/clean.img"
    mke2fs -F -q "${EXT4_MKFS_ARGS[@]}" "$tmp/clean.img" >/dev/null 2>&1
    expect 0 "a freshly built image passes" "$tmp/clean.img"

    # Sound but not at rest: the states e2fsck's exit code alone passes.
    cp "$tmp/clean.img" "$tmp/dirty.img"
    debugfs -w -R "ssv state 0" "$tmp/dirty.img" >/dev/null 2>&1
    expect 1 "an s_state other than clean is rejected" "$tmp/dirty.img"

    cp "$tmp/clean.img" "$tmp/live.img"
    printf 'jo -c\njw -b 7000 /dev/zero\njc\n' | debugfs -w -f - "$tmp/live.img" >/dev/null 2>&1
    expect 1 "a journal still needing recovery is rejected" "$tmp/live.img"

    cp "$tmp/clean.img" "$tmp/flagged.img"
    debugfs -w -R "feature needs_recovery" "$tmp/flagged.img" >/dev/null 2>&1
    expect 1 "needs_recovery over an empty journal is rejected" "$tmp/flagged.img"

    cp "$tmp/live.img" "$tmp/unflagged.img"
    debugfs -w -R "feature -needs_recovery" "$tmp/unflagged.img" >/dev/null 2>&1
    expect 1 "a live journal under a clear flag is rejected" "$tmp/unflagged.img"

    # Damaged metadata: the e2fsck arm.
    cp "$tmp/clean.img" "$tmp/corrupt.img"
    itable="$(dumpe2fs "$tmp/clean.img" 2>/dev/null | sed -n 's/^ *Inode table at \([0-9]*\)-.*/\1/p' | head -n 1)"
    dd if=/dev/urandom of="$tmp/corrupt.img" bs=4096 seek="$itable" count=4 \
        conv=notrunc status=none
    expect 1 "a corrupted inode table is rejected" "$tmp/corrupt.img"

    expect 2 "a missing image is refused, not reported OK" "$tmp/absent.img"

    : > "$tmp/empty.img"
    expect 2 "an empty image is refused, not reported OK" "$tmp/empty.img"

    if [ "$failures" -ne 0 ]; then
        echo "check_fs_image --self-test: $failures check(s) failed" >&2
        exit 1
    fi
    echo "check_fs_image: self-test OK — 8 checks, both directions"
    exit 0
fi

check_image "$IMAGE"
