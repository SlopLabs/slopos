#!/usr/bin/env bash
set -euo pipefail

# The C and C++ cross compiler for `x86_64-unknown-slopos`: a target sysroot
# and the `x86_64-unknown-slopos-clang{,++}` wrappers over the host clang.
#
# Usage: make_slopos_cross.sh <sysroot> <bin dir>
#        make_slopos_cross.sh --print-stamp
#
# `scripts/bootstrap_slopos_toolchain.sh` hands the wrappers to bootstrap and
# `scripts/build_recipes.sh` builds the recipes with them. Each passes its own
# two directories, because assembling a sysroot starts by deleting it and a
# recipe build must not do that under a running `x.py`.
#
# `--print-stamp` digests what an object built with this compiler depends on:
# this file, the host tools, slibc's headers, the C++ runtime's stamp and ABI
# flags, the start file and builtins archive linked into every executable, and
# the *names* `libc.so` defines. Names and not bytes: a shared library built
# here links `libc.so` rather than copying it, so only a new or vanished
# function can change what a configure probe finds. `libc.a` is left out for
# the same reason: nothing built with this compiler links it.
#
# Needs a tests userland build in the build directory (`BUILD_DIR`, default
# builddir) and the C++ runtime (`scripts/make_slopos_cxx.sh`).

SELF="make_slopos_cross"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

TARGET="x86_64-unknown-slopos"
HOST_TRIPLE="x86_64-unknown-linux-gnu"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
RELEASE_DIR="$BUILD_DIR/target/$TARGET/release"
CXX_DIR="$REPO_ROOT/third_party/slopos-cxx"

CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"

for library in libc.so crt0.o libbuiltins.a; do
    [ -f "$BUILD_DIR/$library" ] ||
        die "no $library in $BUILD_DIR — run a tests userland build first"
done
[ -f "$CXX_DIR/lib/libc++.so" ] || die "no C++ runtime — run scripts/make_slopos_cxx.sh"

# `--print-abi-flags` and not a literal: the rune-table flag decides the width
# and bits of `ctype_base::mask`, a type passed by value, so a consumer that
# omits it disagrees with the runtime about it — and libc++'s own `__locale`
# then names glibc's `_ISalpha` and does not compile at all.
CXX_ABI_FLAGS="$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"

if [ "${1:-}" = "--print-stamp" ]; then
    {
        sha256sum "$SCRIPT_DIR/$SELF.sh" | cut -d' ' -f1
        printf '%s\n' "$CXX_TOOLS" "$CXX_ABI_FLAGS"
        "$CLANG" --version | head -n 1
        (cd "$REPO_ROOT/slibc/include" && find . -type f -print | LC_ALL=C sort | xargs sha256sum)
        cat "$CXX_DIR/.slopos-stamp" 2>/dev/null || echo "no C++ runtime stamp"
        sha256sum "$BUILD_DIR/crt0.o" "$BUILD_DIR/libbuiltins.a" | cut -d' ' -f1
        readelf --dyn-syms -W "$BUILD_DIR/libc.so" |
            awk 'NF >= 8 && $1 ~ /:$/ && $7 != "UND" { print $8 }' | LC_ALL=C sort -u
    } | sha256sum | cut -d' ' -f1
    exit 0
fi

[ $# -eq 2 ] || die "usage: $SELF.sh <sysroot> <bin dir> | --print-stamp"
[ -f "$RELEASE_DIR/libc.a" ] || die "no libc.a in $RELEASE_DIR — run a tests userland build first"
SYSROOT="$1"
WRAPPER_DIR="$2"
case "$SYSROOT$WRAPPER_DIR" in
    *[[:space:]]*) die "the wrapper cannot take a path containing whitespace: $SYSROOT $WRAPPER_DIR" ;;
esac

# ---------------------------------------------------------------------------
# The target sysroot: what a cross compiler for this triple needs to find.
# Assembled rather than pointed at, because the pieces live in three places —
# the userland build's output, slibc's generated headers, and the cross-built
# C++ runtime.
#
# Copied with their times: every object of the cross LLVM depends on these
# headers, so a copy dated now would make ninja recompile all of it on every
# run whose LLVM step is due.
# ---------------------------------------------------------------------------
rm -rf "$SYSROOT"
mkdir -p "$SYSROOT/lib" "$SYSROOT/include" "$WRAPPER_DIR"
cp -p "$BUILD_DIR/libc.so" "$BUILD_DIR/crt0.o" "$BUILD_DIR/libbuiltins.a" "$SYSROOT/lib/"
cp -p "$RELEASE_DIR/libc.a" "$SYSROOT/lib/"
cp -p "$CXX_DIR/lib/libc++.so" "$CXX_DIR/lib/libc++.a" "$SYSROOT/lib/"
cp -rp "$REPO_ROOT/slibc/include/." "$SYSROOT/include/"
cp -rp "$CXX_DIR/include/c++" "$SYSROOT/include/c++"
SYSROOT="$(cd "$SYSROOT" && pwd)"

