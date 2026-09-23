#!/usr/bin/env bash
# Shared plumbing for the pinned `slopos` toolchain fork.
#
# Four scripts must agree, byte for byte, on what a pin *is*:
# `make_slopos_sysroot.sh` and `make_rustc_src.sh` materialise a tree each and
# write its stamp; `check_toolchain_pin.sh` and `check_rustc_target.sh`
# re-derive those stamps and fail when one has drifted. Duplicating a hash
# definition across four callers is how a stamp that never goes stale gets
# shipped, so they live here once.
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
# The compiler fork is a second materialised tree with a second stamp: the
# sysroot is a clone of a *built* toolchain, so a patch to rustc's own sources
# has nowhere to land in it, and re-cloning the sysroot for a compiler patch
# would be work for nothing.
TP_COMPILER_OVERLAY_REL="toolchain/compiler"
TP_COMPILER_PIN_REL="toolchain/compiler/PIN"

# The cargo fork lands inside the compiler fork's tree, at `src/tools/cargo`,
# because the rustc source tarball already carries cargo's sources.
TP_CARGO_OVERLAY_REL="toolchain/cargo"
TP_CARGO_PIN_REL="toolchain/cargo/PIN"
TP_CARGO_TREE_REL="src/tools/cargo"

# The llvm-project fork: a fourth tree, pinned beside the tarball it patches.
TP_LLVM_OVERLAY_REL="toolchain/llvm"
TP_LLVM_PIN_REL="toolchain/cxx/PIN"
TP_RUSTC_SRC_REL="third_party/slopos-rustc-src"

# The same port, in rustc's own bundled LLVM — a different version from the
# runtime's, which is why it is a second tree. See toolchain/compiler/PIN.
TP_LLVM_RUSTC_OVERLAY_REL="toolchain/llvm-rustc"
TP_LLVM_RUSTC_TREE_REL="src/llvm-project"

# The crate ports: crates.io crates the compiler and cargo depend on, each
# unpacked from its pinned `.crate` into `slopos-crates/<name>-<version>` in
# the rustc source tree and patched there, beside a copy of the libc fork at
# `slopos-crates/libc`. `wiring/` is the `[patch.crates-io]` that points both
# workspaces at them, and applies at the tree's root.
TP_CRATES_OVERLAY_REL="toolchain/crates"
TP_CRATES_PIN_REL="toolchain/crates/PIN"
TP_CRATES_WIRING_REL="toolchain/crates/wiring"
TP_CRATES_TREE_REL="slopos-crates"

# Print the vendored-sources directory named by `$1/.cargo/vendor.toml`, repo-relative.
tp_vendor_rel() {
    local rel
    rel="$(sed -n 's/^directory[[:space:]]*=[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p' "$1/.cargo/vendor.toml" 2>/dev/null | head -n 1)"
    [ -n "$rel" ] || return 1
    printf '%s\n' "$rel"
}

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

# Every `crate=<name> <version> <sha256>` line of the crate-ports PIN, as
# `name version sha256`: the `.crate` each port is cut against.
tp_pin_crates() {
    sed -n 's/^crate=\([^ ]*\) \([^ ]*\) \([0-9a-fA-F]\{64\}\)[[:space:]]*$/\1 \2 \3/p' "$1"
}

tp_pin_files() {
    printf '%s\n%s\n%s\n%s\n' "$TP_PIN_REL" "$TP_COMPILER_PIN_REL" "$TP_CARGO_PIN_REL" \
        "$TP_CRATES_PIN_REL"
}

