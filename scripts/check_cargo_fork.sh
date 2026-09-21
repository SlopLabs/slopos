#!/usr/bin/env bash
set -euo pipefail

# Hold the SlopOS cargo fork to the cut it claims.
#
# Usage: check_cargo_fork.sh [--require] [--self-test]
#
# `toolchain/cargo/` puts curl, libgit2 and OpenSSL behind a `network`
# feature, because `x86_64-unknown-slopos` has none of the six C libraries
# they close over and bootstrap builds cargo for the host it is asked for.
# Two things break that and neither fails to compile on Linux:
#
#   * A rebase onto a newer cargo re-introduces one of those crates on the
#     offline path — a new dependency, or an existing one losing its
#     `optional = true`. The Linux build is unaffected; the slopos build
#     stops at a C compiler it does not have, hours into a bootstrap run.
#   * The `#[cfg(feature = "network")]` cut stops compiling. Upstream moves
#     code across the line often, and every default build stays green.
#
# So the gate reads the dependency closure on both sides of the feature and
# then compiles the offline side. The closure check is the cheap half and
# catches the first; the compile is the expensive half and catches the second.
#
# `skipped` without a materialised source tree, since a checkout that has not
# fetched 265 MB still has a consistent pin; the CI step that has one passes
# `--require`. `--self-test` grades the skip and `--require` paths on every
# host, and the closure check against crafted metadata wherever cargo runs.

SELF="check_cargo_fork"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

REQUIRE=0
SELF_TEST=0
for arg in "$@"; do
    case "$arg" in
        --require) REQUIRE=1 ;;
        --self-test) SELF_TEST=1 ;;
        *) die "unknown argument: $arg" ;;
    esac
done

skip() {
    [ "$REQUIRE" -eq 0 ] || die "$1"
    echo "$SELF: skipped — $1"
    exit 0
}

SRC="${RUSTC_SRC_DIR:-$REPO_ROOT/$TP_RUSTC_SRC_REL}"
CARGO_SRC="$SRC/$TP_CARGO_TREE_REL"
BUILD="${BUILD_DIR:-$REPO_ROOT/builddir}/gates/cargo-fork"

# Every crate that builds or links a C library, which the offline closure
# must not contain. `openssl-probe` is not among them: it looks for a
# certificate store and builds nothing.
C_LIBRARIES="curl curl-sys git2 git2-curl libgit2-sys libssh2-sys libz-sys libnghttp2-sys openssl openssl-sys openssl-src"
# The subset a *default* cargo pulls, which is the positive control: a run
# where neither closure carries a C library is a gate that has stopped
# reading the manifest. `openssl` and `openssl-src` are not in it because
# upstream already gates those on `vendored-openssl`.
DEFAULT_C_LIBRARIES="curl curl-sys git2 git2-curl libgit2-sys libssh2-sys libz-sys libnghttp2-sys openssl-sys"
# SQLite stays: it is one amalgamated C file with no build system and it
# compiles for this target against slibc's headers. Losing it would make
# `toolchain/cargo/PIN` say something untrue.
REQUIRED="libsqlite3-sys"

SKIP_REASON=""

inputs_ready() {
    if [ ! -f "$CARGO_SRC/Cargo.toml" ]; then
        SKIP_REASON="no cargo sources at $CARGO_SRC — run scripts/make_rustc_src.sh"
        return 1
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is required"
    return 0
}

# The package names cargo itself depends on, one per line. `cargo tree` and
# not `cargo metadata`: metadata resolves the whole workspace, including the
# test-support crates whose own dependencies are nobody's concern here, and
# its `--no-default-features` then describes a graph the `cargo` binary is
# only part of. No `--target`, so the closure is the union over platforms and
# a C crate hiding behind any `cfg` is still caught.
closure_names() {
    local out="$1"
    shift
    (cd "$CARGO_SRC" && cargo tree -p cargo -e normal,build --prefix none "$@") \
        2>"$out.err" | awk 'NF { print $1 }' | LC_ALL=C sort -u >"$out" || {
        tail -n 5 "$out.err" >&2
        die "cargo tree failed for the cargo fork; see $out.err"
    }
    [ -s "$out" ] || die "cargo tree printed no package names; see $out.err"
}

grade_offline() {
    local names="$1" bad=0 crate
    for crate in $C_LIBRARIES; do
        if grep -qx "$crate" "$names"; then
            echo "  $crate is in the offline dependency closure" >&2
            bad=1
        fi
    done
    for crate in $REQUIRED; do
        if ! grep -qx "$crate" "$names"; then
            echo "  $crate left the closure — toolchain/cargo/PIN says SQLite stays" >&2
            bad=1
        fi
    done
    return "$bad"
}

grade_default() {
    local names="$1" bad=0 crate
    for crate in $DEFAULT_C_LIBRARIES; do
        if ! grep -qx "$crate" "$names"; then
            echo "  $crate is absent from the default dependency closure" >&2
            bad=1
        fi
    done
    return "$bad"
}

