#!/bin/sh
# Build the SlopOS kernel ELF, on the Linux host or inside SlopOS.
#
# Usage: build_kernel.sh <build_dir> <cargo_target_dir> [features]
#
# POSIX sh using only the coreutils, because the guest runs it with /bin/shell
# and has no bash, rustup, python or LLVM binutils. What only the host does —
# ensure_toolchain.sh before, the ELF gates after — lives in the justfile.
#
# Environment:
#   CARGO             - cargo command, split on blanks (default: cargo); the
#                       justfile passes `cargo +slopos`
#   RUST_TARGET       - kernel target JSON (default: targets/x86_64-slos.json)
#   KERNEL_RUSTFLAGS  - extra RUSTFLAGS (default: -C force-frame-pointers=yes)
#   KERNEL_RELEASE    - 1 for an optimized kernel
#   KERNEL_SAFESTACK  - 0 to build without the SafeStack sanitizer
set -eu

if [ $# -lt 2 ]; then
    echo "usage: build_kernel.sh <build_dir> <cargo_target_dir> [features]" >&2
    exit 2
fi
FEATURES="${3:-}"
mkdir -p "$1"
BUILD_DIR="$(cd "$1" && pwd)"
case "$2" in
/*) CARGO_TARGET_DIR="$2" ;;
*) CARGO_TARGET_DIR="$(pwd)/$2" ;;
esac
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CARGO="${CARGO:-cargo}"
RUST_TARGET="${RUST_TARGET:-targets/x86_64-slos.json}"
KERNEL_RUSTFLAGS="${KERNEL_RUSTFLAGS:--C force-frame-pointers=yes}"

release=
VARIANT=dev
case "$FEATURES" in
*kernel/tests*) VARIANT=tests ;;
esac
if [ "${KERNEL_RELEASE:-0}" = 1 ]; then
    release=--release
    case "$VARIANT" in
    tests) VARIANT=release-tests ;;
    *) VARIANT=release ;;
    esac
fi

# SafeStack: `karch::safestack_rt` supplies `__safestack_pointer_address` and
# `_start` primes it before any instrumented code runs, so the runtime archive
# rustc links into every safestack binary only has to exist. rustup ships none
# for a custom target; an empty archive is the 8-byte magic.
if [ "${KERNEL_SAFESTACK:-1}" = 1 ]; then
    KERNEL_RUSTFLAGS="$KERNEL_RUSTFLAGS -Z sanitizer=safestack -C llvm-args=-safestack-use-pointer-address"
    libdir="$($CARGO rustc --locked -Zunstable-options --print target-libdir \
        --target "$RUST_TARGET" -p kernel --bin kernel)"
    stub="$libdir/librustc-nightly_rt.safestack.a"
    mkdir -p "$libdir"
    printf '!<arch>\n' >"$stub.$$"
    if cmp -s "$stub.$$" "$stub" 2>/dev/null; then
        rm -f "$stub.$$"
    else
        mv -f "$stub.$$" "$stub"
    fi
fi

# One ELF per variant: a shared path lets whichever build ran last answer for
# all of them, to the gates, to gdb and to the ISO builder.
KERNEL_ELF="$BUILD_DIR/kernel-$VARIANT.elf"
rm -f "$BUILD_DIR/kernel" "$KERNEL_ELF"

# The kernel embeds a symbol table generated from its own ELF. Both passes
# point slopos-ostd's build script at this file, which it tracks by content, so
# the second pass is a cache hit unless the symbols moved. Keyed by variant so
# the variants' different symbol sets do not invalidate each other.
KSYMS_RS="$BUILD_DIR/kallsyms-$VARIANT.rs"
if [ ! -f "$KSYMS_RS" ]; then
    printf 'pub static KERNEL_SYMBOLS: &[crate::ksym::KernelSymbol] = &[];\n' >"$KSYMS_RS"
fi

# For the machine running this build: no --target.
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" $CARGO build --locked --release -p slopos-kallsyms
KALLSYMS="$CARGO_TARGET_DIR/release/kallsyms"

# trim-paths so no absolute path of this checkout or its sysroot reaches the
# image: two checkouts, or the host and the guest, then build the same bytes.
# The future-incompat notice is core's stdarch enabling `sse` on this soft-float
# target (rust#117938): upstream's to fix, and repeated on every build.
build_kernel_once() {
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
        SLOPOS_KSYMS_RS="$KSYMS_RS" \
        RUSTFLAGS="${RUSTFLAGS:-} $KERNEL_RUSTFLAGS -Zunstable-options -Zemit-stack-sizes" \
        $CARGO build --locked $release \
        -Zbuild-std=core,alloc \
        -Zbuild-std-features=compiler-builtins-mem \
        -Zunstable-options \
        -Ztrim-paths \
        --config 'profile.dev.trim-paths="all"' \
        --config 'profile.release.trim-paths="all"' \
        --config 'future-incompat-report.frequency="never"' \
        --target "$RUST_TARGET" \
        --package kernel \
        --bin kernel \
        "--features=$FEATURES" \
        --artifact-dir "$BUILD_DIR"
    # --artifact-dir hard-links cargo's own output, so on a cache hit the
    # uplifted binary is already the ELF the first pass moved.
    if [ "$BUILD_DIR/kernel" -ef "$KERNEL_ELF" ]; then
        rm -f "$BUILD_DIR/kernel"
    elif [ -f "$BUILD_DIR/kernel" ]; then
        mv -f "$BUILD_DIR/kernel" "$KERNEL_ELF"
    fi
}

build_kernel_once
"$KALLSYMS" "$KERNEL_ELF" "$KSYMS_RS"
build_kernel_once

echo "build_kernel: $VARIANT kernel -> $KERNEL_ELF"
