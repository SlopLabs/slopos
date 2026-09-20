#!/usr/bin/env bash
# Hold the built-in `x86_64-unknown-slopos` target to the JSON spec this tree
# builds with, and to rustc's own rules for a built-in target.
#
# Why a gate. The compiler fork (toolchain/compiler/) exists so bootstrap can
# resolve `--host=x86_64-unknown-slopos`, which it can only do through rustc's
# built-in target list. That leaves two specs describing one machine — the
# built-in one and targets/x86_64-unknown-slopos.json, which every userland
# build still passes by path — and a disagreement between them fails to
# compile nowhere. A cross-built toolchain would simply produce binaries for a
# slightly different target than the one the tree tests.
#
# Three questions, all silent otherwise:
#
#   1. Is the tuple registered? `--host` resolves through `TARGETS`, so a
#      patch that adds the target module and forgets `supported_targets!`
#      builds a compiler that cannot host this target and says so only at the
#      bootstrap run that was waiting on it.
#   2. Does the built-in spec equal the JSON one? Compared as
#      `Target::to_json()` on both sides, which is rustc's own normalisation:
#      defaults elided, linker flavor resolved to its CLI spelling. Every
#      field, not a chosen subset.
#   3. Does the target still allow dynamic linking? `rustc_driver` is
#      `crate-type = ["dylib"]`, so a host rustc needs it, and `libc.so` is a
#      `cdylib` against the same permission.
#
# Question 2 is why the JSON says `relocation-model: pic`: rustc refuses a
# built-in target that allows dynamic linking under any other relocation
# model, and the tree's static images pin `-C relocation-model=static` on the
# build line instead (scripts/build_userland.sh).
#
# rustc's own per-target test runs last: `spec::tests::<module>` drives
# `check_consistency(TargetKind::Builtin)` plus a JSON round trip, and it is
# what upstream CI runs on a patch like this one.
#
# The gate reports `skipped` when the source tree has not been materialised,
# because `scripts/make_rustc_src.sh` fetches 265 MB and unpacks 656 MiB, which
# does not belong on every checkout. `--require` turns that into a failure,
# for the CI job whose only reason to exist is asking these questions.
#
# Usage:
#     scripts/check_rustc_target.sh
#     scripts/check_rustc_target.sh --require
#     scripts/check_rustc_target.sh --self-test
#
# Environment:
#   RUSTC_SRC_DIR - the materialised source tree (default:
#                   third_party/slopos-rustc-src); the self-test points it at
#                   a tree that is not there.

set -euo pipefail

SELF="check_rustc_target"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

TUPLE="x86_64-unknown-slopos"
CONSISTENCY_TEST="spec::tests::x86_64_unknown_slopos"
TARGET_JSON="$REPO_ROOT/targets/$TUPLE.json"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
SRC="${RUSTC_SRC_DIR:-$REPO_ROOT/$TP_RUSTC_SRC_REL}"
# Keyed by the tree: the manifest names it, so two runs over different trees
# sharing one BUILD_DIR would otherwise rewrite each other's probe and grade
# the wrong tree.
PROBE_DIR="$BUILD_DIR/gates/rustc-target-probe-$(printf '%s' "$SRC" | tp_sha256_stream | cut -c1-12)"

REQUIRE=0
SELF_TEST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --require)   REQUIRE=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        *) echo "$SELF: unknown option $1" >&2; exit 2 ;;
    esac
done

RUST_CHANNEL="$(tp_channel "$REPO_ROOT")"

# The compiler workspace refuses to build outside bootstrap, and `rustc_span`
# reads the release it was built for out of the environment. Both are
# bootstrap's to supply; the source tree is where the values come from. Lints
# are capped because these are somebody else's sources: every compiler crate
# carries `cfg(bootstrap)`, which is a name only bootstrap declares.
compiler_env() {
    local version
    version="$(cat "$SRC/version")"
    env RUSTC_BOOTSTRAP=1 \
        RUSTFLAGS="--cap-lints=allow" \
        CFG_VERSION="$version" \
        CFG_RELEASE="${version%% *}" \
        CFG_RELEASE_CHANNEL="$(cat "$SRC/src/ci/channel")" \
        "$@"
}

