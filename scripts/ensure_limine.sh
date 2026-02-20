#!/usr/bin/env bash
set -euo pipefail

# Ensure the Limine bootloader is cloned and built.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

LIMINE_DIR="${LIMINE_DIR:-${REPO_ROOT}/third_party/limine}"
LIMINE_REPO="${LIMINE_REPO:-https://github.com/limine-bootloader/limine.git}"
LIMINE_BRANCH="${LIMINE_BRANCH:-v8.x-branch-binary}"

if [ ! -d "$LIMINE_DIR" ]; then
    echo "Cloning Limine bootloader..." >&2
    git clone --branch="$LIMINE_BRANCH" --depth=1 "$LIMINE_REPO" "$LIMINE_DIR"
fi

if [ ! -f "$LIMINE_DIR/limine-bios.sys" ] || [ ! -f "$LIMINE_DIR/BOOTX64.EFI" ]; then
    echo "Building Limine..." >&2
    make -C "$LIMINE_DIR" >/dev/null
fi
