#!/usr/bin/env bash
# Shared plumbing for the pinned `slopos` toolchain fork.
#
# Two scripts must agree, byte for byte, on what the pin *is*:
# `make_slopos_sysroot.sh` materialises the sysroot and writes the stamp, and
# `check_toolchain_pin.sh` re-derives the same stamp and fails when it has
# drifted. Duplicating the hash definition in both is how a stamp that never
# goes stale gets shipped, so it lives here once.
#
# Portability matches scripts/lib/gate_common.sh: bash 3.2, no `mapfile`, no
# `declare -A`, no GNU-only `realpath`.

# The materialised sysroot, the rustup name it is linked under, and the stamp
# inside it. Repo-relative so the self-test can point them at a scratch tree.
TP_SYSROOT_REL="third_party/rust-slopos"
TP_TOOLCHAIN_NAME="slopos"
TP_STAMP_NAME=".slopos-stamp"
TP_OVERLAY_REL="toolchain"
TP_PIN_REL="toolchain/PIN"
# Where `-Zbuild-std` reads the standard library from inside a sysroot, and
# therefore the directory the rust patch applies in; the libc patch applies in
# its `libc` subdirectory.
TP_LIBRARY_REL="lib/rustlib/src/rust/library"

tp_sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "toolchain_pin: need sha256sum or shasum" >&2
        exit 2
    fi
}

tp_sha256_stream() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 | awk '{ print $1 }'
    else
        echo "toolchain_pin: need sha256sum or shasum" >&2
        exit 2
    fi
}

# Absolute, symlink-resolved where the path exists; lexical otherwise, so a
# link planted at a directory that was since removed still compares.
tp_abspath() {
    local p="$1" d b
    if [ -d "$p" ]; then
        (cd "$p" && pwd -P)
        return
    fi
    d="$(dirname "$p")"
    b="$(basename "$p")"
    if [ -d "$d" ]; then
        printf '%s/%s\n' "$(cd "$d" && pwd -P)" "$b"
    else
        printf '%s\n' "$p"
    fi
}

# The channel `rust-toolchain.toml` pins, e.g. `nightly-2026-09-03`.
tp_channel() {
    sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$1/rust-toolchain.toml"
}

# `toolchain/PIN` is `key=value`, one per line, `#` comments and blank lines
# ignored. Repeated keys are legal — `patch_sha256` carries one line per patch
# file — so single-valued lookups take the first and say so.
tp_pin_value() {
    local pin="$1" key="$2"
    sed -n "s/^${key}=\\(.*\\)\$/\\1/p" "$pin" | head -n 1
}

tp_pin_value_required() {
    local pin="$1" key="$2" who="$3" value
    value="$(tp_pin_value "$pin" "$key")"
    if [ -z "$value" ]; then
        echo "$who: $pin is missing the required \`${key}=\` line" >&2
        exit 1
    fi
    printf '%s\n' "$value"
}

# Every `patch_sha256=<repo-relative path>:<sha256>` line, as `path sha256`.
tp_pin_patches() {
    sed -n 's/^patch_sha256=\(.*\):\([0-9a-fA-F]\{64\}\)[[:space:]]*$/\1 \2/p' "$1"
}

tp_pin_patch_sha() {
    local pin="$1" rel="$2"
    tp_pin_patches "$pin" | awk -v want="$rel" '$1 == want { print $2; exit }'
}

# Every patch file the overlay carries, repo-relative and sorted. Both halves
# of the fork live here: `toolchain/rust/` patches the std source tree,
# `toolchain/libc/` patches the unpacked libc crate.
tp_patch_files() {
    local root="$1"
    (cd "$root" && find "$TP_OVERLAY_REL" -type f -name '*.patch' -print | LC_ALL=C sort)
}

# The files a patch *creates*, one per line, `-p1`-stripped so each is
# relative to the directory the patch is applied in. Read as: the cheapest
# observable evidence that a patch really landed in a materialised tree. A
# `git diff` announces a creation as a hunk whose source is `/dev/null`.
tp_patch_new_files() {
    awk '
        /^--- / { from = $2; next }
        /^\+\+\+ / {
            if (from == "/dev/null" && $2 != "/dev/null") {
                path = $2
                sub(/^[^\/]*\//, "", path)
                if (path != "") print path
            }
            from = ""
        }
    ' "$1"
}

# The stamp: one sha256 over every file in the overlay — `toolchain/PIN`
# included, since it lives there — keyed by path so a rename is a change. This
# is what `.slopos-stamp` records and what makes re-materialising idempotent.
tp_stamp() {
    local root="$1"
    (
        cd "$root" || exit 1
        find "$TP_OVERLAY_REL" -type f -print | LC_ALL=C sort | while IFS= read -r path; do
            printf '%s  %s\n' "$(tp_sha256_file "$path")" "$path"
        done
    ) | tp_sha256_stream
}

# `rustup toolchain link` is a symlink under $RUSTUP_HOME/toolchains on unix,
# so the registration can be read without paying a rustup invocation — and
# planted by a self-test that owns its own RUSTUP_HOME.
tp_link_path() {
    printf '%s/toolchains/%s\n' "${RUSTUP_HOME:-$HOME/.rustup}" "$TP_TOOLCHAIN_NAME"
}

# Where the linked toolchain points, or empty when nothing is linked.
tp_link_target() {
    local link raw
    link="$(tp_link_path)"
    if [ -L "$link" ]; then
        raw="$(readlink "$link")"
        case "$raw" in
            /*) ;;
            *) raw="$(dirname "$link")/$raw" ;;
        esac
        tp_abspath "$raw"
    elif [ -d "$link" ]; then
        tp_abspath "$link"
    fi
}
