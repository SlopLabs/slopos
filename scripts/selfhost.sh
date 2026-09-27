#!/bin/sh
# Build a kernel in the guest and try it from the spare boot slot.
#
# Usage: scripts/selfhost.sh build|install [release|dev|tests]
#
# `install` arms one boot of the new kernel: `bootctl reboot` tries it and
# `bootctl commit` keeps it; without the commit the next boot is the default.
set -eu

usage() {
    echo "usage: scripts/selfhost.sh build|install [release|dev|tests]" >&2
    exit 2
}

[ $# -ge 1 ] && [ $# -le 2 ] || usage
case "$1" in
build | install) ;;
*) usage ;;
esac

features=
case "${2:-release}" in
release) release=1 variant=release ;;
dev) release=0 variant=dev ;;
tests) release=0 variant=tests features="slopos-testing/qemu-exit kernel/tests" ;;
*) usage ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# build_devdisk.sh stages the toolchain where the host keeps its own sysroot.
PREFIX="$REPO_ROOT/third_party/rust-slopos"
if [ ! -x "$PREFIX/bin/cargo" ]; then
    echo "selfhost: no toolchain at $PREFIX; on the host, just toolchain and then just reset devdisk" >&2
    exit 1
fi

export PATH="$PREFIX/bin:/bin"
export CARGO_HOME="${CARGO_HOME:-$REPO_ROOT/builddir/cargo-home}"
export KERNEL_RELEASE="$release"
unset LD_LIBRARY_PATH

cd "$REPO_ROOT"
scripts/build_kernel.sh builddir builddir/target "$features"
[ "$1" = install ] || exit 0

elf="$REPO_ROOT/builddir/kernel-$variant.elf"
case "$(bootctl status)" in
*"default: slopos-b"*) slot=a ;;
*) slot=b ;;
esac
bootctl install "$slot" "$elf"
bootctl oneshot "slopos-$slot"
echo "selfhost: bootctl reboot tries slopos-$slot once; bootctl commit there keeps it"
