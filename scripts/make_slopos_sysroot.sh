#!/usr/bin/env bash
set -euo pipefail

# Materialise the owned `slopos` sysroot and register it with rustup.
#
# `-Zbuild-std` resolves the standard library from
# `<host sysroot>/lib/rustlib/src/rust/library` and rebuilds it in an ephemeral
# workspace, so a `[patch]` in this workspace can never reach it — but owning
# the sysroot can. This script therefore does not mutate the rustup toolchain
# in place, the way the retired std-patching script did. It builds a
# *separate* sysroot:
#
#   1. `cp -al` the pinned rustup toolchain into third_party/rust-slopos.
#      Hardlinks, so it costs ~0.2 s and no disk.
#   2. Replace `lib/rustlib/src` with a real (unlinked) copy. Editing through
#      the hardlinks would write into the rustup toolchain itself, which is
#      exactly the failure mode owning a sysroot is meant to retire, so the
#      unlinking is asserted rather than assumed.
#   3. Unpack the pinned `libc` crate at `.../library/libc`, apply
#      toolchain/libc/*.patch inside it, then toolchain/rust/*.patch over
#      `.../library/`. That order, because the std patch points
#      `[patch.crates-io]` at the libc tree step 3 just unpacked.
#   4. `rustup toolchain link slopos third_party/rust-slopos`.
#
# After which `cargo +slopos ... --target targets/x86_64-unknown-slopos.json`
# builds against the fork. Nothing inside the pinned rustup toolchain
# directory is ever written; the only thing this adds to $RUSTUP_HOME is the
# `slopos` symlink that registration *is*.
#
# Idempotent: the stamp at third_party/rust-slopos/.slopos-stamp records the
# hash of toolchain/{PIN,rust,libc} and of this script — not toolchain/compiler/,
# which stamps the separate tree make_rustc_src.sh builds — so a second run
# with unchanged inputs re-checks the registration and exits. Any change to
# them rebuilds from scratch — the fork is a fork, not an incremental
# mutation.
#
# Usage: make_slopos_sysroot.sh
#
# Environment:
#   RUSTUP_HOME  - rustup root (default: ~/.rustup)
#   CARGO_HOME   - cargo root, searched for a cached libc crate (default: ~/.cargo)
#   LIBC_URL     - override the crate download URL (default: static.crates.io)

SELF="make_slopos_sysroot"
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
OVERLAY="$REPO_ROOT/$TP_OVERLAY_REL"
SYSROOT="$REPO_ROOT/$TP_SYSROOT_REL"
STAMP="$SYSROOT/$TP_STAMP_NAME"

[ -d "$OVERLAY" ] || die "missing $TP_OVERLAY_REL/ — the fork overlay (PIN + patches) is tracked in-repo; check out the tree that carries it"
[ -f "$PIN" ] || die "missing $TP_PIN_REL — the fork overlay (PIN + patches) is tracked in-repo; check out the tree that carries it"

CHANNEL="$(tp_channel "$REPO_ROOT")"
[ -n "$CHANNEL" ] || die "failed to read the Rust channel from rust-toolchain.toml"

PIN_CHANNEL="$(tp_pin_value_required "$PIN" channel "$SELF")"
LIBC_VERSION="$(tp_pin_value_required "$PIN" libc_version "$SELF")"

if [ "$PIN_CHANNEL" != "$CHANNEL" ]; then
    die "$TP_PIN_REL pins channel=$PIN_CHANNEL but rust-toolchain.toml says $CHANNEL
       The fork is cut against one compiler. Re-cut the patches, then update
       $TP_PIN_REL. scripts/check_toolchain_pin.sh gates exactly this."
fi

STAMP_WANT="$(tp_stamp "$REPO_ROOT")"

# ---------------------------------------------------------------------------
# Registration. `rustup toolchain link` is a symlink, so an already-correct
# link is free to detect and a stale one is cheap to replace without touching
# the materialised tree.
# ---------------------------------------------------------------------------
register() {
    local want linked
    want="$(tp_abspath "$SYSROOT")"
    linked="$(tp_link_target)"
    if [ "$linked" = "$want" ]; then
        return 0
    fi
    command -v rustup >/dev/null 2>&1 || die "rustup is required to register the $TP_TOOLCHAIN_NAME toolchain"
    if [ -n "$linked" ]; then
        rustup toolchain uninstall "$TP_TOOLCHAIN_NAME" >/dev/null
    fi
    rustup toolchain link "$TP_TOOLCHAIN_NAME" "$want"
}

# ---------------------------------------------------------------------------
# Fast path: the overlay is unchanged, so the tree is what the stamp says it
# is. Only the registration is re-checked — a materialised sysroot nothing
# points at builds nothing.
# ---------------------------------------------------------------------------
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$STAMP_WANT" ]; then
    register
    echo "$SELF: $TP_SYSROOT_REL up to date, linked as +$TP_TOOLCHAIN_NAME (stamp $STAMP_WANT)"
    exit 0
fi

# ---------------------------------------------------------------------------
# Locate the pinned rustup toolchain to clone from.
# ---------------------------------------------------------------------------
command -v rustc >/dev/null 2>&1 || die "rustc is required; run scripts/ensure_toolchain.sh"
RUSTUP_TOOLCHAIN_DIR="$(rustc "+$CHANNEL" --print sysroot 2>/dev/null || true)"
[ -n "$RUSTUP_TOOLCHAIN_DIR" ] && [ -d "$RUSTUP_TOOLCHAIN_DIR" ] \
    || die "toolchain $CHANNEL is not installed; run scripts/ensure_toolchain.sh"