# Each fork is pinned beside the tree it is applied to, so that editing one
# does not restamp another: the compiler fork's own PIN, the cargo fork's
# own, the crate ports' own, the llvm-project fork's beside the tarball both
# it and the C++ runtime are cut from, and the std and libc forks in the
# sysroot's.
tp_patch_pin_file() {
    case "$1" in
        "$TP_COMPILER_OVERLAY_REL/"*) printf '%s\n' "$TP_COMPILER_PIN_REL" ;;
        "$TP_CARGO_OVERLAY_REL/"*) printf '%s\n' "$TP_CARGO_PIN_REL" ;;
        "$TP_CRATES_OVERLAY_REL/"*) printf '%s\n' "$TP_CRATES_PIN_REL" ;;
        "$TP_LLVM_RUSTC_OVERLAY_REL/"*) printf '%s\n' "$TP_COMPILER_PIN_REL" ;;
        "$TP_LLVM_OVERLAY_REL/"*) printf '%s\n' "$TP_LLVM_PIN_REL" ;;
        *) printf '%s\n' "$TP_PIN_REL" ;;
    esac
}

# All seven forks live here: `toolchain/rust/` patches the std source tree,
# `toolchain/libc/` the unpacked libc crate, `toolchain/compiler/` rustc's own
# sources, `toolchain/cargo/` cargo's inside them, `toolchain/crates/` the
# crates.io crates both of those depend on, `toolchain/llvm/` the pinned
# llvm-project the C++ runtime is built from, and `toolchain/llvm-rustc/` the
# one rustc ships.
tp_patch_files() {
    local root="$1"
    (cd "$root" && find "$TP_OVERLAY_REL" -type f -name '*.patch' -print | LC_ALL=C sort)
}

tp_patch_tree_rel() {
    case "$1" in
        "$TP_COMPILER_OVERLAY_REL/"* | "$TP_CARGO_OVERLAY_REL/"* | "$TP_LLVM_RUSTC_OVERLAY_REL/"* | \
            "$TP_CRATES_OVERLAY_REL/"*)
            printf '%s\n' "$TP_RUSTC_SRC_REL"
            ;;
        # The llvm fork's tree is `third_party/llvm-project-<version>.src`,
        # whose name this file has no version to spell, so `check_cxx_pin.sh`
        # grades it instead. Answering the overlay's own path matches none of
        # the trees the materialisation check walks, which is the point.
        "$TP_LLVM_OVERLAY_REL/"*) printf '%s\n' "$TP_LLVM_OVERLAY_REL" ;;
        *) printf '%s\n' "$TP_SYSROOT_REL" ;;
    esac
}

# The std and libc forks describe a `library/` tree and there are three of
# those — the owned sysroot's, the rustc source tree's, which a bootstrap run
# builds the target's std out of, and the crate ports' directory, whose libc
# the compiler and cargo resolve — so the second argument names which one.
tp_patch_apply_dir() {
    local rel="$1" library="${2:-$TP_SYSROOT_REL/$TP_LIBRARY_REL}"
    case "$rel" in
        "$TP_OVERLAY_REL/libc/"*) printf '%s/libc\n' "$library" ;;
        "$TP_COMPILER_OVERLAY_REL/"*) printf '%s\n' "$TP_RUSTC_SRC_REL" ;;
        "$TP_CARGO_OVERLAY_REL/"*) printf '%s/%s\n' "$TP_RUSTC_SRC_REL" "$TP_CARGO_TREE_REL" ;;
        "$TP_CRATES_WIRING_REL/"*) printf '%s\n' "$TP_RUSTC_SRC_REL" ;;
        "$TP_CRATES_OVERLAY_REL/"*)
            printf '%s/%s/%s\n' "$TP_RUSTC_SRC_REL" "$TP_CRATES_TREE_REL" "$(basename "$rel" .patch)"
            ;;
        "$TP_LLVM_RUSTC_OVERLAY_REL/"*) printf '%s/%s\n' "$TP_RUSTC_SRC_REL" "$TP_LLVM_RUSTC_TREE_REL" ;;
        "$TP_LLVM_OVERLAY_REL/"*)
            echo "toolchain_pin: $rel is applied by make_slopos_llvm_src.sh, not here" >&2
            return 1
            ;;
        *) printf '%s\n' "$library" ;;
    esac
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

