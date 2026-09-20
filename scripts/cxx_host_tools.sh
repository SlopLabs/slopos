#!/usr/bin/env bash
set -euo pipefail

# Resolve the host LLVM toolchain that cross-builds the C++ runtime.
#
# Usage: eval "$(cxx_host_tools.sh)"       # CLANG/CLANGXX/LD_LLD/LLVM_AR/CXX_HOST_MAJOR
#        cxx_host_tools.sh --describe      # one human line, for a build log
#
# `toolchain/cxx/PIN` pins the llvm-project *sources* and a *minimum* host
# major, and those two are different kinds of pin on purpose. The sources fix
# the C++ ABI and the set of libc symbols `libc++.so` ends up needing, which is
# the part slibc has to keep up with; the host compiler only codegens them. A
# newer major is therefore accepted and an older one is not — libc++'s sources
# use its own release's clang.
#
# The alternative — demanding the sources' exact major — is what shipped first,
# and it is unbuildable on a rolling distribution: no Linux ships LLVM 18 by
# default any more (Arch is at 22, Fedora 20, Debian 13 at 19), Arch has no
# versioned llvm18 package in either the repositories or the AUR, and building
# one from source to compile a 700 KiB archive is an hour of CPU for nothing.
#
# The four tools must come from **one** major: a host with clang 18 and lld 22
# on PATH would otherwise link the runtime with a mismatched linker. The
# sources' own major is preferred wherever it is installed (Debian's
# `clang-18`, apt.llvm.org's `/usr/lib/llvm-18/bin`, Fedora's `llvm18` compat
# tree), because that is the pairing upstream tests; the unsuffixed default is
# next; any other installed major above the floor is last.
#
# The env vars are a full override and never fall back: a named toolchain that
# fails the checks is an error, not a reason to pick another one.

SELF="cxx_host_tools"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PIN="$REPO_ROOT/toolchain/cxx/PIN"

die() {
    echo "$SELF: $*" >&2
    exit 1
}

[ -f "$PIN" ] || die "missing toolchain/cxx/PIN"

pin_value() {
    sed -n "s/^$1=\\(.*\\)$/\\1/p" "$PIN" | head -n 1
}

MIN_MAJOR="$(pin_value clang_major_min)"
TESTED_MAJORS="$(pin_value clang_major_tested)"
SOURCE_MAJOR="$(pin_value llvm_version)"
SOURCE_MAJOR="${SOURCE_MAJOR%%.*}"
case "$MIN_MAJOR" in
    '' | *[!0-9]*) die "toolchain/cxx/PIN has no numeric clang_major_min" ;;
esac
[ -n "$SOURCE_MAJOR" ] || die "toolchain/cxx/PIN has no llvm_version"

tool_major() {
    local text
    text="$("$1" --version 2>/dev/null)" || return 1
    text="$(printf '%s\n' "$text" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | sed -n 1p)"
    [ -n "$text" ] || return 1
    printf '%s\n' "${text%%.*}"
}

# Echoes the shared major on success; silent failure is "this set is not it".
resolve_set() {
    local agreed="" tool major
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || return 1
        major="$(tool_major "$tool")" || return 1
        if [ -z "$agreed" ]; then
            agreed="$major"
        elif [ "$major" != "$agreed" ]; then
            return 1
        fi
    done
    [ "$agreed" -ge "$MIN_MAJOR" ] || return 1
    printf '%s\n' "$agreed"
}

# Where a distribution puts a non-default major: Debian and apt.llvm.org use
# both a suffix and a versioned prefix, Fedora's compat packages only the
# prefix and without the dash.
major_dirs() {
    local m="$1"
    printf '%s\n' \
        "/usr/lib/llvm-$m/bin" \
        "/usr/lib64/llvm-$m/bin" \
        "/usr/lib/llvm$m/bin" \
        "/usr/lib64/llvm$m/bin" \
        "/opt/llvm-$m/bin" \
        "/usr/local/llvm-$m/bin"
}

CLANG_PATH=""
CLANGXX_PATH=""
LD_LLD_PATH=""
LLVM_AR_PATH=""
HOST_MAJOR=""

# Sets the four paths and HOST_MAJOR if this set resolves.
try_set() {
    local major
    major="$(resolve_set "$1" "$2" "$3" "$4")" || return 1
    CLANG_PATH="$1"
    CLANGXX_PATH="$2"
    LD_LLD_PATH="$3"
    LLVM_AR_PATH="$4"
    HOST_MAJOR="$major"
}

