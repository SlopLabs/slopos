# The volume SlopOS formats, as `ext4-core/profile` states it, and whether one
# is at rest. Sourced.

EXT4_PROFILE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/ext4-core/profile"

ext4_profile_value() {
    sed -n "s/^$1=//p" "$EXT4_PROFILE"
}

# `mke2fs` arguments for a profile volume, into the array EXT4_MKFS_ARGS.
# `-O none` first, so nothing the host's mke2fs.conf adds survives.
ext4_mkfs_args() {
    EXT4_MKFS_ARGS=(-t ext4 -O none -O "$(ext4_profile_value features)"
        -I "$(ext4_profile_value inode_size)" -b "$(ext4_profile_value block_size)")
}

# Why the volume in $1 is not at rest (`s_state` clean, nothing to replay), or
# nothing when it is. `e2fsck -fn` exits 0 over a volume that needs recovery.
ext4_unrest() {
    local header state
    header="$(dumpe2fs -h "$1" 2>/dev/null)" || { echo "unreadable"; return; }
    state="$(echo "$header" | sed -n 's/^Filesystem state:[[:space:]]*//p' | head -n 1)"
    if [ -z "$state" ]; then
        echo "unreadable"
    elif [ "$state" != "clean" ]; then
        echo "state $state"
    elif echo "$header" | grep -q '^Filesystem features:.*\bneeds_recovery\b'; then
        echo "needs_recovery"
    elif echo "$header" | grep -Eq '^Journal start:[[:space:]]*[1-9]'; then
        echo "journal holds transactions"
    fi
}

# Whether the volume in $1 has the profile's inode size and every profile
# feature but those named after it.
ext4_meets_profile() {
    local image="$1" header have want
    shift
    header="$(dumpe2fs -h "$image" 2>/dev/null)" || return 1
    have=" $(echo "$header" | sed -n 's/^Filesystem features:[[:space:]]*//p') "
    for want in $(ext4_profile_value features | tr ',' ' '); do
        case " $* " in *" $want "*) continue ;; esac
        case "$have" in *" $want "*) ;; *) return 1 ;; esac
    done
    [ "$(echo "$header" | sed -n 's/^Inode size:[[:space:]]*//p')" = "$(ext4_profile_value inode_size)" ]
}

ext4_has_feature() {
    case " $(dumpe2fs -h "$1" 2>/dev/null | sed -n 's/^Filesystem features:[[:space:]]*//p') " in
        *" $2 "*) ;;
        *) return 1 ;;
    esac
}

ext4_ext2_era() {
    ! ext4_has_feature "$1" extent && ! ext4_has_feature "$1" has_journal
}
