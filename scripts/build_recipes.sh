#!/usr/bin/env bash
set -euo pipefail

# Build the recipes under toolchain/recipes/ for x86_64-unknown-slopos.
#
# Usage: build_recipes.sh [<name>...]
#        build_recipes.sh --print-stamp [<name>...]
#
# A recipe is a pinned upstream tarball, its checksum, the recipes it needs,
# and one of the build templates below — and nothing else: no patch, no edit
# to the unpacked source. A build that would need one is a finding against
# slibc or the kernel, fixed there. `scripts/check_recipes.sh` holds every
# recipe to that shape, and this driver proves it of every build: both
# templates build out of the unpacked tree, and a recipe whose configure,
# build or install leaves that tree other than the tarball unpacked it — a
# byte, a mode, a file, or a change time — fails.
#
# `toolchain/recipes/<name>/recipe` is `key=value`, one per line, `#`
# comments ignored:
#
#   version, url, sha256, license   the tarball and what it is
#   template                        `cmake` or `openssl`
#   depends                         recipes built first, space-separated
#   soname                          each shared library it must install
#   arg                             one argument to the template's configure
#   config, target                  `openssl`: the out-of-tree target
#                                   definition beside the recipe, and the
#                                   target it defines
#
# Templates:
#
#   cmake     configure with a toolchain file naming the SlopOS compiler
#             wrapper and confining every search to the target sysroot and
#             the recipe prefix, build with Ninja, install.
#   openssl   `Configure --config=<file> <target>`, `make build_sw`,
#             `make install_sw`.
#
# Everything builds shared, into one prefix (`<recipes dir>/prefix`) with a
# `$ORIGIN` run path, and every shared object is linked `-z defs`: a symbol
# no library on its line defines — a libc function slibc lacks — fails the
# build here rather than a `dlopen` on SlopOS. `bootstrap_slopos_toolchain.sh`
# copies the prefix's runtime libraries and headers into the target sysroot,
# and so into the toolchain install and the dev disk.
#
# Idempotent: a stamp per recipe over its directory, this file, the cross
# compiler's inputs (`make_slopos_cross.sh --print-stamp`), the host build
# tools, the prefix and the stamps of the recipes it depends on. A rebuilt
# recipe removes the files it installed last time before installing again.
# `--print-stamp` prints `<name> <stamp>` for each recipe instead of
# building.
#
# Tarballs are cached in third_party/recipes/ as `<name>-<version>.<ext>`.
# An offline checkout pre-populates that directory or points `<NAME>_URL`
# (`ZLIB_URL`, `OPENSSL_URL`, ...) at a local copy.
#
# Environment:
#   BUILD_DIR            where the userland build staged libc (default: builddir)
#   SLOPOS_RECIPES_DIR   the recipes' build tree (default: <build dir>/slopos-recipes)
#   RECIPE_JOBS          parallel jobs (default: nproc)

SELF="build_recipes"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

die() {
    echo "$SELF: $1" >&2
    exit 1
}

PRINT_STAMP=0
if [ "${1:-}" = "--print-stamp" ]; then
    PRINT_STAMP=1
    shift
fi

RECIPES="$REPO_ROOT/toolchain/recipes"
CACHE="$REPO_ROOT/third_party/recipes"
BUILD_DIR="${BUILD_DIR:-$REPO_ROOT/builddir}"
BUILD_DIR="$(mkdir -p "$BUILD_DIR" && cd "$BUILD_DIR" && pwd)"
OUT="${SLOPOS_RECIPES_DIR:-$BUILD_DIR/slopos-recipes}"
mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
PREFIX="$OUT/prefix"
CROSS="$OUT/cross"
JOBS="${RECIPE_JOBS:-$(nproc)}"
TARGET="x86_64-unknown-slopos"

case "$OUT" in
    *[[:space:]]*) die "the recipe tree cannot live at a path containing whitespace: $OUT" ;;
esac

# The first value of `key`, and every value of it.
recipe_value() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$RECIPES/$1/recipe" | head -n 1
}
recipe_values() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$RECIPES/$1/recipe"
}