# slibc is one library: there is no separate libm, libdl, libpthread or
# librt, and a build system that probes for them finds the host's unless
# something answers. Empty archives are what musl-derived sysroots answer
# with, and they turn a probe that would link against glibc into one that
# links against nothing.
for stub in m dl pthread rt util; do
    "$LLVM_AR" crs "$SYSROOT/lib/lib$stub.a"
done

# ---------------------------------------------------------------------------
# The compiler wrapper. It is `toolchains::SlopOS` written in shell, and it
# goes away the day a clang built from `toolchain/llvm/` is the one running
# the build.
#
# It compiles and links in two invocations, with two different triples, and
# that is the whole reason it exists. Compilation must name SlopOS, or the
# preprocessor defines `__linux__` and LLVM takes `/proc/self/exe` and
# `sched_getaffinity` paths this system has not got. Linking must not: the
# host clang has no SlopOS toolchain, so for that triple it hands the link to
# `gcc`, which would make a host GCC a build requirement
# `scripts/cxx_host_tools.sh` deliberately does not have and would put the
# host's library directories on the line. Naming a triple clang does have a
# toolchain for keeps `ld.lld` the linker, and `--sysroot` confines the search
# to this sysroot — `-lm` against the host's libm is then a link error rather
# than a binary that dies on SlopOS.
# ---------------------------------------------------------------------------
write_wrapper() {
    local path="$1" compiler="$2" stdlib="$3" stdlib_libs="${4:-}"
    cat >"$path" <<WRAPPER
#!/bin/sh
set -e
# Generated by scripts/$SELF.sh — do not edit.
sysroot="$SYSROOT"
cflags="--target=$TARGET -D__slopos__ -nostdlibinc $stdlib -isystem \$sysroot/include"
cflags="\$cflags -Wno-unused-command-line-argument"

# Response files are expanded first: CMake writes one when a link line grows
# past the argument limit, and a scan that sees only \`@file\` classifies a
# link as a compile and hands the whole thing to the host driver.
expanded=""
for arg in "\$@"; do
    case "\$arg" in
        @*)
            file="\${arg#@}"
            [ -f "\$file" ] || { echo "\$0: no response file \$file" >&2; exit 1; }
            expanded="\$expanded \$(tr '\n' ' ' <"\$file")"
            ;;
        *) expanded="\$expanded \$arg" ;;
    esac
done
# shellcheck disable=SC2086
set -- \$expanded

# A caller's own \`--target\` is dropped rather than overridden. The \`cc\`
# crate appends one built from Cargo's \`CARGO_CFG_TARGET_*\`, which spells
# the environment as a fourth component — \`x86_64-unknown-slopos-slibc\` —
# and clang reads that as a version field and refuses the triple. Measured:
# it is what stopped \`compiler_builtins\`' build script.
#
# \`-Xlinker\`, \`-Xassembler\` and \`-Xpreprocessor\` take an operand that is
# a flag for *that* tool, so it is carried through without being read as one
# of ours: \`-Xlinker -E\` is \`--export-dynamic\`, not \`clang -E\`.
linking=1
shared=0
static=0
static_cxx=0
carry=0
drop=0
remaining=\$#
while [ "\$remaining" -gt 0 ]; do
    arg="\$1"
    shift
    remaining=\$((remaining - 1))
    if [ "\$drop" -eq 1 ]; then
        drop=0
        continue
    fi
    if [ "\$carry" -eq 1 ]; then
        carry=0
        set -- "\$@" "\$arg"
        continue
    fi
    case "\$arg" in
        --target=*) continue ;;
        -target)
            drop=1
            continue
            ;;
        -Xlinker | -Xassembler | -Xpreprocessor) carry=1 ;;
        -c | -S | -E | -M | -MM | -fsyntax-only | -### | --version | --help | -dumpmachine | -dumpversion | -print-* | --print-*)
            linking=0
            ;;
        -shared) shared=1 ;;
        -static) static=1 ;;
        -static-libstdc++) static_cxx=1 ;;
    esac
    set -- "\$@" "\$arg"
done

