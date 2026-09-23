#!/usr/bin/env bash
set -euo pipefail

# Hold the cross-build configuration to the toolchain it claims to produce.
#
# Usage: check_bootstrap_config.sh [--require] [--self-test]
#
# `scripts/bootstrap_slopos_toolchain.sh` is hours of CPU, so what is graded
# here is everything up to the first object: bootstrap's own dry run, which
# validates the config, resolves `--host=x86_64-unknown-slopos` through the
# compiler's built-in target list and walks the whole step graph; and the
# compiler wrapper, by compiling and linking with it.
#
# Three things break this and none of them fails loudly:
#
#   * The step graph quietly loses an artifact. `cargo` is an *extended* tool
#     — `build.extended` and `build.tools` decide whether it is built at all,
#     and a stage2 rustc does not depend on it — so a config that stops
#     naming it still builds a compiler, and the dev disk arrives with no
#     cargo on it hours later.
#   * `toolchain/compiler/0003-bootstrap-cmake-system-name.patch` goes away.
#     bootstrap maps a cross target's triple to a `CMAKE_SYSTEM_NAME` by
#     hand; an unrecognised one prints a note, sets `Generic`, and exits 0 —
#     and `Generic` is the value that loses `LLVM_ON_UNIX` and with it every
#     `Unix/*.inc` file the LLVM port patches.
#   * The wrapper stops producing SlopOS binaries. It names two triples on
#     purpose — SlopOS to compile, Linux to link — and either half silently
#     regressing gives a host-shaped object or a `gcc`-driven link.
#
# `skipped` without a materialised source tree or a staged target sysroot;
# the CI step that has both passes `--require`. A first run downloads
# bootstrap's stage0 compiler (~200 MB).

SELF="check_bootstrap_config"
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

TARGET="x86_64-unknown-slopos"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
SRC="${RUSTC_SRC_DIR:-$REPO_ROOT/$TP_RUSTC_SRC_REL}"
GATE="$BUILD_DIR/gates/bootstrap-config"
WRAPPER="$GATE/bin/$TARGET-clang"
WRAPPER_CXX="$GATE/bin/$TARGET-clang++"
INSTALL="${SLOPOS_TOOLCHAIN_INSTALL:-$BUILD_DIR/slopos-toolchain/install}"
# Every library an installed object may need. Anything else is a host library
# the cross link reached, which the guest does not have.
ALLOWED_NEEDED='^(libc\.so|libc\+\+\.so|libLLVM[-.].*|libclang-cpp\.so.*|librustc_driver-[0-9a-f]+\.so|libstd-[0-9a-f]+\.so)$'

# Each step the cross-build exists to produce, as bootstrap spells it. The
# triple is on the right of the arrow because these are the steps built *for*
# SlopOS; the same words appear for the build triple and must not satisfy it.
# The `Installing` half is graded too, because the real run is `x.py install`
# and a config that builds every artifact and installs none of them is a
# toolchain nobody can put on a disk. `src` is the `rust-src` component
# `-Zbuild-std` needs in the guest, and it is installed for the build triple
# because it is the same sources either way.
WANTED_STEPS="
Building LLVM for $TARGET
Building stage2 cargo (stage1:x86_64-unknown-linux-gnu -> stage2:$TARGET)
Building stage2 compiler artifacts (stage1:x86_64-unknown-linux-gnu -> stage2:$TARGET)
Building stage1 library artifacts (stage1:x86_64-unknown-linux-gnu -> stage1:$TARGET)
Installing stage2 rustc (stage1:x86_64-unknown-linux-gnu -> stage2:$TARGET)
Installing stage2 cargo (stage1:x86_64-unknown-linux-gnu -> stage2:$TARGET)
Installing stage2 std (stage1:x86_64-unknown-linux-gnu -> stage2:$TARGET)
Installing llvm-tools for $TARGET
Installing src for x86_64-unknown-linux-gnu
"

SKIP_REASON=""

