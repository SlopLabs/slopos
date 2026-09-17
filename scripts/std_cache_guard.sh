#!/usr/bin/env bash
set -euo pipefail

# Reliable `-Zbuild-std` cache guard.
#
# Problem: cargo caches a compiled `libstd-<fingerprint>.rlib` in the build
# target dir. The fingerprint is derived from the std source files cargo knew
# about during the PREVIOUS build. When scripts/patch_std.sh adds a *new* std
# module file (e.g. sys/paths/slopos.rs), the old dep-info never listed it, so
# cargo concludes std is unchanged and silently reuses the stale rlib — the
# freshly-patched code is missing at link time even though the sysroot source
# is correct. Touching mtimes is racy and was observed to fail.
#
# Solution (content-addressed, deterministic): patch_std.sh writes a stamp
# hashing the patch script + all PAL sources. This guard compares that stamp
# against a per-target-dir copy. When they differ, the cached build-std outputs
# in this target dir are stale, so we DELETE them. Cargo always rebuilds a
# crate whose output artifact is missing (regardless of its fingerprint DB),
# which forces std (and its dependents) to recompile from the patched source.
#
# Usage: std_cache_guard.sh <cargo_target_dir> <target_triple>

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CARGO_TARGET_DIR="${1:?Usage: std_cache_guard.sh <cargo_target_dir> <target_triple>}"
TARGET_TRIPLE="${2:?Usage: std_cache_guard.sh <cargo_target_dir> <target_triple>}"

RUST_CHANNEL="${RUST_CHANNEL:-$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "${REPO_ROOT}/rust-toolchain.toml")}"
SYSROOT="$(rustc +"$RUST_CHANNEL" --print sysroot 2>/dev/null || rustc --print sysroot)"
PATCH_STAMP="$SYSROOT/lib/rustlib/src/rust/library/std/src/sys/.slopos_patch_stamp"

# No stamp ⇒ std was not patched (e.g. a core/alloc-only kernel build). The
# guard is a no-op; nothing to invalidate.
if [ ! -f "$PATCH_STAMP" ]; then
    exit 0
fi

want="$(cat "$PATCH_STAMP")"
local_stamp="$CARGO_TARGET_DIR/.slopos_std_stamp.${TARGET_TRIPLE}"
have="$(cat "$local_stamp" 2>/dev/null || true)"

if [ "$want" = "$have" ]; then
    # Cached build-std matches the currently-applied patches: keep the cache.
    exit 0
fi

# Both profiles: a userland build is `--release`, but a `debug` build-std tree
# survives from any non-release invocation, and the proof below searches the
# whole target dir. Purging one profile while checking two is how this guard
# came to fail on an unmodified tree.
STD_CRATES="std core alloc panic_abort panic_unwind"

for profile in release debug; do
    ROOT="$CARGO_TARGET_DIR/$TARGET_TRIPLE/$profile"
    [ -d "$ROOT" ] || continue
    DEPS="$ROOT/deps"
    FINGERPRINT="$ROOT/.fingerprint"
    # Since cargo 1.94 the `-Zbuild-std` sysroot units land in
    # `build/<crate>/<hash>/out/` and never in `deps/`, where the old purge
    # silently matched nothing.
    BUILD="$ROOT/build"

    # Build-std sysroot crates we patch into, plus the core sysroot crates
    # whose rebuild they may transitively require. Removing the output
    # rlib/rmeta forces a rebuild; removing the matching fingerprint dir keeps
    # cargo's bookkeeping consistent so it does not later think the
    # (now-deleted) artifact is fresh.
    for crate in $STD_CRATES; do
        if [ -d "$DEPS" ]; then
            rm -f "$DEPS/lib${crate}-"*.rlib "$DEPS/lib${crate}-"*.rmeta \
                  "$DEPS/${crate}-"*.d 2>/dev/null || true
        fi
        rm -rf "${BUILD:?}/$crate"
        if [ -d "$FINGERPRINT" ]; then
            # `find ... -exec rm -rf` tolerates the no-match case cleanly.
            find "$FINGERPRINT" -maxdepth 1 -type d -name "${crate}-*" \
                -exec rm -rf {} + 2>/dev/null || true
        fi
    done
done

# Layout-independent proof that the purge worked: a surviving `libstd` built
# from pre-patch PAL sources is exactly what this guard exists to prevent.
stale="$(find "$CARGO_TARGET_DIR/$TARGET_TRIPLE" -name 'libstd-*.rlib' -print -quit 2>/dev/null || true)"
if [ -n "$stale" ]; then
    echo "ERROR: std_cache_guard purged its known paths but a build-std libstd survives:" >&2
    echo "       $stale" >&2
    echo "       The cargo build-std output layout changed; update this script." >&2
    exit 1
fi

mkdir -p "$CARGO_TARGET_DIR"
echo "$want" > "$local_stamp"
echo "std_cache_guard: std patches changed → purged stale build-std artifacts for $TARGET_TRIPLE"
