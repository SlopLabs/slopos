#!/usr/bin/env bash
set -euo pipefail

# Ensure the pinned sccache is under third_party/ and print its path:
# `RUSTC_WRAPPER="$(scripts/ensure_sccache.sh)" cargo build ...`. Offline hosts
# pre-stage the binary at that path or set SCCACHE_URL.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VERSION="0.10.0"
SHA256="1fbb35e135660d04a2d5e42b59c7874d39b3deb17de56330b25b713ec59f849b"
ASSET="sccache-v${VERSION}-x86_64-unknown-linux-musl"
URL="${SCCACHE_URL:-https://github.com/mozilla/sccache/releases/download/v${VERSION}/${ASSET}.tar.gz}"
DIR="$REPO_ROOT/third_party/sccache-$VERSION"
BIN="$DIR/sccache"

if [ ! -x "$BIN" ]; then
    [ "$(uname -sm)" = "Linux x86_64" ] || {
        echo "ensure_sccache: the pinned binary is for x86_64 Linux; install sccache and set RUSTC_WRAPPER yourself" >&2
        exit 1
    }
    echo "ensure_sccache: fetching sccache $VERSION" >&2
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    curl -L --fail --show-error --silent --retry 3 --retry-all-errors "$URL" -o "$tmp/sccache.tar.gz"
    echo "$SHA256  $tmp/sccache.tar.gz" | sha256sum -c - >&2
    tar -xzf "$tmp/sccache.tar.gz" -C "$tmp"
    mkdir -p "$DIR"
    install -m 0755 "$tmp/$ASSET/sccache" "$BIN"
fi

echo "$BIN"