# A materialiser is an input to the tree it builds: its unpack rules, and this
# library's patch-placement rules, decide what ends up in one, so a tree built
# under the previous set must not be stamped current. Absent under a scratch
# root, which carries an overlay and no scripts.
tp_materialiser_hash() {
    if [ -f "$1/scripts/$2" ]; then
        printf '%s  scripts/%s\n' "$(tp_sha256_file "$1/scripts/$2")" "$2"
    fi
}

tp_hash_lines() {
    local root="$1"
    (
        cd "$root" || exit 1
        LC_ALL=C sort | while IFS= read -r path; do
            printf '%s  %s\n' "$(tp_sha256_file "$path")" "$path"
        done
    )
}

# A stamp is one sha256 over what a materialised tree was built from, keyed by
# path so a rename is a change. Each tree stamps its own inputs and nothing
# else: the sysroot is `toolchain/{PIN,rust,libc}`, and the compiler source
# tree is `toolchain/{compiler,cargo,crates,libc}/` plus the lines it shares
# with that PIN — the channel and the libc crate its ports' libc is cut from.
# A std patch, a cargo patch and a C++ pin bump therefore restamp one tree,
# the other tree and neither; a libc patch restamps both.
tp_stamp() {
    {
        tp_materialiser_hash "$1" make_slopos_sysroot.sh
        tp_materialiser_hash "$1" lib/toolchain_pin.sh
        (cd "$1" && find "$TP_PIN_REL" "$TP_OVERLAY_REL/rust" "$TP_OVERLAY_REL/libc" \
            -type f -print) | tp_hash_lines "$1"
    } | tp_sha256_stream
}

tp_rustc_stamp() {
    {
        printf 'channel=%s\n' "$(tp_pin_value "$1/$TP_PIN_REL" channel)"
        printf 'libc_version=%s\n' "$(tp_pin_value "$1/$TP_PIN_REL" libc_version)"
        printf 'libc_checksum=%s\n' "$(tp_pin_value "$1/$TP_PIN_REL" libc_checksum)"
        tp_materialiser_hash "$1" make_rustc_src.sh
        tp_materialiser_hash "$1" lib/toolchain_pin.sh
        (cd "$1" && find "$TP_COMPILER_OVERLAY_REL" "$TP_CARGO_OVERLAY_REL" \
            "$TP_CRATES_OVERLAY_REL" "$TP_OVERLAY_REL/libc" -type f -print) | tp_hash_lines "$1"
    } | tp_sha256_stream
}