inputs_ready() {
    if [ ! -f "$SRC/x.py" ]; then
        SKIP_REASON="no rustc sources at $SRC — run scripts/make_rustc_src.sh"
        return 1
    fi
    if [ ! -f "$BUILD_DIR/libc.so" ] || [ ! -d "$REPO_ROOT/third_party/slopos-cxx/lib" ]; then
        SKIP_REASON="no staged target sysroot — run a tests userland build"
        return 1
    fi
    command -v python3 >/dev/null 2>&1 || die "python3 is required to run x.py"
    return 0
}

# Reads a file rather than a pipe, so the self-test can hand it a crafted plan.
grade_plan() {
    local plan="$1" bad=0 step
    if grep -q 'could not determine CMAKE_SYSTEM_NAME' "$plan"; then
        echo "  bootstrap does not know what CMAKE_SYSTEM_NAME $TARGET is, and fell back to Generic" >&2
        bad=1
    fi
    while IFS= read -r step; do
        [ -n "$step" ] || continue
        grep -qF "$step" "$plan" || {
            echo "  the plan does not carry: $step" >&2
            bad=1
        }
    done <<EOF
$WANTED_STEPS
EOF
    return "$bad"
}

# Compile and link with the wrapper, and grade what came out. `probe` is the
# whole contract in one program: a SlopOS macro set, a libc call, an
# executable the loader can start, and a shared object that needs the C++
# runtime.
grade_wrapper() {
    local dir="$1" bad=0 macros
    cat >"$dir/probe.c" <<'PROBE'
#include <stdio.h>
#include <string.h>
int main(void) {
    printf("%zu\n", strspn("+-4", "+-0123456789"));
    return 0;
}
PROBE
    cat >"$dir/probe.cpp" <<'PROBE'
#include <stdexcept>
#include <string>
extern "C" int probe(void) {
    try {
        throw std::runtime_error(std::string("x"));
    } catch (const std::exception &) {
        return 7;
    }
}
PROBE

    macros="$("$WRAPPER" -dM -E - </dev/null)"
    printf '%s\n' "$macros" | grep -q '^#define __slopos__' || {
        echo "  the wrapper does not predefine __slopos__" >&2
        bad=1
    }
    if printf '%s\n' "$macros" | grep -q '^#define __linux__'; then
        echo "  the wrapper predefines __linux__, so LLVM takes Linux code paths" >&2
        bad=1
    fi

    "$WRAPPER" -O1 "$dir/probe.c" -o "$dir/probe.elf" >"$dir/cc.log" 2>&1 || {
        tail -n 10 "$dir/cc.log" >&2
        echo "  the wrapper cannot compile and link a C program in one invocation" >&2
        return 1
    }
    # No `-lc++` and no `-L`: a wrapper that cannot supply its own C++
    # standard library is one CMake cannot link a C++ program with, and
    # passing it here would be the gate supplying the thing under test.
    "$WRAPPER_CXX" -O1 -shared -fPIC "$dir/probe.cpp" -o "$dir/probe.so" \
        >"$dir/cxx.log" 2>&1 || {
        tail -n 10 "$dir/cxx.log" >&2
        echo "  the wrapper cannot link a shared C++ object" >&2
        return 1
    }

    local headers
    headers="$(readelf -lhd "$dir/probe.elf")"
    printf '%s\n' "$headers" | grep -q 'Requesting program interpreter: /lib/ld-slopos.so.1' || {
        echo "  the executable names no SlopOS interpreter" >&2
        bad=1
    }
    printf '%s\n' "$headers" | grep -q 'Shared library: \[libc.so\]' || {
        echo "  the executable does not need libc.so" >&2
        bad=1
    }
    printf '%s\n' "$headers" | grep -q 'Type: *EXEC' || {
        echo "  the executable is not a non-PIE EXEC at the address the loader maps" >&2
        bad=1
    }
    readelf -d "$dir/probe.so" | grep -q 'Shared library: \[libc++.so\]' || {
        echo "  the shared object does not need libc++.so" >&2
        bad=1
    }
    return "$bad"
}

needed_of() {
    readelf -d "$1" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p'
}

