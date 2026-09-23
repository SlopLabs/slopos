#!/usr/bin/env bash
set -euo pipefail

# Materialise the pinned rustc source tree carrying the SlopOS target.
#
# `x86_64-unknown-slopos` is a JSON spec, which is enough to build *for* and
# not enough to build rustc *for*: bootstrap resolves `--host` through the
# compiler's own built-in target list. The built-in spec therefore lives in a
# patch over rustc's sources, and this is the tree that patch lands in —
# what `scripts/check_rustc_target.sh` grades and what a bootstrap run takes
# as its `--src`.
#
# Emits third_party/slopos-rustc-src/, the pinned nightly's own sources minus
# two subtrees this tree does not use:
#
#   vendor/           2.4 GB of crates.io copies. Cargo fetches the same
#                     versions from the lockfile, which is why `.cargo/` — the
#                     config that redirects crates-io at that directory — is
#                     dropped with it. An offline build wants both back.
#   src/llvm-project/ 1.4 GB of C++. The C++ the toolchain needs is pinned
#                     separately in toolchain/cxx/PIN, and a bootstrap run
#                     takes LLVM from `download-ci-llvm` or from that tarball.
#
# The tree carries three forks: `toolchain/compiler/`; in `src/tools/cargo`,
# `toolchain/cargo/` (see toolchain/cargo/PIN for what that one is for); and
# `toolchain/crates/`, ports of crates.io crates both workspaces depend on,
# unpacked from their pinned `.crate` files into `slopos-crates/` beside a
# copy of the libc fork, and wired into both workspaces by the
# `[patch.crates-io]` in `toolchain/crates/wiring/`. See toolchain/crates/PIN.
#
# Idempotent: the stamp at third_party/slopos-rustc-src/.slopos-stamp records
# the hash of toolchain/{compiler,cargo,crates,libc}/, of this script — its
# --exclude set decides what the tree holds — and of toolchain/PIN's channel
# and libc lines, so a second run with unchanged inputs exits immediately.
#
# Usage: make_rustc_src.sh
#
# Environment:
#   RUSTC_SRC_URL - override the tarball URL (default: static.rust-lang.org)

SELF="make_rustc_src"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

case "${1:-}" in
    "") ;;
    *) echo "usage: $SELF.sh" >&2; exit 2 ;;
esac

die() {
    echo "$SELF: $*" >&2
    exit 1
}

PIN="$REPO_ROOT/$TP_PIN_REL"
COMPILER_PIN="$REPO_ROOT/$TP_COMPILER_PIN_REL"
CARGO_PIN="$REPO_ROOT/$TP_CARGO_PIN_REL"
CRATES_PIN="$REPO_ROOT/$TP_CRATES_PIN_REL"
SRC="$REPO_ROOT/$TP_RUSTC_SRC_REL"
STAMP="$SRC/$TP_STAMP_NAME"

[ -f "$COMPILER_PIN" ] || die "missing $TP_COMPILER_PIN_REL — the compiler fork (PIN + patch) is tracked in-repo"
[ -f "$CARGO_PIN" ] || die "missing $TP_CARGO_PIN_REL — the cargo fork (PIN + patch) is tracked in-repo"
[ -f "$CRATES_PIN" ] || die "missing $TP_CRATES_PIN_REL — the crate ports (PIN + patches) are tracked in-repo"

STAMP_WANT="$(tp_rustc_stamp "$REPO_ROOT")"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$STAMP_WANT" ]; then
    echo "$SELF: $TP_RUSTC_SRC_REL up to date (stamp $STAMP_WANT)"
    exit 0
fi

CHANNEL="$(tp_channel "$REPO_ROOT")"
PIN_CHANNEL="$(tp_pin_value_required "$PIN" channel "$SELF")"
[ "$CHANNEL" = "$PIN_CHANNEL" ] ||
    die "channel drift: $TP_PIN_REL pins $PIN_CHANNEL, rust-toolchain.toml says $CHANNEL"

