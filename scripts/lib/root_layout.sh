# The ownership map every root is held to, as `slopos_abi::fs::ROOT_LAYOUT`
# and `SESSION_TREES` state it; a test in tools/initramfs holds them equal.
# Sourced.

# `<path>:<uid>:<gid>:<mode>`, parents first.
ROOT_LAYOUT=(/:0:0:0755 /etc:0:0:0755 /var:0:0:0755 /var/lib:0:0:0755 /var/lib/slopos:0:0:0755 /var/log:0:0:0755 /var/tmp:0:0:1777 /root:0:0:0700 /home:0:0:0755 /home/user:1000:1000:0755 /media:0:0:0755 /usr:0:0:0755 /usr/local:1000:1000:0755 /opt:0:0:0755 /opt/bin:0:0:0755)
SESSION_TREES=(/home /src /usr/local)
SESSION_OWNER=1000:1000
OWNERS_RECORD=/var/lib/slopos/owners

# What init writes once a root's session trees are the session's, which a root
# built with the map holds from the start.
root_layout_record() {
    local tree
    for tree in "${SESSION_TREES[@]}"; do
        echo "$SESSION_OWNER $tree"
    done
}

# `uid:gid` of whatever lands at $1: the map's for one of its directories, the
# session's below a session tree, uid 0's elsewhere.
root_layout_owner() {
    local spec tree
    for spec in "${ROOT_LAYOUT[@]}"; do
        if [ "${spec%%:*}" = "$1" ]; then
            spec="${spec#*:}"
            echo "${spec%:*}"
            return
        fi
    done
    for tree in "${SESSION_TREES[@]}"; do
        case "$1" in "$tree" | "$tree"/*)
            echo "$SESSION_OWNER"
            return
            ;;
        esac
    done
    echo 0:0
}