# Written out rather than tracked as a workspace member, as the codegen-backend
# probe is: a crate under scripts/ would either join this workspace and be
# built for the host on every `cargo build`, or sit outside it and drift from
# the gate that reads it. It also path-depends on a tree that is usually
# absent, which no tracked manifest can express.
write_probe_crate() {
    mkdir -p "$PROBE_DIR/src"
    cat > "$PROBE_DIR/Cargo.toml" <<EOF
[workspace]

[package]
name = "slopos-target-probe"
version = "0.0.0"
edition = "2024"

[dependencies]
rustc_target = { path = "$SRC/compiler/rustc_target" }
EOF
    # The tree's own lockfile, so the probe grades the `rustc_target` bootstrap
    # would build rather than whatever crates.io resolves today.
    cp "$SRC/Cargo.lock" "$PROBE_DIR/Cargo.lock" || {
        echo "$SELF: FAIL — $SRC has no Cargo.lock to pin the probe's dependencies" >&2
        return 1
    }
    cat > "$PROBE_DIR/src/main.rs" <<'EOF'
use rustc_target::json::ToJson;
use rustc_target::spec::{Target, TargetTuple};

fn main() -> std::process::ExitCode {
    let mut args = std::env::args().skip(1);
    let tuple = args.next().expect("usage: probe <tuple> <target.json>");
    let path = args.next().expect("usage: probe <tuple> <target.json>");
    let mut failures = 0;

    if !rustc_target::spec::TARGETS.contains(&tuple.as_str()) {
        println!("FAIL {tuple} is not a built-in target");
        return std::process::ExitCode::FAILURE;
    }

    let builtin = Target::expect_builtin(&TargetTuple::from_tuple(&tuple));
    if !builtin.dynamic_linking {
        println!("FAIL {tuple} does not allow dynamic linking");
        failures += 1;
    }

    let text = std::fs::read_to_string(&path).expect("target JSON");
    let (spec, _) = match Target::from_json(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            println!("FAIL {path} does not parse as a target spec: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let builtin = builtin.to_json();
    let spec = spec.to_json();
    let (builtin, spec) = (builtin.as_object().unwrap(), spec.as_object().unwrap());
    let mut keys: Vec<_> = builtin.keys().chain(spec.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        let (lhs, rhs) = (builtin.get(key), spec.get(key));
        if lhs != rhs {
            println!("FAIL {key}: built-in {lhs:?}, {path} {rhs:?}");
            failures += 1;
        }
    }

    if failures > 0 {
        return std::process::ExitCode::FAILURE;
    }
    println!("OK {tuple}: built in, dynamically linkable, and equal to {path}");
    std::process::ExitCode::SUCCESS
}
EOF
}

build_probe() {
    write_probe_crate || return 1
    compiler_env env CARGO_TARGET_DIR="$PROBE_DIR/target" \
        cargo "+$RUST_CHANNEL" build --quiet \
        --manifest-path "$PROBE_DIR/Cargo.toml" ||
        { echo "$SELF: FAIL — the probe does not build against $SRC/compiler/rustc_target" >&2; return 1; }
}

run_probe() {
    compiler_env env CARGO_TARGET_DIR="$PROBE_DIR/target" \
        cargo "+$RUST_CHANNEL" run --quiet \
        --manifest-path "$PROBE_DIR/Cargo.toml" -- "$TUPLE" "$1"
}

# libtest exits 0 when a name filter matches nothing, and the name is the
# target *module*, which a rebase can rename while the tuple survives. So the
# pass is read out of the output rather than out of the exit code.
run_consistency_test() {
    local name="$1" out
    if ! out="$(compiler_env env CARGO_TARGET_DIR="$BUILD_DIR/gates/rustc-target-test" \
            cargo "+$RUST_CHANNEL" test --quiet --manifest-path "$SRC/Cargo.toml" \
            -p rustc_target --lib -- --exact "$name" 2>&1)" ||
        ! printf '%s\n' "$out" | grep -q '^test result: ok\. 1 passed'; then
        printf '%s\n' "$out" >&2
        return 1
    fi
}

# `Os` and `Env` are matched exhaustively outside `rustc_target` — librustdoc
# turns each into a human-readable name — so adding a variant stops another
# crate compiling, which building `rustc_target` alone cannot see and a whole
# compiler build is too expensive to ask per run. Every match arm over the
# neighbouring variant is therefore required to have one over ours.
enum_arms_covered() {
    local tree="$1" pair neighbour ours file seen bad=0
    for pair in "Rtems:Slopos" "Sim:Slibc"; do
        neighbour="${pair%%:*}"
        ours="${pair##*:}"
        seen=0
        for file in $(grep -rl "^ *$neighbour => " "$tree/compiler" "$tree/src" \
                --include='*.rs' 2>/dev/null); do
            seen=$((seen + 1))
            grep -q "^ *$ours => " "$file" || {
                echo "$SELF: FAIL — ${file#"$tree/"} matches $neighbour and not $ours
       It is an exhaustive match over an enum the patch extends." >&2
                bad=1
            }
        done
        # A pattern that matches nothing grades nothing: the arms moved, were
        # written qualified, or the tree is not shaped as this expects.
        if [ "$seen" -eq 0 ]; then
            echo "$SELF: FAIL — no match arm over $neighbour anywhere in the tree
       The check for $ours graded nothing; re-derive it against the sources." >&2
            bad=1
        fi
    done
    return "$bad"
}

run_gate() {
    if [ ! -d "$SRC" ]; then
        if [ "$REQUIRE" -eq 1 ]; then
            echo "$SELF: FAIL — no source tree at $SRC; run scripts/make_rustc_src.sh" >&2
            return 1
        fi
        echo "$SELF: skipped — no source tree at $SRC (scripts/make_rustc_src.sh)"
        return 0
    fi

    [ -f "$TARGET_JSON" ] || { echo "$SELF: missing targets/$TUPLE.json" >&2; return 1; }

    if [ "$(cat "$SRC/$TP_STAMP_NAME" 2>/dev/null)" != "$(tp_rustc_stamp "$REPO_ROOT")" ]; then
        echo "$SELF: FAIL — $SRC was not built from the current toolchain/compiler/ overlay
       Re-run scripts/make_rustc_src.sh; a stale tree grades the previous fork." >&2
        return 1
    fi

    build_probe || return 1
    if ! run_probe "$TARGET_JSON"; then
        echo "$SELF: FAIL — the built-in target and targets/$TUPLE.json describe different machines
       Change both or neither: toolchain/compiler/0001-slopos-target.patch and
       targets/$TUPLE.json, then re-run scripts/make_rustc_src.sh." >&2
        return 1
    fi

    if ! run_consistency_test "$CONSISTENCY_TEST"; then
        echo "$SELF: FAIL — rustc's own consistency test did not run, or rejects the built-in target" >&2
        return 1
    fi

    enum_arms_covered "$SRC" || return 1

    echo "$SELF: OK — $TUPLE is built in, agrees with targets/$TUPLE.json, and passes rustc's consistency test"
    return 0
}

# ---------------------------------------------------------------------------
# Self-test. The skip and `--require` paths are graded on every host, because
# a checkout with no source tree takes them; the comparison is graded against
# a planted disagreement whenever there is a tree to grade it against.
# ---------------------------------------------------------------------------
self_test() {
    local scratch="$BUILD_DIR/$SELF-selftest" failed=0
    rm -rf "$scratch"
    mkdir -p "$scratch"
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
        echo "  case no-source-tree-required: --require fails where a skip would hide it"
    fi

    if [ -d "$SRC" ]; then
        build_probe || { rm -rf "$scratch"; return 1; }
        sed 's/"max-atomic-width": 64/"max-atomic-width": 32/' "$TARGET_JSON" > "$scratch/drifted.json"
        if run_probe "$scratch/drifted.json" >"$scratch/drifted.log" 2>&1; then
            echo "$SELF --self-test: a JSON spec disagreeing on max-atomic-width was accepted" >&2
            failed=1
        elif ! grep -q '^FAIL max-atomic-width' "$scratch/drifted.log"; then
            echo "$SELF --self-test: the disagreement was rejected without naming the field" >&2
            sed 's/^/      /' "$scratch/drifted.log" >&2
            failed=1
        else
            echo "  case drifted-json: names the field the two specs disagree on"
        fi

        if run_probe "$TARGET_JSON" >/dev/null 2>&1; then
            echo "  case tracked-json: silent on the spec pair the tree ships"
        else
            echo "$SELF --self-test: the tracked JSON was rejected" >&2
            failed=1
        fi

        if run_consistency_test "spec::tests::x86_64_unknown_nosuch" >/dev/null 2>&1; then
            echo "$SELF --self-test: a consistency test that matched nothing was accepted" >&2
            failed=1
        else
            echo "  case vanished-test: a filter matching no test fails, where libtest exits 0"
        fi

        local stale_out
        mkdir -p "$scratch/stale"
        printf '%s\n' "$(printf '%064d' 1)" > "$scratch/stale/$TP_STAMP_NAME"
        stale_out="$(RUSTC_SRC_DIR="$scratch/stale" "$SCRIPT_DIR/$SELF.sh" --require 2>&1 || true)"
        if printf '%s\n' "$stale_out" | grep -q 'not built from the current'; then
            echo "  case stale-tree: refuses a tree the current overlay did not build"
        else
            echo "$SELF --self-test: a tree stamped by another overlay was accepted" >&2
            failed=1
        fi

        mkdir -p "$scratch/arms/compiler" "$scratch/arms/src"
        printf '        Rtems => "RTEMS OS",\n' > "$scratch/arms/src/cfg.rs"
        if enum_arms_covered "$scratch/arms" 2>/dev/null; then
            echo "$SELF --self-test: a match naming Rtems and not Slopos was accepted" >&2
            failed=1
        else
            echo "  case uncovered-arm: catches an exhaustive match the patch did not extend"
        fi
        mkdir -p "$scratch/noarms/compiler" "$scratch/noarms/src"
        if enum_arms_covered "$scratch/noarms" 2>/dev/null; then
            echo "$SELF --self-test: a tree with no match arm at all was accepted" >&2
            failed=1
        else
            echo "  case no-arms: fails closed when the pattern grades nothing"
        fi

        if enum_arms_covered "$SRC"; then
            echo "  case covered-arms: silent on the tree the patch produced"
        else
            echo "$SELF --self-test: the patched tree has an uncovered match arm" >&2
            failed=1
        fi
    else
        echo "  cases drifted-json, tracked-json, vanished-test, stale-tree, uncovered-arm, no-arms: skipped, no source tree"
    fi

    rm -rf "$scratch"
    trap - EXIT INT TERM
    if [ "$failed" -ne 0 ]; then
        echo "$SELF: SELF-TEST FAILED — the gate does not catch what it claims to" >&2
        return 1
    fi
    echo "$SELF: self-test OK"
    return 0
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
else
    run_gate
fi
