#!/usr/bin/env bash
set -euo pipefail

# Ensure the pinned Rust nightly toolchain and required targets are installed.
# Reads the channel from rust-toolchain.toml in the repository root.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TOOLCHAIN_FILE="${REPO_ROOT}/rust-toolchain.toml"

if ! command -v rustup >/dev/null 2>&1; then
    echo "rustup is required to install the pinned nightly toolchain" >&2
    exit 1
fi

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$TOOLCHAIN_FILE")"
if [ -z "$RUST_CHANNEL" ]; then
    echo "Failed to read Rust channel from $TOOLCHAIN_FILE" >&2
    exit 1
fi

if ! rustup toolchain list | grep -q "^${RUST_CHANNEL}"; then
    rustup toolchain install "$RUST_CHANNEL" \
        --component=rust-src \
        --component=rustfmt \
        --component=clippy \
        --component=llvm-tools-preview
fi

if ! rustup target list --toolchain "$RUST_CHANNEL" --installed | grep -q "^x86_64-unknown-none"; then
    rustup target add x86_64-unknown-none --toolchain "$RUST_CHANNEL"
fi