# `readelf --dyn-syms` columns: Num Value Size Type Bind Vis Ndx Name.
dynsyms() {
    readelf --dyn-syms -W "$1" | awk -v want="$2" '
        NF >= 8 && $1 ~ /:$/ {
            name = $8; sub(/@.*/, "", name)
            if (name == "") next
            if (want == "defined" && $7 != "UND") print name
            if (want == "undefined" && $7 == "UND" && $5 != "WEAK") print name
        }' | sort -u
}

# Where the loader finds `$2` for the object at `$1`: its DT_RUNPATH or
# DT_RPATH with `$ORIGIN` expanded, as the guest has no LD_LIBRARY_PATH and
# no toolchain library in /lib. `libc.so` is the interpreter, already mapped.
resolve_needed() {
    local object="$1" name="$2" origin entry
    if [ "$name" = libc.so ]; then
        printf '%s\n' "$INSTALL_ROOT/lib/libc.so"
        return 0
    fi
    origin="$(dirname "$object")"
    for entry in $(readelf -d "$object" |
        sed -n 's/.*Library r\(un\)\{0,1\}path: \[\(.*\)\]/\2/p' | tr ':' ' '); do
        entry="${entry//\$\{ORIGIN\}/$origin}"
        entry="${entry//\$ORIGIN/$origin}"
        if [ -f "$entry/$name" ]; then
            printf '%s\n' "$entry/$name"
            return 0
        fi
    done
    return 1
}

# The names an object may bind to: its own and those of everything in its
# DT_NEEDED closure. A name the object's search path cannot find is written
# to `$cache/unfound`.
closure_defined() {
    local cache="$1" queue="$2" seen path name found key
    seen=" "
    while :; do
        # shellcheck disable=SC2086
        set -- $queue
        [ $# -gt 0 ] || break
        path="$1"
        shift
        queue="$*"
        case "$seen" in *" $path "*) continue ;; esac
        seen="$seen$path "
        key="$cache/$(printf '%s' "$path" | tr '/' '_').def"
        [ -f "$key" ] || dynsyms "$path" defined >"$key"
        cat "$key"
        for name in $(needed_of "$path"); do
            if found="$(resolve_needed "$path" "$name")"; then
                queue="$queue $found"
            else
                printf '%s needs %s\n' "${path#"$INSTALL_ROOT"/}" "$name" >>"$cache/unfound"
            fi
        done
    done | sort -u
}

# The loader binds `DTPMOD64`/`DTPOFF64` and startup `TPOFF64` and nothing
# else for TLS, so a `TLSDESC` relocation is a program that dies at startup.
# It also binds every symbol before `main` and nothing here links with
# `-z defs`, so a library the search path misses, or a name nothing in the
# closure defines, is a program that does not start.
grade_install() {
    local dir="$1" bad=0 file needed lib cache unbound
    INSTALL_ROOT="$dir"
    cache="$(mktemp -d)"
    while IFS= read -r file; do
        [ "$(od -An -c -N4 "$file" | tr -d ' ')" = '177ELF' ] || continue
        readelf -h "$file" 2>/dev/null | grep -Eq 'Type: +(EXEC|DYN)' || continue
        needed="$(needed_of "$file")"
        for lib in $needed; do
            printf '%s\n' "$lib" | grep -Eq "$ALLOWED_NEEDED" || {
                echo "  ${file#"$dir"/} needs $lib, which no SlopOS toolchain ships" >&2
                bad=1
            }
        done
        if readelf -rW "$file" | grep 'R_X86_64_TLSDESC' >/dev/null; then
            echo "  ${file#"$dir"/} carries R_X86_64_TLSDESC, which the loader does not bind" >&2
            bad=1
        fi
        [ -n "$needed" ] || continue
        rm -f "$cache/unfound"
        unbound="$(dynsyms "$file" undefined |
            comm -23 - <(closure_defined "$cache" "$file") | head -n 5 | tr '\n' ' ')"
        if [ -s "$cache/unfound" ]; then
            sed 's/^/  /; s/$/, which its search path does not find/' "$cache/unfound" | sort -u >&2
            bad=1
        elif [ -n "$unbound" ]; then
            echo "  ${file#"$dir"/} binds names nothing it needs defines: $unbound" >&2
            bad=1
        fi
    done <<EOF
