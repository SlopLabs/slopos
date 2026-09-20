#!/usr/bin/env bash
# Verify the cross-built C++ runtime against its pin and against the C library.
#
# Three failures, all silent, all measured rather than reviewed.
#
#   1. **The pin stops describing what is on disk.** `toolchain/cxx/PIN` names
#      the llvm-project release, its checksum and the clang major the runtime
#      is built with. A `third_party/slopos-cxx` built from something else
#      still links; it just links a different C++ ABI.
#   2. **The built tree stops matching its inputs.** `make_slopos_cxx.sh`
#      rebuilds on a stamp over the pin, its own build line and slibc's C
#      headers; a tree built before one of those moved is a C++ runtime
#      compiled against headers the C library no longer has. Asked of the
#      build script rather than recomputed here, so there is no second copy of
#      that digest to drift.
#   3. **`libc++.so` names a symbol `libc.so` does not export.** The C++
#      library is linked without `-z defs`, because an undefined symbol in a
#      shared object is legal and the loader resolves it at load time — so a
#      libc gap that would have been a link error here is instead a `dlopen`
#      that fails on a machine, at the point the runtime is first needed. This
#      is the measurement Workstream 1.1 produced, kept as a check: every
#      symbol the C++ runtime needs from the C library, still there.
#
# The first is unconditional. The other two need the tree to have been built
# *in this checkout*, which a `builddir/libc.so` is what says: a CI job
# restores `third_party/slopos-cxx` from a cache before the step that would
# refresh it, and a stamp failure there is a red run no later step can repair.
# So a checkout that has never built the userland passes on the pin alone.
#
# `MIN_UNDEFINED` is the floor that stops a measurement which stopped
# happening from reading as one that got free: a truncated `libc++.so`, or an
# `nm` that failed, yields an empty symbol list, and an empty list is a subset
# of everything.
#
# Usage: check_cxx_pin.sh [--libc <path to libc.so>]
#        check_cxx_pin.sh --self-test

set -euo pipefail

SELF="check_cxx_pin"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MIN_UNDEFINED=40

fail() {
    echo "$SELF: $*" >&2
    exit 1
}

# `nm` into a sorted symbol list, with its own failure reported rather than
# swallowed: process substitution hides an exit status from `pipefail`, and an
# empty list would otherwise pass every comparison below.
symbols() {
    local kind="$1" object="$2" out="$3"
    nm -D "--${kind}-only" "$object" >"$out.raw" ||
        fail "nm --${kind}-only failed on $object"
    awk '{print $NF}' "$out.raw" | sort -u >"$out"
    [ -s "$out" ] || fail "$object lists no ${kind} dynamic symbols"
}

# The comparison itself, over two sorted lists, so `--self-test` can drive it
# with crafted ones.
compare_symbols() {
    local undefined="$1" defined="$2"
    local count missing
    count="$(wc -l <"$undefined")"
    if [ "$count" -lt "$MIN_UNDEFINED" ]; then
        fail "libc++.so lists $count undefined symbols, under the floor of $MIN_UNDEFINED
       That is not a smaller C++ runtime, it is a measurement that stopped
       happening — a truncated object, or an nm that read nothing."
    fi
    missing="$(comm -23 "$undefined" "$defined")"
    if [ -n "$missing" ]; then
        fail "libc++.so needs symbols libc.so does not export:
$(echo "$missing" | sed 's/^/       /')
       Each one is a C library entry point the C++ runtime calls and slibc has
       not got. Implement it, or rebuild both after changing what does."
    fi
    echo "$count"
}

# The build script's own answer to "what should the stamp be", asked rather
# than recomputed: a second copy of that digest here would be a second thing
# to drift.
compare_stamp() {
    local maker="$1" out="$2"
    [ -x "$maker" ] || fail "missing $maker"
    local want have
    want="$("$maker" --print-stamp)" || fail "$(basename "$maker") --print-stamp failed"
    [ -n "$want" ] || fail "$(basename "$maker") --print-stamp printed nothing"
    have="$(cat "$out/.slopos-stamp" 2>/dev/null || true)"
    [ "$want" = "$have" ] || fail "third_party/slopos-cxx was built from other inputs
       expected: $want
       actual:   ${have:-no stamp at all}
       The pin, the build line or slibc's C headers moved since it was built.
       Rebuild it: scripts/make_slopos_cxx.sh <dir holding libc.so>"
}

read_pin() {
    sed -n "s/^$2=\\(.*\\)$/\\1/p" "$1" | head -n 1
}

