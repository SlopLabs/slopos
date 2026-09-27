#!/usr/bin/env bash
# A `RUSTC_WRAPPER` that names a crate the same whichever triple it is built
# for.
#
# Usage: RUSTC_WRAPPER=scripts/rustc_neutral_metadata.sh <cargo or x.py ...>
#        (called as `rustc_neutral_metadata.sh <rustc> <args...>`)
#
# Cargo's `-C metadata` hashes the unit's target triple, and rustc hashes
# `-C metadata` into the crate's `StableCrateId`, which every mangled symbol
# carries: `core::fmt::Formatter::pad` is `_RNvMsa_NtCsgRlzlzJNmri_4core...`
# in the Linux std and `_RNvMsa_NtCs61faTTiSLg5_4core...` in the SlopOS one,
# out of one source tree and one compiler (measured). A PGO profile is keyed
# by those names, so a profile gathered on a Linux-hosted compiler matches
# nothing in a SlopOS-hosted one built from the same sources — unless both
# builds name their crates without the triple, which is what this does.
#
# The replacement is a hash of what identifies a crate within one crate graph
# and does not depend on the target: package, version, crate name, crate
# types, whether the unit is built for the host (no `--target`) or for the
# target, and `__CARGO_DEFAULT_LIB_METADATA`, which bootstrap sets to the
# version and a tag for std, rustc-private tools and codegen backends — the
# one thing that tells std's `cfg-if` from the compiler's, the same package,
# version and kind in one crate graph (measured: without it rustc stops at
# "colliding StableCrateId values"). Cargo builds one unit per package and
# kind — features are unified per kind — so no two crates in one graph share
# the rest. Features are left out on purpose: they may differ between the two
# builds (the profiling build's std carries `profiler`), and a crate has one
# StableCrateId either way. `-C extra-filename` is left alone, so cargo's file
# names do not change.
#
# Everything else is passed through untouched, including invocations without
# `-C metadata` (cargo's `-vV` and `--print` probes). An `@file` argument is
# read the way rustc reads one — bootstrap's shim hands the wrapper, the
# compiler's path included, an argument file once a line passes 1 MiB — and
# the rewritten line goes back to rustc in a file of its own.
set -euo pipefail

args=()
argfile=0
for arg in "$@"; do
    if [ "${arg#@}" != "$arg" ] && [ -f "${arg#@}" ]; then
        argfile=1
        while IFS= read -r line || [ -n "$line" ]; do
            args+=("$line")
        done <"${arg#@}"
    else
        args+=("$arg")
    fi
done
[ "${#args[@]}" -gt 0 ] || {
    echo "rustc_neutral_metadata: no compiler to run" >&2
    exit 1
}
rustc="${args[0]}"
args=("${args[@]:1}")

if [ -n "${CARGO_PKG_NAME:-}" ]; then
    kind=host
    types=""
    prev=""
    for arg in "${args[@]}"; do
        case "$prev" in
            --crate-type) types="$types,$arg" ;;
        esac
        case "$arg" in
            --target | --target=*) kind=target ;;
            --crate-type=*) types="$types,${arg#--crate-type=}" ;;
        esac
        prev="$arg"
    done
    meta="$(printf '%s\n' "$CARGO_PKG_NAME" "${CARGO_PKG_VERSION:-}" "${CARGO_CRATE_NAME:-}" "$types" "$kind" \
        "${__CARGO_DEFAULT_LIB_METADATA:-}" | sha256sum)"
    meta="${meta:0:16}"

    prev=""
    for i in "${!args[@]}"; do
        arg="${args[$i]}"
        if [ "$prev" = "-C" ] && [ "${arg#metadata=}" != "$arg" ]; then
            args[i]="metadata=$meta"
        elif [ "${arg#-Cmetadata=}" != "$arg" ]; then
            args[i]="-Cmetadata=$meta"
        fi
        prev="$arg"
    done
fi

if [ "$argfile" -eq 0 ]; then
    exec "$rustc" ${args[@]+"${args[@]}"}
fi
tmp="$(mktemp "${TMPDIR:-/tmp}/rustc-args.XXXXXX")"
trap 'rm -f "$tmp"' EXIT
printf '%s\n' ${args[@]+"${args[@]}"} >"$tmp"
"$rustc" "@$tmp"