run_gate() {
    inputs_ready || skip "$SKIP_REASON"

    local want
    want="$(tp_rustc_stamp "$REPO_ROOT")"
    [ "$(cat "$SRC/$TP_STAMP_NAME" 2>/dev/null)" = "$want" ] ||
        die "$TP_RUSTC_SRC_REL was not built from the current toolchain/{compiler,cargo}/ overlay — run scripts/make_rustc_src.sh"

    mkdir -p "$BUILD"
    closure_names "$BUILD/names-default.txt"
    closure_names "$BUILD/names-offline.txt" --no-default-features

    grade_default "$BUILD/names-default.txt" ||
        die "the default cargo build no longer pulls the C libraries this fork drops"
    grade_offline "$BUILD/names-offline.txt" ||
        die "the \`network\` cut no longer drops every C library"

    (cd "$CARGO_SRC" && CARGO_TARGET_DIR="$BUILD/target" \
        cargo check --locked -p cargo --no-default-features --message-format short) \
        >"$BUILD/check.log" 2>&1 || {
        grep -E ': error' "$BUILD/check.log" | head -n 20 >&2
        die "cargo does not compile without the \`network\` feature; see $BUILD/check.log"
    }

    local dropped
    dropped="$(LC_ALL=C comm -13 "$BUILD/names-offline.txt" "$BUILD/names-default.txt" | wc -l | tr -d ' ')"
    echo "$SELF: the \`network\` cut compiles and drops $dropped crates, every C library among them"
}

self_test() {
    local failed=0 scratch
    scratch="$(mktemp -d)"
    trap 'rm -rf "$scratch"' EXIT INT TERM

    if RUSTC_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" >/dev/null 2>&1; then
        echo "  case no-source-tree: skipped rather than failed"
    else
        echo "$SELF --self-test: a checkout with no source tree was failed" >&2
        failed=1
    fi

    if RUSTC_SRC_DIR="$scratch/absent" "$SCRIPT_DIR/$SELF.sh" --require >/dev/null 2>&1; then
        echo "$SELF --self-test: --require passed with no source tree" >&2
        failed=1
    else
        echo "  case require-no-source-tree: failed rather than skipped"
    fi

    # The graders, against closures this test writes rather than against the
    # tree: planting a violation in the shared source tree would leave it
    # unported if the run died between planting and repairing.
    printf '%s\n' libsqlite3-sys serde >"$scratch/clean.txt"
    if grade_offline "$scratch/clean.txt" 2>/dev/null; then
        echo "  case clean-closure: accepted a closure with no C library in it"
    else
        echo "$SELF --self-test: a clean closure was rejected" >&2
        failed=1
    fi

    printf '%s\n' libsqlite3-sys openssl-sys serde >"$scratch/dirty.txt"
    if grade_offline "$scratch/dirty.txt" 2>/dev/null; then
        echo "$SELF --self-test: a closure carrying openssl-sys was accepted" >&2
        failed=1
    else
        echo "  case c-library-back: rejected a closure a dependency bump put openssl-sys into"
    fi

    printf '%s\n' serde >"$scratch/nosqlite.txt"
    if grade_offline "$scratch/nosqlite.txt" 2>/dev/null; then
        echo "$SELF --self-test: a closure with no libsqlite3-sys was accepted" >&2
        failed=1
    else
        echo "  case sqlite-gone: rejected a closure the PIN's SQLite claim no longer describes"
    fi

    # The other direction: a default closure that has stopped carrying the C
    # libraries means the gate is reading something that is not cargo.
    printf '%s\n' libsqlite3-sys serde >"$scratch/empty-default.txt"
    if grade_default "$scratch/empty-default.txt" 2>/dev/null; then
        echo "$SELF --self-test: a default closure with no C library was accepted" >&2
        failed=1
    else
        echo "  case default-closure-empty: rejected a default closure that drops the C libraries"
    fi

    # The extractor against the real tree, so a `cargo tree` format change is
    # a failure here rather than an empty closure everywhere.
    if inputs_ready; then
        closure_names "$scratch/real.txt"
        if grep -qx cargo "$scratch/real.txt" && [ "$(wc -l <"$scratch/real.txt")" -gt 100 ]; then
            echo "  case closure-shape: read $(wc -l <"$scratch/real.txt" | tr -d ' ') package names out of cargo tree"
        else
            echo "$SELF --self-test: could not read package names out of cargo tree" >&2
            failed=1
        fi
    else
        echo "  case closure-shape: skipped — $SKIP_REASON"
    fi

    rm -rf "$scratch"
    trap - EXIT INT TERM
    if [ "$failed" -ne 0 ]; then
        echo "$SELF: SELF-TEST FAILED — the gate does not catch what it claims to" >&2
        return 1
    fi
    echo "$SELF: self-test OK"
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
else
    run_gate
fi
