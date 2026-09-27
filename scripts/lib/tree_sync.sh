# Re-stage a materialised tree without touching what did not change.
#
# A tree here is re-created from a tarball and patched whenever its inputs
# change, and a fresh extraction dates every patched file now: cargo and
# ninja then rebuild everything that includes one, which for the LLVM port is
# most of LLVM and for the compiler fork is every crate above `rustc_target`
# — hours, for a one-line edit elsewhere. Staging into a fresh directory and
# carrying it over the previous one by content keeps the old modification
# time of every file whose bytes are the same, and gives the rest a new one.
#
# Usage, around the code that extracts and patches <dir>:
#   ts_set_aside <dir>              before: the previous tree moves to <dir>.prev
#   ts_carry_over <dir> [rsync-args] after: <dir>.prev takes <dir>'s content
#
# Without rsync the previous tree is dropped and the result is a plain fresh
# extraction, which is correct and only slower to rebuild.

# A `.prev` left by a run that stopped part way is the tree the build
# directory was built from, so it is kept and the half-staged tree dropped.
ts_set_aside() {
    local dir="$1"
    if [ -d "$dir.prev" ]; then
        rm -rf "$dir"
    elif [ -d "$dir" ]; then
        mv "$dir" "$dir.prev"
    fi
}

# Extra arguments go to rsync: an `--exclude=/<path>` keeps the previous
# tree's copy of <path> as it was, for a subtree something else stages.
ts_carry_over() {
    local dir="$1"
    shift
    [ -d "$dir.prev" ] || return 0
    if ! command -v rsync >/dev/null 2>&1; then
        rm -rf "$dir.prev"
        return 0
    fi
    # No `-t`: a file rsync rewrites is dated now, one it skips keeps its
    # time, and `--checksum` makes "skips" mean "same bytes".
    rsync -rlp --checksum --delete "$@" -- "$dir/" "$dir.prev/" &&
        rm -rf "$dir" &&
        mv "$dir.prev" "$dir"
}