check_tree() {
    local root="$1"
    local libc_override="${2:-}"
    local pin="$root/toolchain/cxx/PIN"
    [ -f "$pin" ] || fail "missing toolchain/cxx/PIN"

    local version url sha min tested major
    version="$(read_pin "$pin" llvm_version)"
    url="$(read_pin "$pin" llvm_url)"
    sha="$(read_pin "$pin" llvm_sha256)"
    min="$(read_pin "$pin" clang_major_min)"
    tested="$(read_pin "$pin" clang_major_tested)"
    [ -n "$version" ] || fail "toolchain/cxx/PIN has no llvm_version"
    [ -n "$url" ] || fail "toolchain/cxx/PIN has no llvm_url"
    case "$min" in
        '' | *[!0-9]*) fail "toolchain/cxx/PIN has no numeric clang_major_min" ;;
    esac
    case "$sha" in
        [0-9a-f]*) [ "${#sha}" -eq 64 ] || fail "llvm_sha256 is not a sha256" ;;
        *) fail "toolchain/cxx/PIN has no llvm_sha256" ;;
    esac
    case "$url" in
        *"$version"*) ;;
        *) fail "llvm_url does not name the pinned version $version" ;;
    esac
    # The floor is the sources' own major, never a later one: a floor above
    # the sources would make *every* build a skewed one, which is the
    # configuration the tested list exists to keep track of rather than the
    # one to demand.
    [ "$min" = "${version%%.*}" ] ||
        fail "clang_major_min ($min) is not llvm_version's major (${version%%.*})"
    [ -n "$tested" ] || fail "toolchain/cxx/PIN has no clang_major_tested"
    case " $tested " in
        *" $min "*) ;;
        *) fail "clang_major_tested ($tested) does not include the floor $min" ;;
    esac
    for major in $tested; do
        case "$major" in
            '' | *[!0-9]*) fail "clang_major_tested has a non-numeric entry: $major" ;;
        esac
        [ "$major" -ge "$min" ] ||
            fail "clang_major_tested names $major, below the floor $min"
    done

    local tarball="$root/third_party/llvm-project-${version}.src.tar.xz"
    if [ -f "$tarball" ]; then
        local have
        have="$(sha256sum "$tarball" | cut -d' ' -f1)"
        [ "$have" = "$sha" ] || fail "third_party/$(basename "$tarball") is not what toolchain/cxx/PIN names
       expected: $sha
       actual:   $have"
    fi

    local out="$root/third_party/slopos-cxx"
    [ -d "$out" ] || {
        echo "$SELF: OK — pin consistent; third_party/slopos-cxx not built here"
        return 0
    }

    for artifact in lib/libc++.so lib/libc++.a include/c++/v1/exception \
        licenses/libcxx-LICENSE.TXT licenses/libcxxabi-LICENSE.TXT; do
        [ -e "$out/$artifact" ] || fail "third_party/slopos-cxx is missing $artifact
       Rebuild it: scripts/make_slopos_cxx.sh <dir holding libc.so>"
    done

    # The staged artifact, named rather than searched for: `builddir` holds
    # several `libc.so` files under cargo's build-script output directories,
    # and a `find` picks whichever readdir reaches first — which can be a
    # stale one that still defines a symbol the shipped library has lost.
    local libc="${libc_override:-$root/builddir/libc.so}"
    if [ ! -f "$libc" ]; then
        echo "$SELF: OK — pin consistent; no libc.so built to check the runtime against"
        return 0
    fi

    # Checked here rather than beside the artifact list, because a `libc.so`
    # on disk is what says the userland has been built in this tree: a CI job
    # restores `third_party/slopos-cxx` from a cache *before* the build that
    # would refresh it, and failing there is a red run no later step can repair.
    compare_stamp "$root/scripts/make_slopos_cxx.sh" "$out"

    command -v nm >/dev/null 2>&1 || fail "nm is required to check the runtime's undefined symbols"
    local work
    work="$(mktemp -d)"
    symbols undefined "$out/lib/libc++.so" "$work/undefined"
    symbols defined "$libc" "$work/defined"
    local count
    count="$(compare_symbols "$work/undefined" "$work/defined")"
    rm -rf "$work"
    echo "$SELF: OK — llvm-project $version, host LLVM >= $min (tested: $tested); $count undefined symbols, all in $(basename "$libc")"
}