if [ "\$linking" -eq 0 ]; then
    # \`toolchains::SlopOS\` looks \`-print-file-name\` up in the sysroot's
    # \`lib/\`; the host driver has no library path for this triple and answers
    # with the bare name. It is how bootstrap finds \`libc++.a\` for a
    # \`rustc_llvm\` that links the C++ runtime statically.
    for arg in "\$@"; do
        case "\$arg" in
            -print-file-name=*)
                if [ -f "\$sysroot/lib/\${arg#-print-file-name=}" ]; then
                    printf '%s\n' "\$sysroot/lib/\${arg#-print-file-name=}"
                    exit 0
                fi
                ;;
        esac
    done
    exec $compiler \$cflags "\$@"
fi

# A link invocation may still carry sources — CMake's \`try_compile\` is
# exactly that shape. Each is compiled for SlopOS first; what reaches the
# link is objects only. Anything that is neither a source, an input file nor
# a linker argument reaches both phases, because \`-O2\`, \`-g\` and \`-flto\`
# all mean something to each.
objects=""
link_args=""
compile_args=""
output=""
sources=""
skip=0
carry=0
for arg in "\$@"; do
    if [ "\$skip" -eq 1 ]; then
        output="\$arg"
        skip=0
        continue
    fi
    if [ "\$carry" -eq 1 ]; then
        carry=0
        link_args="\$link_args \$arg"
        continue
    fi
    case "\$arg" in
        -o) skip=1 ;;
        -o*) output="\${arg#-o}" ;;
        -Xlinker)
            carry=1
            link_args="\$link_args \$arg"
            ;;
        -l* | -L* | -Wl,* | -shared | -static | -rdynamic | -pie | -no-pie)
            link_args="\$link_args \$arg"
            ;;
        -*)
            compile_args="\$compile_args \$arg"
            link_args="\$link_args \$arg"
            ;;
        *.c | *.cc | *.cpp | *.cxx | *.C | *.s | *.S) sources="\$sources \$arg" ;;
        *.o | *.a | *.so | *.so.*) link_args="\$link_args \$arg" ;;
        *)
            compile_args="\$compile_args \$arg"
            link_args="\$link_args \$arg"
            ;;
    esac
done
[ "\$skip" -eq 0 ] || { echo "\$0: -o with no operand" >&2; exit 1; }

tmp="\$(mktemp -d)"
trap 'rm -rf "\$tmp"' EXIT INT TERM
for source in \$sources; do
    obj="\$tmp/\$(printf '%s' "\$source" | tr '/.' '__').o"
    $compiler \$cflags \$compile_args -c "\$source" -o "\$obj"
    objects="\$objects \$obj"
done
[ -n "\$output" ] || output=a.out

# Objects ahead of \`-l\` and \`.a\`: archive resolution is order-sensitive, and
# a \`try_compile\` with \`CMAKE_REQUIRED_LIBRARIES\` is one source plus one
# \`-l\`.
set -- --target=$HOST_TRIPLE --sysroot="\$sysroot" --gcc-toolchain="\$sysroot" -fuse-ld=lld -nostdlib \\
    -Wno-unused-command-line-argument -L"\$sysroot/lib" \\
    \$objects \$link_args -o "\$output" -Wl,--eh-frame-hdr
if [ "\$shared" -eq 0 ]; then
    set -- -no-pie "\$@" -Wl,--image-base=0x400000 "\$sysroot/lib/crt0.o"
fi
if [ "\$static" -eq 0 ]; then
    set -- "\$@" -Wl,-z,now
    [ "\$shared" -eq 1 ] || set -- "\$@" -Wl,--dynamic-linker=/lib/ld-slopos.so.1
fi
# \`-static-libstdc++\` is what clang's GNU toolchains make of it: the C++
# runtime alone out of its archive, everything else still shared.
cxx_libs="$stdlib_libs"
if [ "\$static_cxx" -eq 1 ] && [ "\$static" -eq 0 ] && [ -n "\$cxx_libs" ]; then
    cxx_libs="-Wl,-Bstatic \$cxx_libs -Wl,-Bdynamic"
fi
set -- "\$@" \$cxx_libs -lc "\$sysroot/lib/libbuiltins.a"
status=0
$compiler "\$@" || status=\$?
rm -rf "\$tmp"
trap - EXIT INT TERM
exit "\$status"
WRAPPER
    chmod +x "$path"
}

write_wrapper "$WRAPPER_DIR/$TARGET-clang" "$CLANG" ""
write_wrapper "$WRAPPER_DIR/$TARGET-clang++" "$CLANGXX" \
    "-nostdinc++ -isystem $SYSROOT/include/c++/v1 $CXX_ABI_FLAGS" "-lc++"