all_recipes() {
    (cd "$RECIPES" && for dir in */; do printf '%s\n' "${dir%/}"; done) | LC_ALL=C sort
}

# Depth-first, dependencies first; a cycle is an error rather than a hang.
ORDER=()
declare -A VISIT=()
visit() {
    local name="$1" dep
    [ -f "$RECIPES/$name/recipe" ] || die "no recipe named $name"
    case "${VISIT[$name]:-}" in
        done) return 0 ;;
        active) die "the recipes depend on each other in a cycle through $name" ;;
    esac
    VISIT[$name]=active
    for dep in $(recipe_value "$name" depends); do
        visit "$dep"
    done
    VISIT[$name]=done
    ORDER+=("$name")
}

if [ $# -eq 0 ]; then
    set -- $(all_recipes)
fi
for name in "$@"; do
    visit "$name"
done

# ---------------------------------------------------------------------------
# Host tools. `cxx_host_tools.sh` answers for the compiler the wrapper runs;
# the build systems are named here and go into the stamp with their versions.
# ---------------------------------------------------------------------------
CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"
. "$SCRIPT_DIR/lib/rustc_build_settings.sh"
eval "$(rbs_llvm_archivers "$LLVM_AR")" || die "no llvm-ranlib beside $LLVM_AR"
LLVM_AR="$RBS_AR"
LLVM_RANLIB="$RBS_RANLIB"
for tool in cmake ninja make perl; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required to build the recipes"
done
HOST_TOOLS="$(cmake --version | head -n 1; ninja --version; make --version | head -n 1; perl -e 'print "perl $]\n"')"

CROSS_STAMP="$(BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_slopos_cross.sh" --print-stamp)" ||
    die "the cross compiler's inputs are not there — see above"
DRIVER_STAMP="$(sha256sum "$SCRIPT_DIR/$SELF.sh" | cut -d' ' -f1)"

declare -A WANT=()
stamp_want() {
    local name="$1" dep
    if [ -z "${WANT[$name]:-}" ]; then
        WANT[$name]="$(
            printf '%s\n' "$DRIVER_STAMP" "$CROSS_STAMP" "$HOST_TOOLS" "$PREFIX"
            (cd "$RECIPES/$name" && find . -type f -print | LC_ALL=C sort | xargs sha256sum)
            for dep in $(recipe_value "$name" depends); do
                printf 'depends %s %s\n' "$dep" "$(stamp_want "$dep")"
            done
        )"
        WANT[$name]="$(printf '%s\n' "${WANT[$name]}" | sha256sum | cut -d' ' -f1)"
    fi
    printf '%s\n' "${WANT[$name]}"
}

if [ "$PRINT_STAMP" -eq 1 ]; then
    for name in "${ORDER[@]}"; do
        printf '%s %s\n' "$name" "$(stamp_want "$name")"
    done
    exit 0
fi

# ---------------------------------------------------------------------------
# The cross compiler, in a sysroot of the recipes' own: assembling one starts
# by deleting it, and bootstrap's may be under a running x.py.
# ---------------------------------------------------------------------------
BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_slopos_cross.sh" "$CROSS/sysroot" "$CROSS/bin" ||
    die "could not assemble the cross compiler"
SYSROOT="$CROSS/sysroot"
CC_WRAPPER="$CROSS/bin/$TARGET-clang"
CXX_WRAPPER="$CROSS/bin/$TARGET-clang++"

