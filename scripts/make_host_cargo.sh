#!/usr/bin/env bash
set -euo pipefail

# Build the cargo fork for the machine running this script.
#
# Usage: make_host_cargo.sh
#
# `toolchain/cargo/0002-host-independent-metadata.patch` changes what cargo
# hashes into a crate's `-C metadata`, so a kernel the guest's cargo builds
# can match only a host build made by a cargo carrying the same patch. This is
# that cargo: `network` off, like the guest's, since the builds it serves read
# vendored sources. Emits <build dir>/host-cargo/cargo; incremental, so a
# warm run is cargo's own no-op.

SELF="make_host_cargo"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

SRC="$REPO_ROOT/$TP_RUSTC_SRC_REL"
OUT="${BUILD_DIR:-$REPO_ROOT/builddir}/host-cargo"

[ -f "$SRC/$TP_CARGO_TREE_REL/Cargo.toml" ] ||
    die "no cargo sources at $TP_RUSTC_SRC_REL/$TP_CARGO_TREE_REL — run scripts/make_rustc_src.sh"
[ "$(cat "$SRC/$TP_STAMP_NAME" 2>/dev/null)" = "$(tp_rustc_stamp "$REPO_ROOT")" ] ||
    die "$TP_RUSTC_SRC_REL is stale — run scripts/make_rustc_src.sh"

mkdir -p "$OUT"
(cd "$SRC/$TP_CARGO_TREE_REL" && CARGO_TARGET_DIR="$OUT/target" \
    cargo build --release --locked -p cargo --no-default-features) ||
    die "the cargo fork did not build for this host"
cp "$OUT/target/release/cargo" "$OUT/cargo.tmp"
mv -f "$OUT/cargo.tmp" "$OUT/cargo"
echo "$SELF: $OUT/cargo"