# Only a dated nightly has a source tarball at a URL this can derive. A
# release channel would need the version rather than the date, and nothing
# here has ever been pinned to one.
case "$CHANNEL" in
    nightly-[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]) ;;
    *) die "channel $CHANNEL is not a dated nightly; $TP_PIN_REL pins the source tarball by date" ;;
esac
DATE="${CHANNEL#nightly-}"

SHA_WANT="$(tp_pin_value_required "$COMPILER_PIN" rustc_src_sha256 "$SELF")"
URL="${RUSTC_SRC_URL:-https://static.rust-lang.org/dist/$DATE/rustc-nightly-src.tar.xz}"
TARBALL="$REPO_ROOT/third_party/rustc-src-$CHANNEL.tar.xz"

fetch() {
    echo "$SELF: fetching rustc $CHANNEL sources (265 MB)..." >&2
    mkdir -p "$(dirname "$TARBALL")"
    trap 'rm -f "$TARBALL.part"' EXIT INT TERM
    curl -L --fail --show-error "$URL" -o "$TARBALL.part" || die "could not fetch $URL
       An offline checkout pre-populates third_party/ with
       $(basename "$TARBALL"), or points RUSTC_SRC_URL at a local copy."
    mv "$TARBALL.part" "$TARBALL"
    trap - EXIT INT TERM
}

# A tarball that fails its checksum is re-fetched once: the file is restored
# from a CI cache and left alone by an offline checkout, so refusing outright
# would wedge a job on a pin correction until the cache expired.
[ -f "$TARBALL" ] || fetch
if [ "$(tp_sha256_file "$TARBALL")" != "$SHA_WANT" ]; then
    rm -f "$TARBALL"
    fetch
    SHA_GOT="$(tp_sha256_file "$TARBALL")"
    [ "$SHA_GOT" = "$SHA_WANT" ] || die "checksum mismatch for $(basename "$TARBALL")
       expected: $SHA_WANT ($TP_COMPILER_PIN_REL)
       actual:   $SHA_GOT"
fi

command -v git >/dev/null 2>&1 || die "git is required to apply the compiler fork (git apply)"

rm -rf "$SRC" "$SRC.part"
mkdir -p "$SRC.part"
tar -xf "$TARBALL" -C "$SRC.part" --strip-components=1 \
    --exclude 'rustc-nightly-src/vendor' \
    --exclude 'rustc-nightly-src/src/llvm-project' \
    --exclude 'rustc-nightly-src/.cargo' ||
    die "failed to unpack $(basename "$TARBALL")"
[ -d "$SRC.part/compiler/rustc_target/src/spec/targets" ] ||
    die "unpacked tree has no compiler/rustc_target/src/spec/targets"
mv "$SRC.part" "$SRC"

PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_COMPILER_OVERLAY_REL/")" ||
    die "the compiler fork did not apply"
if [ "$PATCHES" = "0" ]; then
    die "no patches under $TP_COMPILER_OVERLAY_REL/ — an unpatched tree has no slopos target"
fi

CARGO_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_CARGO_OVERLAY_REL/")" ||
    die "the cargo fork did not apply"
if [ "$CARGO_PATCHES" = "0" ]; then
    die "no patches under $TP_CARGO_OVERLAY_REL/ — an unpatched cargo has no offline build"
fi

# The crate ports, each unpacked from the `.crate` its patch is cut against,
# and the libc fork beside them: the compiler and cargo resolve crates.io
# `libc`, and bootstrap's own `library/libc` exists only once a bootstrap run
# has staged it.
CRATES_DIR="$SRC/$TP_CRATES_TREE_REL"
mkdir -p "$CRATES_DIR"
PORTS=0
while read -r name version sha; do
    [ -n "$name" ] || continue
    [ -f "$REPO_ROOT/$TP_CRATES_OVERLAY_REL/$name-$version.patch" ] ||
        die "$TP_CRATES_PIN_REL pins $name $version, which has no $TP_CRATES_OVERLAY_REL/$name-$version.patch"
    tp_unpack_crate "$name" "$version" "$sha" "$CRATES_DIR/$name-$version" "$TP_CRATES_PIN_REL" ||
        die "could not stage $name $version"
    PORTS=$((PORTS + 1))