RUSTUP_SRC="$RUSTUP_TOOLCHAIN_DIR/lib/rustlib/src"
[ -d "$RUSTUP_SRC/rust/library" ] \
    || die "toolchain $CHANNEL has no rust-src component at $RUSTUP_SRC/rust/library
       Install it with: rustup component add rust-src --toolchain $CHANNEL"

if [ "$(tp_abspath "$RUSTUP_TOOLCHAIN_DIR")" = "$(tp_abspath "$SYSROOT")" ]; then
    die "rustc +$CHANNEL already resolves to $TP_SYSROOT_REL — refusing to clone the clone.
       Something has overridden the $CHANNEL toolchain to point at the fork."
fi

# The fork is cut against *pristine* rust-src. A machine that ever ran the
# retired in-place std-patching script has slopos files sitting in the rustup
# component itself, and the fork patches would then apply on top of them —
# silently, and to a tree nobody can reproduce.
POLLUTED="$(find "$RUSTUP_SRC" -name '*slopos*' -print 2>/dev/null | head -n 3)"
if [ -n "$POLLUTED" ]; then
    die "the rustup rust-src component is not pristine — it carries SlopOS files:
$(printf '%s\n' "$POLLUTED" | sed 's/^/         /')
       This machine ran the retired in-place std patcher. Both steps are
       needed: a reinstall restores the files it *edited*, but rustup only
       deletes what its own manifest lists, so the files it *added* survive
       a remove/add untouched (measured, not assumed).
         rustup component remove rust-src --toolchain $CHANNEL && \\
           rustup component add rust-src --toolchain $CHANNEL
         find '$RUSTUP_SRC' -name '*slopos*' -delete
       then re-run this script."
fi

# The one invariant worth asserting: after the copy, the src tree must not
# share inodes with the rustup toolchain, or applying a patch would rewrite
# the toolchain every other project on this machine compiles against.
link_count() {
    if stat -c '%h' "$1" >/dev/null 2>&1; then
        stat -c '%h' "$1"
    else
        stat -f '%l' "$1"
    fi
}

# ---------------------------------------------------------------------------
# Clone the toolchain, then unlink the source tree.
# ---------------------------------------------------------------------------
rm -rf "$SYSROOT"
mkdir -p "$(dirname "$SYSROOT")"
cp -al "$RUSTUP_TOOLCHAIN_DIR" "$SYSROOT" \
    || die "cp -al failed — this needs a cp(1) with hardlink support (-l) and a
       \$RUSTUP_HOME on the same filesystem as $TP_SYSROOT_REL"

rm -rf "$SYSROOT/lib/rustlib/src"
cp -a "$RUSTUP_SRC" "$SYSROOT/lib/rustlib/src"

LIBRARY="$SYSROOT/$TP_LIBRARY_REL"
[ -d "$LIBRARY" ] || die "copied source tree has no rust/library at $LIBRARY"

for sample in "$LIBRARY/std/src/lib.rs" "$LIBRARY/std/build.rs"; do
    [ -f "$sample" ] || die "copied source tree is missing $sample — the rust-src layout changed"
    count="$(link_count "$sample")"
    if [ "$count" != "1" ]; then
        die "$sample still has $count links after the copy: it is shared with
       $RUSTUP_TOOLCHAIN_DIR and patching it would corrupt the rustup toolchain.
       Aborting before any patch is applied."
    fi
done

LIBC_DIR="$LIBRARY/libc"
tp_unpack_libc_crate "$REPO_ROOT" "$LIBC_DIR" || die "could not stage the pinned libc crate"

# ---------------------------------------------------------------------------
# The fork patches, libc first: the std patch adds `libc = { path = "libc" }`
# under `[patch.crates-io]` in library/Cargo.toml, so the tree it names has to
# be unpacked and patched before that resolution exists.
# ---------------------------------------------------------------------------
command -v git >/dev/null 2>&1 || die "git is required to apply the fork patches (git apply)"

LIBC_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_OVERLAY_REL/libc/")" ||
    die "the libc fork did not apply"
RUST_PATCHES="$(tp_apply_patches "$REPO_ROOT" "$TP_OVERLAY_REL/rust/")" ||
    die "the std fork did not apply"

if [ "$LIBC_PATCHES" = "0" ]; then
    die "no patches under $TP_OVERLAY_REL/libc/ — an unpatched libc has no slopos module"
fi
if [ "$RUST_PATCHES" = "0" ]; then
    die "no patches under $TP_OVERLAY_REL/rust/ — the std fork is what makes this sysroot a fork"
fi

# ---------------------------------------------------------------------------
# Stamp last: a tree that failed halfway through must not look finished.
# ---------------------------------------------------------------------------
printf '%s\n' "$STAMP_WANT" > "$STAMP"
register

echo "$SELF: materialised $TP_SYSROOT_REL from $CHANNEL — libc $LIBC_VERSION, $RUST_PATCHES rust + $LIBC_PATCHES libc patch(es), linked as +$TP_TOOLCHAIN_NAME (stamp $STAMP_WANT)"