self_test() {
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN

    mkdir -p "$tmp/toolchain/cxx"
    # A pin whose URL names a different release than its version.
    cat >"$tmp/toolchain/cxx/PIN" <<'EOF'
llvm_version=18.1.8
llvm_url=https://example.invalid/llvm-project-17.0.1.src.tar.xz
llvm_sha256=0000000000000000000000000000000000000000000000000000000000000000
clang_major_min=18
clang_major_tested=18 22
EOF
    if (check_tree "$tmp" >/dev/null 2>&1); then
        fail "--self-test: a pin whose URL names another release was accepted"
    fi

    # A tarball that is not what the pin names.
    sed -i 's|17.0.1|18.1.8|' "$tmp/toolchain/cxx/PIN"
    mkdir -p "$tmp/third_party"
    echo "not llvm" >"$tmp/third_party/llvm-project-18.1.8.src.tar.xz"
    if (check_tree "$tmp" >/dev/null 2>&1); then
        fail "--self-test: a tarball with the wrong checksum was accepted"
    fi

    # A consistent pin with nothing built is the case CI has.
    rm "$tmp/third_party/llvm-project-18.1.8.src.tar.xz"
    (check_tree "$tmp" >/dev/null 2>&1) ||
        fail "--self-test: a consistent pin with nothing built was rejected"

    # A floor above the sources' own major: every build would then be a
    # skewed one, which is not a thing this pin may ask for.
    sed -i 's|^clang_major_min=18|clang_major_min=19|' "$tmp/toolchain/cxx/PIN"
    if (check_tree "$tmp" >/dev/null 2>&1); then
        fail "--self-test: a floor above the pinned sources' major was accepted"
    fi
    sed -i 's|^clang_major_min=19|clang_major_min=18|' "$tmp/toolchain/cxx/PIN"

    # A tested list that does not include the major CI builds with.
    sed -i 's|^clang_major_tested=.*|clang_major_tested=22|' "$tmp/toolchain/cxx/PIN"
    if (check_tree "$tmp" >/dev/null 2>&1); then
        fail "--self-test: a tested list missing the floor was accepted"
    fi
    sed -i 's|^clang_major_tested=.*|clang_major_tested=18 17|' "$tmp/toolchain/cxx/PIN"
    if (check_tree "$tmp" >/dev/null 2>&1); then
        fail "--self-test: a tested major below the floor was accepted"
    fi
    sed -i 's|^clang_major_tested=.*|clang_major_tested=18 22|' "$tmp/toolchain/cxx/PIN"

    # The stamp half, driven with a maker that states an answer, because the
    # cases above all return before reaching it.
    mkdir -p "$tmp/scripts" "$tmp/built"
    printf '#!/bin/sh\necho deadbeef\n' >"$tmp/scripts/maker.sh"
    chmod +x "$tmp/scripts/maker.sh"
    echo deadbeef >"$tmp/built/.slopos-stamp"
    (compare_stamp "$tmp/scripts/maker.sh" "$tmp/built" >/dev/null 2>&1) ||
        fail "--self-test: a tree whose stamp matches its inputs was rejected"
    echo stale >"$tmp/built/.slopos-stamp"
    if (compare_stamp "$tmp/scripts/maker.sh" "$tmp/built" >/dev/null 2>&1); then
        fail "--self-test: a tree built from other inputs was accepted"
    fi
    rm "$tmp/built/.slopos-stamp"
    if (compare_stamp "$tmp/scripts/maker.sh" "$tmp/built" >/dev/null 2>&1); then
        fail "--self-test: a tree with no stamp at all was accepted"
    fi

    # The symbol half, driven with crafted lists: it is the half the gate
    # exists for, and the cases above never reach it.
    seq 1 "$MIN_UNDEFINED" | sed 's/^/sym/' | sort >"$tmp/undefined"
    cp "$tmp/undefined" "$tmp/defined"
    (compare_symbols "$tmp/undefined" "$tmp/defined" >/dev/null 2>&1) ||
        fail "--self-test: a runtime whose symbols are all present was rejected"

    echo "symMISSING" >>"$tmp/undefined"
    sort -o "$tmp/undefined" "$tmp/undefined"
    if (compare_symbols "$tmp/undefined" "$tmp/defined" >/dev/null 2>&1); then
        fail "--self-test: a symbol absent from libc.so was accepted"
    fi

    : >"$tmp/undefined"
    if (compare_symbols "$tmp/undefined" "$tmp/defined" >/dev/null 2>&1); then
        fail "--self-test: an empty symbol list was accepted"
    fi

    echo "$SELF: --self-test OK"
}

case "${1:-}" in
    --self-test) self_test ;;
    --libc) [ $# -eq 2 ] || { echo "usage: $SELF.sh --libc <path>" >&2; exit 2; }
            check_tree "$REPO_ROOT" "$2" ;;
    "") check_tree "$REPO_ROOT" ;;
    *) echo "usage: $SELF.sh [--libc <path>] | --self-test" >&2; exit 2 ;;
esac
