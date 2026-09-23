#!/usr/bin/env bash
set -euo pipefail

# Materialise the directory `.cargo/vendor.toml` names with every crates.io
# package in the workspace's `Cargo.lock` and in the pinned toolchain's
# `library/Cargo.lock`, which `-Zbuild-std` resolves std against. rust-src
# vendors the second set already, but build-std never reads that tree's
# `.cargo/config.toml`, so an offline build needs both in one directory.
#
# Nothing here is a new pin: the workspace's packages are fetched against
# `Cargo.lock`'s checksums and std's are copied from the pinned rust-src.
# Idempotent: stamped over both lockfiles, `.cargo/vendor.toml` and this script.
#
# Usage: make_vendor.sh
#
# Environment:
#   CARGO - cargo binary (default: cargo)

SELF="make_vendor"
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

CARGO="${CARGO:-cargo}"
CHANNEL="$(tp_channel "$REPO_ROOT")"
VENDOR_REL="$(tp_vendor_rel "$REPO_ROOT")" || die ".cargo/vendor.toml names no vendored-sources directory"
OUT="$REPO_ROOT/$VENDOR_REL"
STAMP="$OUT/$TP_STAMP_NAME"

LOCK="$REPO_ROOT/Cargo.lock"
[ -f "$LOCK" ] || die "Cargo.lock is missing — it is tracked, and it is what pins every crate here"

SYSROOT="$(rustc +"$CHANNEL" --print sysroot 2>/dev/null)" ||
    die "no $CHANNEL toolchain — run scripts/ensure_toolchain.sh"
LIBRARY="$SYSROOT/$TP_LIBRARY_REL"
[ -f "$LIBRARY/Cargo.lock" ] || die "$LIBRARY/Cargo.lock is missing — the rust-src component is not installed"
[ -d "$LIBRARY/vendor" ] || die "$LIBRARY/vendor is missing — this rust-src predates vendored std dependencies"

STAMP_WANT="$(
    {
        tp_materialiser_hash "$REPO_ROOT" make_vendor.sh
        printf '%s  Cargo.lock\n' "$(tp_sha256_file "$LOCK")"
        printf '%s  .cargo/vendor.toml\n' "$(tp_sha256_file "$REPO_ROOT/.cargo/vendor.toml")"
        printf '%s  library/Cargo.lock\n' "$(tp_sha256_file "$LIBRARY/Cargo.lock")"
    } | tp_sha256_stream
)"
if [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$STAMP_WANT" ]; then
    echo "$SELF: $VENDOR_REL up to date (stamp $STAMP_WANT)"
    exit 0
fi

rm -rf "$OUT.part"
trap 'rm -rf "$OUT.part"' EXIT INT TERM

# `--versioned-dirs` names directories as rust-src's `library/vendor` does, so
# one crate at two versions stays two packages and a crate both sets pin is one.
(cd "$REPO_ROOT" && "$CARGO" +"$CHANNEL" vendor --locked --versioned-dirs --quiet "$OUT.part" >/dev/null) ||
    die "cargo vendor failed — the crates come from the registry cache when they are there and from crates.io otherwise"

library_count=0
for dir in "$LIBRARY/vendor"/*/; do
    name="$(basename "$dir")"
    [ -f "$dir/.cargo-checksum.json" ] || die "$dir has no .cargo-checksum.json"
    [ -e "$OUT.part/$name" ] && continue
    cp -a "$dir" "$OUT.part/$name"
    library_count=$((library_count + 1))
done

# A copy of this directory, such as the dev disk's, is graded against std's
# lockfile, which nothing in a workspace carries.
cp "$LIBRARY/Cargo.lock" "$OUT.part/library.lock"
printf '%s\n' "$STAMP_WANT" >"$OUT.part/$TP_STAMP_NAME"
rm -rf "$OUT"
mv "$OUT.part" "$OUT"
trap - EXIT INT TERM

total="$(find "$OUT" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')"
echo "$SELF: $VENDOR_REL holds $total packages ($library_count from rust-src's library/vendor) — $(du -sh "$OUT" | cut -f1)"