# Unpack a pinned `.crate` into <dest>: the registry cache first (offline, and
# already on disk for any workspace that depends on it), static.crates.io
# otherwise, checksum-verified either way.
#
# Usage: tp_unpack_crate <name> <version> <sha256> <dest> <pin-rel> [url]
tp_unpack_crate() {
    local cname="$1" version="$2" checksum="$3" dest="$4" pin_rel="$5" url="${6:-}"
    local name crate candidate download sha
    name="${cname}-${version}.crate"
    crate=""
    for candidate in "${CARGO_HOME:-$HOME/.cargo}"/registry/cache/*/"$name"; do
        [ -f "$candidate" ] || continue
        if [ "$(tp_sha256_file "$candidate")" = "$checksum" ]; then
            crate="$candidate"
            break
        fi
        echo "toolchain_pin: cached $candidate does not match its checksum in $pin_rel, ignoring it" >&2
    done

    download=""
    if [ -z "$crate" ]; then
        command -v curl >/dev/null 2>&1 || {
            echo "toolchain_pin: no cached $name and curl is unavailable to fetch it" >&2
            return 1
        }
        download="$(mktemp "${TMPDIR:-/tmp}/slopos-crate.XXXXXX")"
        trap 'rm -f "$download"' INT TERM
        curl -fL -o "$download" "${url:-https://static.crates.io/crates/$cname/$name}" || {
            rm -f "$download"
            echo "toolchain_pin: failed to download $name" >&2
            return 1
        }
        crate="$download"
    fi

    sha="$(tp_sha256_file "$crate")"
    if [ "$sha" != "$checksum" ]; then
        rm -f "$download"
        echo "toolchain_pin: $name checksum mismatch
       expected: $checksum ($pin_rel)
       actual:   $sha ($crate)" >&2
        return 1
    fi

    rm -rf "$dest"
    mkdir -p "$dest"
    tar -xzf "$crate" -C "$dest" --strip-components=1 || {
        rm -f "$download"
        echo "toolchain_pin: failed to unpack $crate into $dest" >&2
        return 1
    }
    rm -f "$download"
    trap - INT TERM
}

# Three trees need the pinned libc: the owned sysroot, the rustc source tree a
# bootstrap run builds the target's std out of, and that tree's crate ports.
tp_unpack_libc_crate() {
    local root="$1" dest="$2"
    local pin="$root/$TP_PIN_REL" version checksum
    version="$(tp_pin_value_required "$pin" libc_version toolchain_pin)" || return 1
    checksum="$(tp_pin_value_required "$pin" libc_checksum toolchain_pin)" || return 1
    tp_unpack_crate libc "$version" "$checksum" "$dest" "$TP_PIN_REL" "${LIBC_URL:-}"
}

# Apply every overlay patch under <prefix>, in the tree each one belongs to,
# and print how many.
#
# `git apply`, not `patch(1)`: the patches are `git diff` output that creates
# whole new directories, which GNU patch does not do reliably. Every
# materialised tree lives *inside* this git repository, and `git apply` run in
# a work tree resolves the patch's paths against the repository root rather
# than the working directory — every path then lands outside the directory it
# was invoked in, which `git apply` silently ignores while exiting 0.
# Measured, not theoretical: it is how the sysroot script once materialised an
# unpatched tree and reported success. `GIT_CEILING_DIRECTORIES` at the tree's
# parent stops repository discovery so `git apply` runs in its non-repo mode,
# and each patch is then re-checked in reverse so a no-op cannot pass again.
tp_apply_patches() {
    local root="$1" prefix="$2" library="${3:-}"
    local rel pin want sha dir ceiling out applied=0
    for rel in $(tp_patch_files "$root"); do
        case "$rel" in
            "$prefix"*) ;;
            *) continue ;;
        esac
        pin="$root/$(tp_patch_pin_file "$rel")"
        want="$(tp_pin_patch_sha "$pin" "$rel")"
        if [ -z "$want" ]; then
            echo "toolchain_pin: $rel carries no \`patch_sha256=$rel:<sha256>\` line in $(tp_patch_pin_file "$rel")" >&2
            return 1
        fi
        sha="$(tp_sha256_file "$root/$rel")"
        if [ "$sha" != "$want" ]; then
            echo "toolchain_pin: $rel does not match its pin
       expected: $want ($(tp_patch_pin_file "$rel"))
       actual:   $sha" >&2
            return 1
        fi
        dir="$root/$(tp_patch_apply_dir "$rel" "$library")"
        ceiling="$(tp_abspath "$(dirname "$root/$(tp_patch_tree_rel "$rel")")")"
        if ! out="$(cd "$dir" && GIT_CEILING_DIRECTORIES="$ceiling" git apply -p1 "$root/$rel" 2>&1)"; then
            printf '%s\n' "$out" >&2
            echo "toolchain_pin: patch failed to apply: $rel (in $dir)" >&2
            return 1
        fi
        if ! (cd "$dir" && GIT_CEILING_DIRECTORIES="$ceiling" \
                git apply -p1 --reverse --check "$root/$rel" >/dev/null 2>&1); then
            echo "toolchain_pin: $rel reported success but is not applied in $dir
       git apply resolved its paths somewhere else and changed nothing." >&2
            return 1
        fi
        applied=$((applied + 1))
    done
    printf '%s\n' "$applied"
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