# `Linux` because CMake's `UNIX` and its ELF/GNU-ld platform rules come with
# it, as for LLVM (`toolchain/compiler/0003`) and the C++ runtime; the
# compiler still defines `__slopos__` and not `__linux__`. The find roots keep
# every probe inside the sysroot and the prefix, so a library the host has
# and SlopOS lacks is a probe that fails rather than a build that links it.
mkdir -p "$PREFIX"
cat >"$CROSS/toolchain.cmake" <<CMAKE
# Generated by scripts/$SELF.sh — do not edit.
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER "$CC_WRAPPER")
set(CMAKE_CXX_COMPILER "$CXX_WRAPPER")
set(CMAKE_AR "$LLVM_AR")
set(CMAKE_RANLIB "$LLVM_RANLIB")
set(CMAKE_FIND_ROOT_PATH "$SYSROOT" "$PREFIX")
set(CMAKE_PREFIX_PATH "$PREFIX")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(CMAKE_SHARED_LINKER_FLAGS_INIT "-Wl,-z,defs")
set(CMAKE_MODULE_LINKER_FLAGS_INIT "-Wl,-z,defs")
set(CMAKE_INSTALL_RPATH "\$ORIGIN")
set(CMAKE_BUILD_WITH_INSTALL_RPATH ON)
CMAKE

# pkg-config sees the prefix and nothing else.
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
unset PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR

# The stamp records no environment, so the build reads none: CMake seeds
# its compilers, flags and search paths from these, OpenSSL's Configure its
# tools and flags, clang its include and library paths, and `find_package`
# a `<Package>_ROOT`.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS LDLIBS ASFLAGS ARFLAGS RCFLAGS \
    CC CXX CPP AS AR RANLIB LD NM OBJCOPY OBJDUMP STRIP RC MT CROSS_COMPILE PERL HASHBANGPERL \
    CPATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH OBJC_INCLUDE_PATH LIBRARY_PATH COMPILER_PATH \
    GCC_EXEC_PREFIX CCC_OVERRIDE_OPTIONS SDKROOT MAKEFLAGS MFLAGS MAKEFILES GNUMAKEFLAGS \
    NINJA_STATUS DESTDIR
for var in $(compgen -e); do
    case "$var" in
        CMAKE_* | *_ROOT) unset "$var" ;;
    esac
done

# ---------------------------------------------------------------------------
# One recipe.
# ---------------------------------------------------------------------------
fetch() {
    local name="$1" version url sha ext file var got
    version="$(recipe_value "$name" version)"
    url="$(recipe_value "$name" url)"
    sha="$(recipe_value "$name" sha256)"
    case "$url" in
        *.tar.gz) ext=tar.gz ;;
        *.tar.xz) ext=tar.xz ;;
        *.tar.bz2) ext=tar.bz2 ;;
        *) die "$name: $url is not a .tar.gz, .tar.xz or .tar.bz2" ;;
    esac
    file="$CACHE/$name-$version.$ext"
    var="$(printf '%s' "$name" | tr '[:lower:]-' '[:upper:]_')_URL"
    protocols=(--proto =https --proto-redir =https)
    if [ -n "${!var:-}" ]; then
        url="${!var}"
        protocols=()
    fi
    if [ ! -f "$file" ]; then
        mkdir -p "$CACHE"
        echo "$SELF: fetching $url" >&2
        curl -L "${protocols[@]}" --fail --show-error "$url" -o "$file.part" || die "could not fetch $url
       An offline checkout pre-populates third_party/recipes/$(basename "$file"),
       or points $var at a local copy."
        mv "$file.part" "$file"
    fi
    got="$(sha256sum "$file" | cut -d' ' -f1)"
    [ "$got" = "$sha" ] || die "$(basename "$file") is $got, not the pinned $sha — delete it to refetch"
    printf '%s\n' "$file"
}

template_cmake() {
    local name="$1" work="$2" args=()
    mapfile -t args < <(recipe_values "$name" arg)
    cmake -G Ninja -S "$work/src" -B "$work/build" -Wno-dev \
        -DCMAKE_TOOLCHAIN_FILE="$CROSS/toolchain.cmake" \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DCMAKE_INSTALL_LIBDIR=lib \
        -DBUILD_SHARED_LIBS=ON \
        "${args[@]}" >"$work/configure.log" 2>&1 ||
        { tail -n 40 "$work/configure.log" >&2; die "$name: configure failed; see $work/configure.log"; }
    ninja -C "$work/build" -j "$JOBS" >"$work/build.log" 2>&1 ||
        { tail -n 40 "$work/build.log" >&2; die "$name: build failed; see $work/build.log"; }
    DESTDIR="$work/dest" cmake --install "$work/build" >"$work/install.log" 2>&1 ||
        { tail -n 20 "$work/install.log" >&2; die "$name: install failed; see $work/install.log"; }
}