$(find "$dir" -type f)
EOF
    rm -rf "$cache"
    return "$bad"
}

# The dry run writes a sysroot and a wrapper, so it is pointed at the gate's
# own directory: a `just toolchain` in progress is compiling against the
# shared ones and executing the shared wrapper.
dry_run() {
    SLOPOS_TOOLCHAIN_OUT="$GATE" SLOPOS_SYSROOT="$GATE/sysroot" \
        "$SCRIPT_DIR/bootstrap_slopos_toolchain.sh" --dry-run
}

run_gate() {
    inputs_ready || skip "$SKIP_REASON"

    rm -rf "$GATE"
    mkdir -p "$GATE"
    dry_run >"$GATE/plan.log" 2>&1 || {
        tail -n 20 "$GATE/plan.log" >&2
        die "the bootstrap dry run failed; see $GATE/plan.log"
    }
    grade_plan "$GATE/plan.log" || die "the cross-build plan does not produce the toolchain"
    grade_wrapper "$GATE" || die "the compiler wrapper does not produce SlopOS binaries"

    local installed=""
    if [ -d "$INSTALL" ]; then
        grade_install "$INSTALL" || die "the toolchain installed at $INSTALL does not run on SlopOS"
        installed=", and the installed toolchain needs nothing SlopOS lacks"
    fi

    echo "$SELF: bootstrap plans $(grep -c '^Building\|^Creating' "$GATE/plan.log") steps for $TARGET, and the wrapper links for it$installed"
}

