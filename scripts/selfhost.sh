#!/bin/sh
# Build the system in the guest and try it from the spare boot slot.
#
# Usage: scripts/selfhost.sh build|install [release|dev|tests]
#
# `build` makes the kernel, the userland and the base image the kernel boots
# with: `builddir/kernel-<variant>.elf` and `builddir/initramfs.cpio`, or for
# `tests` the tests userland and `builddir/initramfs-tests.cpio`. `install`
# puts both into the slot that is not the default and arms one boot of it:
# `bootctl reboot` tries it and `bootctl commit` keeps it; without the commit
# the next boot is the default.
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

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
. "$REPO_ROOT/scripts/lib/base.sh"

features=
userland=
programs="$BASE_PROGRAMS"
shared=
base=builddir/initramfs.cpio
tools="cargo bash"
case "${2:-release}" in
release) release=1 variant=release ;;
dev) release=0 variant=dev ;;
tests)
    release=0 variant=tests features="slopos-testing/qemu-exit kernel/tests"
    userland=--test programs="$BASE_TEST_PROGRAMS" shared="$BASE_TEST_SHARED_OBJECTS"
    base=builddir/initramfs-tests.cpio
    # `build_userland.sh --test` builds the C++ runtime and its probes with
    # these, after the kernel and the rest of the userland.
    tools="$tools clang clang++ ld.lld llvm-ar cmake ninja git"
    ;;
*) usage ;;
esac

for tool in $tools; do
    command -v "$tool" >/dev/null || {
        echo "selfhost: no $tool on PATH; on the host, just toolchain and then just boot installs one at /usr/local" >&2
        exit 1
    }
done

export CARGO_HOME="${CARGO_HOME:-$REPO_ROOT/builddir/cargo-home}"
export KERNEL_RELEASE="$release"
unset LD_LIBRARY_PATH

cd "$REPO_ROOT"
scripts/build_kernel.sh builddir builddir/target "$features"
bash scripts/build_userland.sh builddir builddir/target $userland
COREUTILS_LINKS="$COREUTILS_TOOLS" EXTRA_SHARED_OBJECTS="$shared" \
    scripts/build_initramfs.sh "$base" builddir $programs
[ "$1" = install ] || exit 0

elf="$REPO_ROOT/builddir/kernel-$variant.elf"
case "$(bootctl status)" in
*"default: slopos-b"*) slot=a ;;
*) slot=b ;;
esac
bootctl install "$slot" "$elf" "$REPO_ROOT/$base"
bootctl oneshot "slopos-$slot"
echo "selfhost: bootctl reboot tries slopos-$slot once; bootctl commit there keeps it"
