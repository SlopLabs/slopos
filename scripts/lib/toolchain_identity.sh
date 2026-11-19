#!/usr/bin/env bash
# The identity of the toolchain a tree needs, which `just toolchain` keeps as
# its image's release `display` and a userland build holds the toolchain to.
# Apart from `toolchain_pin.sh`, which both materialised trees stamp: a change
# here restamps neither. Sourced after it.

# Every fork and pin a toolchain is built from: std and libc, the compiler,
# its crate ports, the two LLVM ports and the C++ runtime, the recipes, and
# the settings the compiler is built with. 32 digits, as many as a `display`
# holds.
tp_toolchain_identity() {
    (cd "$1" && find "$TP_PIN_REL" "$TP_OVERLAY_REL/rust" "$TP_OVERLAY_REL/libc" \
        "$TP_COMPILER_OVERLAY_REL" "$TP_CRATES_OVERLAY_REL" "$TP_LLVM_OVERLAY_REL" \
        "$TP_LLVM_RUSTC_OVERLAY_REL" toolchain/cxx toolchain/recipes \
        scripts/lib/rustc_build_settings.sh -type f -print) |
        tp_hash_lines "$1" | tp_sha256_stream | cut -c1-32
}

# Hold a toolchain image's sysroot (`$2`) to the identity the tree at `$1`
# needs. The host's owned sysroot, which `make_slopos_sysroot.sh` stamps,
# passes; anything else carries no identity to hold.
tp_hold_toolchain() {
    local release="$2/.package/release" have="" want key value
    want="$(tp_toolchain_identity "$1")"
    if [ ! -f "$release" ]; then
        [ ! -f "$2/$TP_STAMP_NAME" ] || return 0
        echo "$2 is no toolchain image; this tree needs $want, which just toolchain builds on the host" >&2
        return 1
    fi
    while read -r key value; do
        [ "$key" != display ] || have="$value"
    done <"$release"
    [ "$have" = "$want" ] && return 0
    echo "the toolchain at $2 is ${have:-unnamed}; this tree needs $want, which just toolchain builds on the host" >&2
    return 1
}