self_test() {
    local failed=0 scratch
    scratch="$(mktemp -d)"
    trap "rm -rf '$scratch'" EXIT INT TERM

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

    # The plan grader, against a plan recorded from a real dry run rather
    # than assembled out of `WANTED_STEPS`: a positive control built from the
    # gate's own expectations cannot fail, and so observes nothing. A
    # checkout with no captured plan falls back to the expectations, and says
    # so.
    if [ -s "$GATE/plan.log" ]; then
        cp "$GATE/plan.log" "$scratch/good-plan.log"
    else
        printf '%s' "$WANTED_STEPS" >"$scratch/good-plan.log"
    fi
    if grade_plan "$scratch/good-plan.log" 2>/dev/null; then
        echo "  case complete-plan: accepted a plan naming every artifact"
    else
        echo "$SELF --self-test: a complete plan was rejected" >&2
        failed=1
    fi

    grep -v 'stage2 cargo' "$scratch/good-plan.log" >"$scratch/no-cargo.log"
    if grade_plan "$scratch/no-cargo.log" 2>/dev/null; then
        echo "$SELF --self-test: a plan that builds no cargo was accepted" >&2
        failed=1
    else
        echo "  case cargo-dropped: rejected a plan that stopped building cargo"
    fi

    cp "$scratch/good-plan.log" "$scratch/generic.log"
    echo "could not determine CMAKE_SYSTEM_NAME from the target \`$TARGET\`, build may fail" \
        >>"$scratch/generic.log"
    if grade_plan "$scratch/generic.log" 2>/dev/null; then
        echo "$SELF --self-test: a plan that fell back to CMAKE_SYSTEM_NAME=Generic was accepted" >&2
        failed=1
    else
        echo "  case cmake-system-name-lost: rejected a plan bootstrap could not classify"
    fi

    # The real dry run and the real wrapper, which is the positive control:
    # without it a grader that rejects everything reads as a working check.
    if inputs_ready; then
        rm -rf "$GATE"
        mkdir -p "$GATE"
        if dry_run >"$GATE/plan.log" 2>&1 &&
            grade_plan "$GATE/plan.log" && grade_wrapper "$GATE"; then
            echo "  case real-plan: the tree's own plan and wrapper pass"
        else
            echo "$SELF --self-test: the tree's own plan or wrapper does not pass" >&2
            failed=1
        fi

        # A wrapper that forwards to the host compiler and nothing else: the
        # shape a regression that lost one of the two triples produces. It
        # must be rejected, and for the stated reason — a rejection that
        # happened because the host compiler was missing would read the same.
        eval "$("$SCRIPT_DIR/cxx_host_tools.sh")"
        mkdir -p "$scratch/bin"
        for stub in "$TARGET-clang" "$TARGET-clang++"; do
            printf '#!/bin/sh\nexec %s "$@"\n' "$CLANG" >"$scratch/bin/$stub"
            chmod +x "$scratch/bin/$stub"
        done
        real_wrapper="$WRAPPER"
        real_wrapper_cxx="$WRAPPER_CXX"
        WRAPPER="$scratch/bin/$TARGET-clang"
        WRAPPER_CXX="$scratch/bin/$TARGET-clang++"
        why="$(grade_wrapper "$scratch" 2>&1 >/dev/null || true)"
        WRAPPER="$real_wrapper"
        WRAPPER_CXX="$real_wrapper_cxx"
        if printf '%s\n' "$why" | grep -q 'does not predefine __slopos__'; then
            echo "  case host-wrapper: rejected a wrapper that names neither triple"
        else
            echo "$SELF --self-test: a host-compiler wrapper was not rejected for naming no triple" >&2
            printf '%s\n' "$why" | sed 's/^/      /' >&2
            failed=1
        fi
    else
        echo "  cases real-plan, host-wrapper: skipped — $SKIP_REASON"
    fi

    # The install grader, against shared objects built to each shape: one
    # whose every name binds, one a host C++ library leaked into, one that
    # names a function nothing it needs defines, and one whose TLS the loader
    # cannot bind.
    eval "$("$SCRIPT_DIR/cxx_host_tools.sh")"
    local so="$CLANG --target=x86_64-unknown-linux-gnu -fuse-ld=lld -shared -fPIC -nostdlib"
    local lib="$scratch/install/lib"
    mkdir -p "$scratch/stub" "$lib"
    printf 'int f(void) { return 0; }\n' >"$scratch/f.c"
    printf 'extern int f(void);\nint k(void) { return f(); }\n' >"$scratch/calls-f.c"
    printf 'extern int h(void);\nint k(void) { return h(); }\n' >"$scratch/calls-h.c"
    # Assembly, because clang takes `-mtls-dialect=gnu2` for x86-64 only from 19.
    printf '%s\n' '.globl g' 'g:' 'leaq v@tlsdesc(%rip), %rax' 'call *v@tlscall(%rax)' 'ret' \
        >"$scratch/tls.s"
    if $so -Wl,-soname,libc.so "$scratch/f.c" -o "$lib/libc.so" &&
        $so -Wl,-soname,libstdc++.so.6 "$scratch/f.c" -o "$scratch/stub/libstdc++.so.6" &&
        $so "$scratch/calls-f.c" -L"$lib" -l:libc.so -o "$lib/ok.so" &&
        grade_install "$scratch/install" 2>/dev/null; then
        echo "  case clean-install: accepted an object whose names libc.so defines"
    else
        echo "$SELF --self-test: an object binding only to libc.so was not built or was rejected" >&2
        failed=1
    fi
    install_case() {
        local name="$1" want="$2" object="$3"
        shift 3
        if ! $so "$@" -o "$lib/$object"; then
            echo "$SELF --self-test: case $name: the fixture did not build" >&2
            failed=1
            return
        fi
        why="$(grade_install "$scratch/install" 2>&1 || true)"
        rm "$lib/$object"
        if printf '%s\n' "$why" | grep -q "$want"; then
            echo "  case $name: rejected for '$want'"
        else
            echo "$SELF --self-test: case $name was not rejected for '$want'" >&2
            failed=1
        fi
    }
    install_case host-library 'needs libstdc++.so.6' host.so \
        "$scratch/f.c" -L"$scratch/stub" -l:libstdc++.so.6
    install_case unbound 'nothing it needs defines: h' unbound.so \
        "$scratch/calls-h.c" -L"$lib" -l:libc.so
    install_case tlsdesc 'R_X86_64_TLSDESC' tls.so "$scratch/tls.s"

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