template_openssl() {
    local name="$1" work="$2" config target args=()
    config="$(recipe_value "$name" config)"
    target="$(recipe_value "$name" target)"
    [ -f "$RECIPES/$name/$config" ] || die "$name: no target definition $config beside the recipe"
    mapfile -t args < <(recipe_values "$name" arg)
    mkdir -p "$work/build"
    (cd "$work/build" &&
        CC="$CC_WRAPPER" CXX="$CXX_WRAPPER" AR="$LLVM_AR" RANLIB="$LLVM_RANLIB" \
            perl "$work/src/Configure" --config="$RECIPES/$name/$config" "$target" \
            --prefix="$PREFIX" --libdir=lib shared "${args[@]}") >"$work/configure.log" 2>&1 ||
        { tail -n 40 "$work/configure.log" >&2; die "$name: Configure failed; see $work/configure.log"; }
    make -C "$work/build" -j "$JOBS" build_sw >"$work/build.log" 2>&1 ||
        { tail -n 40 "$work/build.log" >&2; die "$name: build failed; see $work/build.log"; }
    make -C "$work/build" DESTDIR="$work/dest" install_sw >"$work/install.log" 2>&1 ||
        { tail -n 20 "$work/install.log" >&2; die "$name: install failed; see $work/install.log"; }
}

# The unpacked tree as the tarball made it: every entry's path, type, mode,
# change time and link target, then every file's checksum. No write can set
# a change time back, so an edit a build makes and then undoes is caught as
# surely as one it leaves.
source_manifest() {
    (cd "$1" && find . -printf '%p %y %m %C@ %l\n' | LC_ALL=C sort &&
        find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum)
}

build_recipe() {
    local name="$1" want="$2" work="$OUT/$name" tarball template
    tarball="$(fetch "$name")"
    template="$(recipe_value "$name" template)"
    declare -F "template_$template" >/dev/null || die "$name: no template named '$template'"

    echo "$SELF: building $name $(recipe_value "$name" version)" >&2
    if [ -f "$work/manifest" ]; then
        (cd "$PREFIX" && xargs -r -d '\n' rm -f <"$work/manifest")
    fi
    rm -rf "$work"
    mkdir -p "$work/src"
    tar -xf "$tarball" -C "$work/src" --strip-components=1 || die "$name: could not unpack $tarball"
    source_manifest "$work/src" >"$work/source.manifest"
    "template_$template" "$name" "$work"
    source_manifest "$work/src" | diff "$work/source.manifest" - >"$work/source.diff" || {
        head -n 20 "$work/source.diff" >&2
        die "$name: the build changed the upstream source tree; recipes build it unmodified — see $work/source.diff"
    }

    [ -d "$work/dest$PREFIX" ] || die "$name: the install put nothing under $PREFIX"
    (cd "$work/dest$PREFIX" && find . ! -type d -print | sed 's|^\./||' | LC_ALL=C sort) >"$work/manifest"
    cp -a "$work/dest$PREFIX/." "$PREFIX/"
    installed "$name" || die "$name: installed no lib/$(recipe_values "$name" soname | tr '\n' ' ')"
    rm -rf "$work/src" "$work/build" "$work/dest" "$work/source.manifest" "$work/source.diff"
    printf '%s\n' "$want" >"$work/stamp"
}

installed() {
    local name="$1" soname
    for soname in $(recipe_values "$name" soname); do
        [ -e "$PREFIX/lib/$soname" ] || return 1
    done
}

for name in "${ORDER[@]}"; do
    want="$(stamp_want "$name")"
    if [ "$(cat "$OUT/$name/stamp" 2>/dev/null)" = "$want" ] && installed "$name"; then
        echo "$SELF: $name is up to date" >&2
        continue
    fi
    build_recipe "$name" "$want"
done
echo "$SELF: built ${ORDER[*]} into $PREFIX"