try_major() {
    local m="$1" dir
    try_set "clang-$m" "clang++-$m" "ld.lld-$m" "llvm-ar-$m" && return 0
    while read -r dir; do
        try_set "$dir/clang" "$dir/clang++" "$dir/ld.lld" "$dir/llvm-ar" && return 0
    done < <(major_dirs "$m")
    return 1
}

installed_majors() {
    {
        local dir file base
        while read -r dir; do
            [ -d "$dir" ] || continue
            for file in "$dir"/clang-[0-9]*; do
                [ -x "$file" ] || continue
                printf '%s\n' "${file##*/clang-}"
            done
        done < <(printf '%s\n' "$PATH" | tr ':' '\n')
        for dir in /usr/lib/llvm-[0-9]*/bin /usr/lib64/llvm-[0-9]*/bin \
            /usr/lib/llvm[0-9]*/bin /usr/lib64/llvm[0-9]*/bin /opt/llvm-[0-9]*/bin; do
            [ -x "$dir/clang" ] || continue
            base="${dir%/bin}"
            base="${base##*/llvm}"
            printf '%s\n' "${base#-}"
        done
    } | grep -xE '[0-9]+' | sort -n -u
}

if [ -n "${CLANG:-}${CLANGXX:-}${LD_LLD:-}${LLVM_AR:-}" ]; then
    try_set "${CLANG:-clang}" "${CLANGXX:-clang++}" "${LD_LLD:-ld.lld}" "${LLVM_AR:-llvm-ar}" ||
        die "the named toolchain is not one usable LLVM >= $MIN_MAJOR
       CLANG=${CLANG:-clang} CLANGXX=${CLANGXX:-clang++} LD_LLD=${LD_LLD:-ld.lld} LLVM_AR=${LLVM_AR:-llvm-ar}
       All four must exist and report the same major, at or above $MIN_MAJOR."
else
    discover() {
        local candidate
        try_major "$SOURCE_MAJOR" && return 0
        try_set clang clang++ ld.lld llvm-ar && return 0
        while read -r candidate; do
            [ "$candidate" -ge "$MIN_MAJOR" ] || continue
            try_major "$candidate" && return 0
        done < <(installed_majors)
        return 1
    }
    discover || true
    [ -n "$HOST_MAJOR" ] || die "no LLVM >= $MIN_MAJOR toolchain found
       The C++ runtime is cross-built from clang, clang++, ld.lld and llvm-ar
       of one major, plus cmake and ninja. Install them:
         Arch / CachyOS   pacman -S clang lld llvm cmake ninja
         Debian/Ubuntu    apt install clang lld llvm cmake ninja-build
                          (or apt.llvm.org for a specific major: clang-$SOURCE_MAJOR lld-$SOURCE_MAJOR llvm-$SOURCE_MAJOR)
         Fedora           dnf install clang lld llvm cmake ninja-build
         openSUSE         zypper install clang lld llvm cmake ninja
         Nix              nix shell nixpkgs#llvmPackages_$SOURCE_MAJOR.clang nixpkgs#llvmPackages_$SOURCE_MAJOR.bintools
       CLANG/CLANGXX/LD_LLD/LLVM_AR override the names."
fi

case " $TESTED_MAJORS " in
    *" $HOST_MAJOR "*) ;;
    *)
        echo "$SELF: note: LLVM $HOST_MAJOR is outside toolchain/cxx/PIN's tested majors ($TESTED_MAJORS)." >&2
        echo "$SELF: note: the C++ probes in \`just test\` are what grade it; add $HOST_MAJOR to the pin once they pass." >&2
        ;;
esac

if [ "${1:-}" = "--describe" ]; then
    printf 'LLVM %s (%s) building llvm-project %s sources\n' \
        "$HOST_MAJOR" "$(command -v "$CLANGXX_PATH")" "$(pin_value llvm_version)"
    exit 0
fi

printf 'CLANG=%q\n' "$CLANG_PATH"
printf 'CLANGXX=%q\n' "$CLANGXX_PATH"
printf 'LD_LLD=%q\n' "$LD_LLD_PATH"
printf 'LLVM_AR=%q\n' "$LLVM_AR_PATH"
printf 'CXX_HOST_MAJOR=%q\n' "$HOST_MAJOR"
