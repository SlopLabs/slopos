#!/bin/sh
# Build the system in the guest, run its base at once and try its kernel from
# the spare boot slot.
#
# Usage: scripts/selfhost.sh build|install|install-kernel [release|dev|tests]
#
# `build` makes the kernel, the userland and the base image the kernel boots
# with: `builddir/kernel-<variant>.elf` and `builddir/initramfs.cpio`, or for
# `tests` the tests userland and `builddir/initramfs-tests.cpio`, and the apps
# outside the base, `builddir/apps`. `install` builds the userland, the base
# and the apps and makes the base the one every slot boots, running it at
# once when the running kernel serves it and leaving it for the next boot
# otherwise; `bootctl base rollback` takes it back. The apps go to /usr/local
# once the running libc.so is the one they were built against, as whoever
# runs it while /usr/local is theirs and through privd's
# org.slopos.software.install once it is the system's; otherwise the command
# that installs them is printed. A new base
# needing a new kernel goes in first: `install-kernel` builds the kernel and
# puts it into the slot that is not the default, arming one boot of it, which
# `bootctl reboot` tries and `bootctl commit` keeps.
set -eu

usage() {
    echo "usage: scripts/selfhost.sh build|install|install-kernel [release|dev|tests]" >&2
    exit 2
}

[ $# -ge 1 ] && [ $# -le 2 ] || usage
case "$1" in
build | install | install-kernel) ;;
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

[ "$1" = install-kernel ] && tools=cargo
for tool in $tools; do
    command -v "$tool" >/dev/null || {
        echo "selfhost: no $tool on PATH; on the host, just toolchain and then just boot installs one at /usr/local" >&2
        exit 1
    }
done
if [ "$1" != install-kernel ]; then
    for program in $BASE_RECIPE_PROGRAMS; do
        [ -x "/usr/local/$program" ] || {
            echo "selfhost: no /usr/local/$program for the base; on the host, just toolchain and then just boot installs one" >&2
            exit 1
        }
    done
fi

export CARGO_HOME="${CARGO_HOME:-$REPO_ROOT/builddir/cargo-home}"
export KERNEL_RELEASE="$release"
unset LD_LIBRARY_PATH

# A machine the host drives through remoted stays paired: the base it builds
# carries the pairing the running base does.
pairing=
[ -f /usr/share/slopos/remote/remote.conf ] && pairing=/usr/share/slopos/remote

cd "$REPO_ROOT"
elf="$REPO_ROOT/builddir/kernel-$variant.elf"
if [ "$1" != install ]; then
    scripts/build_kernel.sh builddir builddir/target "$features"
fi
if [ "$1" = install-kernel ]; then
    slot="$(bootctl spare)"
    bootctl install "$slot" "$elf"
    bootctl oneshot "slopos-$slot"
    echo "selfhost: bootctl reboot tries slopos-$slot once; bootctl commit there keeps it"
    exit 0
fi
bash scripts/build_userland.sh builddir builddir/target $userland
COREUTILS_LINKS="$COREUTILS_TOOLS" EXTRA_SHARED_OBJECTS="$shared" \
    RECIPE_PREFIX=/usr/local RECIPE_PROGRAMS="$BASE_RECIPE_PROGRAMS" RECIPE_LICENSES="$BASE_RECIPES" \
    REMOTE_PAIRING_DIR="$pairing" scripts/build_initramfs.sh "$base" builddir $programs
[ "$1" = install ] || exit 0

bootctl base install "$REPO_ROOT/$base"
apps="/bin/tree-install apps builddir/apps /usr/local"
if cmp -s /lib/libc.so builddir/libc.so; then
    /bin/tree-install apps builddir/apps /usr/local ||
        echo "selfhost: the apps stay as they were"
else
    echo "selfhost: the apps need this build's libc.so; once its base runs, run in $REPO_ROOT: $apps"
fi
echo "selfhost: bootctl base rollback takes the base back; a kernel it needs goes in with selfhost.sh install-kernel"
