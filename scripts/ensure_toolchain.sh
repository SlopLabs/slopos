#!/usr/bin/env bash
set -euo pipefail

# Ensure the pinned Rust nightly toolchain, its required components and
# targets, and the owned `slopos` sysroot are all present.
# Reads the channel from rust-toolchain.toml in the repository root.
#
# `--no-sysroot` stops before the owned sysroot, for a job that only ever builds
# host-target crates. KernMiri is the one.

WANT_SYSROOT=1
for arg in "$@"; do
    case "$arg" in
        --no-sysroot) WANT_SYSROOT=0 ;;
        *)
            echo "usage: ensure_toolchain.sh [--no-sysroot]" >&2
            exit 2
            ;;
    esac
done

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

# `rust-src` is where `-Zbuild-std` and the owned sysroot both get the standard
# library from, so a toolchain that predates this script must gain it too — the
# install above only runs when the toolchain is absent entirely.
if ! rustup component list --installed --toolchain "$RUST_CHANNEL" | grep -q "^rust-src"; then
    rustup component add rust-src --toolchain "$RUST_CHANNEL"
fi

if [ "$WANT_SYSROOT" -eq 0 ]; then
    exit 0
fi

if ! rustup target list --toolchain "$RUST_CHANNEL" --installed | grep -q "^x86_64-unknown-none"; then
    rustup target add x86_64-unknown-none --toolchain "$RUST_CHANNEL"
fi

# The userland target is built by `cargo +slopos`, not by `cargo +$channel`:
# std for `x86_64-unknown-slopos` comes from the pinned std + libc forks, which
# live in an owned sysroot rather than in the rustup toolchain. Materialising it
# is a no-op once the stamp matches.
"$SCRIPT_DIR/make_slopos_sysroot.sh"