done <<EOF
$(tp_pin_crates "$CRATES_PIN")
EOF
for rel in $(tp_patch_files "$REPO_ROOT"); do
    case "$rel" in
        "$TP_CRATES_WIRING_REL/"*) ;;
        "$TP_CRATES_OVERLAY_REL/"*)
            [ -d "$REPO_ROOT/$(tp_patch_apply_dir "$rel")" ] ||
                die "$rel has no \`crate=\` line in $TP_CRATES_PIN_REL naming the crate it is cut against"
            ;;
    esac
done
tp_unpack_libc_crate "$REPO_ROOT" "$CRATES_DIR/libc" ||
    die "could not stage the pinned libc crate into $TP_RUSTC_SRC_REL/$TP_CRATES_TREE_REL"
LIBC_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_OVERLAY_REL/libc/" "$TP_RUSTC_SRC_REL/$TP_CRATES_TREE_REL")" ||
    die "the libc fork did not apply to $TP_RUSTC_SRC_REL/$TP_CRATES_TREE_REL/libc"
[ "$LIBC_PATCHES" != "0" ] ||
    die "no patches under $TP_OVERLAY_REL/libc/ — an unpatched libc has no slopos module"
CRATE_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_CRATES_OVERLAY_REL/")" ||
    die "the crate ports did not apply"
[ "$CRATE_PATCHES" -gt "$PORTS" ] ||
    die "no patches under $TP_CRATES_WIRING_REL/ — nothing points either workspace at the ports"

# The second LLVM port is applied by a bootstrap run, in a subtree this tree
# does not carry, so nothing else would notice it ceasing to apply until
# hours into one. Only the files it *edits* are unpacked — one `tar` pass,
# because each pass reads the whole 253 MB archive — and the files it creates
# are checked by being absent, which is what `git apply --check` wants.
LLVM_EDITS="$(sed -n 's|^--- a/||p' "$REPO_ROOT/$TP_LLVM_RUSTC_OVERLAY_REL"/*.patch | LC_ALL=C sort -u)"
[ -n "$LLVM_EDITS" ] ||
    die "no patches under $TP_LLVM_RUSTC_OVERLAY_REL/ — an unported LLVM has no SlopOS triple"
PROBE="$(mktemp -d)"
trap 'rm -rf "$PROBE"' EXIT INT TERM
# shellcheck disable=SC2086
tar -xf "$TARBALL" -C "$PROBE" --strip-components=3 \
    $(printf "rustc-nightly-src/src/llvm-project/%s\n" $LLVM_EDITS) ||
    die "the files $TP_LLVM_RUSTC_OVERLAY_REL/ edits are not in $CHANNEL's src/llvm-project"
# Applied rather than `--check`ed, so a second patch is graded against the
# tree the first one left rather than against a pristine one.
for patch in "$REPO_ROOT/$TP_LLVM_RUSTC_OVERLAY_REL"/*.patch; do
    (cd "$PROBE" && GIT_CEILING_DIRECTORIES="$PROBE" git apply -p1 "$patch") ||
        die "$TP_LLVM_RUSTC_OVERLAY_REL/$(basename "$patch") no longer applies to $CHANNEL's src/llvm-project
       The port is cut against rustc's bundled LLVM, which a channel bump moves."
done
rm -rf "$PROBE"
trap - EXIT INT TERM

printf '%s\n' "$STAMP_WANT" > "$STAMP"

echo "$SELF: materialised $TP_RUSTC_SRC_REL from $CHANNEL — $PATCHES compiler, $CARGO_PATCHES cargo, $CRATE_PATCHES crate-port patch(es) (stamp $STAMP_WANT)"
