# The ownership map every root is held to, as `slopos_abi::fs::ROOT_LAYOUT`
# and `PERSON_TREES` state it; a test in tools/initramfs holds them equal.
# Sourced.

# `<path>:<uid>:<gid>:<mode>`, parents first.
ROOT_LAYOUT=(/:0:0:0755 /etc:0:0:0755 /var:0:0:0755 /var/lib:0:0:0755 /var/lib/slopos:0:0:0755 /var/log:0:0:0755 /var/tmp:0:0:1777 /root:0:0:0700 /home:0:0:0755 /media:0:0:0755 /usr:0:0:0755 /usr/local:0:0:0755 /opt:0:0:0755 /opt/bin:0:0:0755)
PERSON_TREES=(/home/user /src /usr/local)
PERSON_OWNER=1000:1000
OWNERS_RECORD=/var/lib/slopos/owners
# The trees the root's record names, every one on a new root.
HELD_TREES=("${PERSON_TREES[@]}")

# Set HELD_TREES from a record on stdin.
root_layout_read_record() {
    local line tree want named=()
    while IFS= read -r line; do
        line="${line%$'\r'}"
        [ "${line%% *}" = "$PERSON_OWNER" ] || continue
        tree="${line#* }"
        named+=("$tree")
    done
    HELD_TREES=()
    for want in "${PERSON_TREES[@]}"; do
        for tree in "${named[@]}"; do
            if [ "$tree" = "$want" ]; then
                HELD_TREES+=("$want")
                break
            fi
        done
    done
}

root_layout_record() {
    local tree
    for tree in "${HELD_TREES[@]}"; do
        echo "$PERSON_OWNER $tree"
    done
}

root_layout_holds() {
    local tree
    for tree in "${HELD_TREES[@]}"; do
        [ "$tree" != "$1" ] || return 0
    done
    return 1
}

# `uid:gid` of whatever lands at $1: the first person's within a tree they
# hold, uid 0's elsewhere.
root_layout_owner() {
    local tree
    for tree in "${HELD_TREES[@]}"; do
        case "$1" in "$tree" | "$tree"/*)
            echo "$PERSON_OWNER"
            return
            ;;
        esac
    done
    echo 0:0
}
