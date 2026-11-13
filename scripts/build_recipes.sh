#!/usr/bin/env bash
set -euo pipefail

# Build the recipes under toolchain/recipes/ for x86_64-unknown-slopos.
#
# Usage: build_recipes.sh [<name>...]
#        build_recipes.sh --print-stamp [<name>...]
#        build_recipes.sh --install-programs <prefix>
#        build_recipes.sh --fetch-source [<name>...]
#
# A recipe is a pinned upstream tarball, its checksum, its dependencies, a
# build template and at most the patches that teach the project the target: a
# build that needs any other edit is a slibc or kernel finding.
# `scripts/check_recipes.sh` checks that shape; this driver fails any build
# that changes the patched tree (byte, mode, file or change time).
#
# `toolchain/recipes/<name>/recipe` is `key=value`, one per line, `#`
# comments ignored:
#
#   version, url, sha256, license   the tarball and what it is
#   license_file                    each licence text in the tarball, installed
#                                   as share/licenses/<name>/<its path there>
#   template                        `cmake`, `meson`, `openssl`, `autotools`
#                                   or `cargo`
#   depends                         recipes built first, space-separated
#   soname                          each shared library it must install
#   program                         each program it must install, relative
#                                   to the prefix
#   arg                             one argument to the template's configure
#   patch                           a patch beside the recipe, applied to the
#                                   unpacked tree in the order given
#   config, target                  `openssl`: the out-of-tree target
#                                   definition beside the recipe, and the
#                                   target it defines
#   lock                            `cargo`: the lockfile beside the recipe,
#                                   which replaces the crate's own
#
# Templates:
#
#   cmake      configure with a toolchain file naming the SlopOS compiler
#              wrapper and the `SlopOS` system of `toolchain/cmake/Platform/`,
#              and confining every search to the target sysroot and the
#              recipe prefix; build with Ninja, install.
#   meson      configure with a cross file naming the same, no subproject
#              fallback, and `--prefix` the guest path of the toolchain the
#              programs are installed into, since a program finds its own
#              files through its compiled-in prefix; build with Ninja, install.
#   openssl    `Configure --config=<file> <target>`, `make build_sw`,
#              `make install_sw`.
#   autotools  `configure` out of tree for `--host` the target, its canonical
#              name taken as the `ac_cv_host` cache answer because upstream's
#              `config.sub` lists the systems it knows, and `--prefix` the
#              guest path, as for meson; `make`, `make install`.
#   cargo      the crate's `.crate`, built by `cargo +slopos` for the target
#              JSON with std from the owned sysroot's forks and linked by the
#              SlopOS compiler wrapper, `--locked` to the recipe's lock; each
#              `program` is `bin/<one of its binaries>`. A package that lock
#              takes with no source is the libc fork or the
#              `toolchain/crates/` port of that name and version, unpacked
#              and patched beside the build and patched in for crates.io.
#
# Everything builds shared into `<recipes dir>/prefix` with `-z defs`, so a
# libc function slibc lacks fails here rather than at `dlopen` on SlopOS. A
# library's run path is `$ORIGIN`; a program's reaches `lib/` from `bin/` or
# `libexec/<name>/`. A recipe that declares no library installs nothing under
# `include/` or `lib/`. Each recipe's stamp covers its directory, this file,
# `make_slopos_cross.sh --print-stamp`, the host tools its template runs, the
# prefixes and its dependencies' stamps, and a `cargo` one the std stamp and
# the crate ports; `--print-stamp` prints `<name> <stamp>` per recipe.
#
# `--install-programs` copies what each built recipe that declares a program
# installed outside `include/` and `lib/` into another prefix; its libraries
# travel with the target sysroot. `--fetch-source` prints the path of each
# recipe's tarball, fetching and checking it, which is the source that travels
# with what a medium distributes of it.
#
# Tarballs are cached in third_party/recipes/; offline, pre-populate it or set
# `<NAME>_URL` (`ZLIB_URL`, ...) to a local copy. A `cargo` recipe's lock is
# fetched into `$CARGO_HOME` as cargo fetches any; offline, set
# `CARGO_NET_OFFLINE=true` over a `$CARGO_HOME` an online build filled.
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
FETCH_SOURCE=0
INSTALL_PROGRAMS=""
case "${1:-}" in
    --print-stamp)
        PRINT_STAMP=1
        shift
        ;;
    --fetch-source)
        FETCH_SOURCE=1
        shift
        ;;
    --install-programs)
        INSTALL_PROGRAMS="${2:?usage: $SELF.sh --install-programs <prefix>}"
        shift 2
        ;;
esac

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

recipe_value() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$RECIPES/$1/recipe" | head -n 1
}
recipe_values() {
    sed -n "s/^$2=\\(.*\\)\$/\\1/p" "$RECIPES/$1/recipe"
}

installed() {
    local name="$1" soname program text
    for soname in $(recipe_values "$name" soname); do
        [ -e "$PREFIX/lib/$soname" ] || return 1
    done
    for program in $(recipe_values "$name" program); do
        [ -e "$PREFIX/$program" ] || return 1
    done
    for text in $(recipe_values "$name" license_file); do
        [ -e "$PREFIX/share/licenses/$name/$text" ] || return 1
    done
}

all_recipes() {
    (cd "$RECIPES" && for dir in */; do printf '%s\n' "${dir%/}"; done) | LC_ALL=C sort
}

fetch() {
    local name="$1" version url sha ext file var got
    version="$(recipe_value "$name" version)"
    url="$(recipe_value "$name" url)"
    sha="$(recipe_value "$name" sha256)"
    case "$url" in
        *.tar.gz) ext=tar.gz ;;
        *.tar.xz) ext=tar.xz ;;
        *.tar.bz2) ext=tar.bz2 ;;
        *.crate) ext=crate ;;
        *) die "$name: $url is not a .tar.gz, .tar.xz, .tar.bz2 or .crate" ;;
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

if [ "$FETCH_SOURCE" -eq 1 ]; then
    for name in "${ORDER[@]}"; do
        fetch "$name"
    done
    exit 0
fi

if [ -n "$INSTALL_PROGRAMS" ]; then
    for name in "${ORDER[@]}"; do
        [ -n "$(recipe_values "$name" program)" ] || continue
        [ -f "$OUT/$name/manifest" ] && installed "$name" ||
            die "$name is not built into $PREFIX — run build_recipes.sh $name first"
        while IFS= read -r rel; do
            case "$rel" in include/* | lib/*) continue ;; esac
            mkdir -p "$INSTALL_PROGRAMS/$(dirname "$rel")"
            cp -a --remove-destination "$PREFIX/$rel" "$INSTALL_PROGRAMS/$rel"
        done <"$OUT/$name/manifest"
    done
    exit 0
fi

CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
eval "$CXX_TOOLS"
. "$SCRIPT_DIR/lib/rustc_build_settings.sh"
GUEST_PREFIX=/usr/local
BUILD_TRIPLE=x86_64-pc-linux-gnu
eval "$(rbs_llvm_archivers "$LLVM_AR")" || die "no llvm-ranlib beside $LLVM_AR"
LLVM_AR="$RBS_AR"
LLVM_RANLIB="$RBS_RANLIB"
# The host tools each template runs, which join its recipes' stamps.
template_tools() {
    case "$1" in
        cmake) echo cmake ninja pkg-config ;;
        meson) echo meson ninja pkg-config ;;
        openssl) echo perl make ;;
        autotools) echo make pkg-config ;;
        cargo) echo cargo git ;;
    esac
}
tool_version() {
    case "$1" in
        # sed drains the pipe: make writes its version a line at a time, and
        # head's early exit would kill it with SIGPIPE under pipefail.
        cmake | make) "$1" --version | sed -n 1p ;;
        perl) perl -e 'print "perl $]\n"' ;;
        cargo) cargo +slopos --version ;;
        git) git --version ;;
        *) printf '%s %s\n' "$1" "$("$1" --version)" ;;
    esac
}
declare -A TOOL_VERSION=()
for name in "${ORDER[@]}"; do
    tools="$(template_tools "$(recipe_value "$name" template)")"
    [ -z "$(recipe_values "$name" patch)" ] || tools="$tools git"
    for tool in $tools; do
        [ -z "${TOOL_VERSION[$tool]:-}" ] || continue
        command -v "$tool" >/dev/null 2>&1 || die "$tool is required to build $name"
        TOOL_VERSION[$tool]="$(tool_version "$tool")"
    done
done
recipe_tools() {
    local tool
    for tool in $(template_tools "$(recipe_value "$1" template)"); do
        printf '%s\n' "${TOOL_VERSION[$tool]}"
    done
}

CROSS_STAMP="$(BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_slopos_cross.sh" --print-stamp)" ||
    die "the cross compiler's inputs are not there — see above"
DRIVER_STAMP="$(sha256sum "$SCRIPT_DIR/$SELF.sh" | cut -d' ' -f1)"
CMAKE_PLATFORM="$REPO_ROOT/toolchain/cmake"
CMAKE_PLATFORM_STAMP="$(cd "$CMAKE_PLATFORM" && find . -type f -print | LC_ALL=C sort | xargs sha256sum)"
. "$SCRIPT_DIR/lib/toolchain_pin.sh"
# What a `cargo` recipe compiles beside its crate: std and the libc fork, the
# ports its lock may take, and the target it is compiled for.
CARGO_STAMP="$(
    tp_std_stamp "$REPO_ROOT"
    (cd "$REPO_ROOT" && find "$TP_CRATES_OVERLAY_REL" "targets/$TARGET.json" -type f -print |
        tp_hash_lines "$REPO_ROOT")
)"

declare -A WANT=()
stamp_want() {
    local name="$1" dep
    if [ -z "${WANT[$name]:-}" ]; then
        WANT[$name]="$(
            printf '%s\n' "$DRIVER_STAMP" "$CROSS_STAMP" "$PREFIX" "$GUEST_PREFIX"
            recipe_tools "$name"
            (cd "$RECIPES/$name" && find . -type f -print | LC_ALL=C sort | xargs sha256sum)
            [ "$(recipe_value "$name" template)" != cmake ] || printf '%s\n' "$CMAKE_PLATFORM_STAMP"
            [ "$(recipe_value "$name" template)" != cargo ] || printf '%s\n' "$CARGO_STAMP"
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

# A sysroot of the recipes' own: assembling one deletes it, and bootstrap's
# may be in use by a running x.py.
BUILD_DIR="$BUILD_DIR" "$SCRIPT_DIR/make_slopos_cross.sh" "$CROSS/sysroot" "$CROSS/bin" ||
    die "could not assemble the cross compiler"
SYSROOT="$CROSS/sysroot"
CC_WRAPPER="$CROSS/bin/$TARGET-clang"
CXX_WRAPPER="$CROSS/bin/$TARGET-clang++"

# The find roots make a library SlopOS lacks a failed probe rather than a link
# against the host's, and the prefix's programs, which run only on SlopOS, are
# never what a build finds to run. A program's run path reaches `lib/` from
# `bin/`.
mkdir -p "$PREFIX"
cat >"$CROSS/toolchain.cmake" <<CMAKE
# Generated by scripts/$SELF.sh — do not edit.
set(CMAKE_SYSTEM_NAME SlopOS)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
list(APPEND CMAKE_MODULE_PATH "$CMAKE_PLATFORM")
set(CMAKE_C_COMPILER "$CC_WRAPPER")
set(CMAKE_CXX_COMPILER "$CXX_WRAPPER")
set(CMAKE_AR "$LLVM_AR")
set(CMAKE_RANLIB "$LLVM_RANLIB")
set(CMAKE_FIND_ROOT_PATH "$SYSROOT" "$PREFIX")
set(CMAKE_PREFIX_PATH "$PREFIX")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_IGNORE_PATH "$PREFIX/bin" "$PREFIX/sbin" "$PREFIX/libexec")
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)
set(CMAKE_SHARED_LINKER_FLAGS_INIT "-Wl,-z,defs")
set(CMAKE_MODULE_LINKER_FLAGS_INIT "-Wl,-z,defs")
set(CMAKE_INSTALL_RPATH "\$ORIGIN;\$ORIGIN/../lib")
set(CMAKE_BUILD_WITH_INSTALL_RPATH ON)
CMAKE

# meson looks a target program with no `[binaries]` entry up on the host's
# PATH, so a recipe disables what would take the host's Perl or Tcl for the
# guest's.
cat >"$CROSS/meson.ini" <<MESON
# Generated by scripts/$SELF.sh — do not edit.
[binaries]
c = '$CC_WRAPPER'
cpp = '$CXX_WRAPPER'
ar = '$LLVM_AR'
pkg-config = '$(command -v pkg-config)'

[built-in options]
c_link_args = ['-Wl,-z,defs', '-Wl,-rpath,\$ORIGIN/../lib:\$ORIGIN/../../lib']
cpp_link_args = ['-Wl,-z,defs', '-Wl,-rpath,\$ORIGIN/../lib:\$ORIGIN/../../lib']

[host_machine]
system = 'slopos'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
MESON

cat >"$CROSS/autotools-link.rsp" <<'RSP'
-Wl,-z,defs -Wl,-rpath,$ORIGIN/../lib:$ORIGIN/../../lib
RSP

# pkg-config sees the prefix and nothing else.
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
unset PKG_CONFIG_PATH PKG_CONFIG_SYSROOT_DIR

# The stamp records no environment, so the build must read none.
unset CFLAGS CXXFLAGS CPPFLAGS LDFLAGS LDLIBS ASFLAGS ARFLAGS RCFLAGS \
    CC CXX CPP AS AR RANLIB LD NM OBJCOPY OBJDUMP STRIP RC MT CROSS_COMPILE PERL HASHBANGPERL \
    CPATH C_INCLUDE_PATH CPLUS_INCLUDE_PATH OBJC_INCLUDE_PATH LIBRARY_PATH COMPILER_PATH \
    GCC_EXEC_PREFIX CCC_OVERRIDE_OPTIONS SDKROOT MAKEFLAGS MFLAGS MAKEFILES GNUMAKEFLAGS \
    NINJA_STATUS DESTDIR RUSTFLAGS CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS RUSTC RUSTC_WRAPPER \
    RUSTC_WORKSPACE_WRAPPER CARGO_TARGET_DIR CARGO_BUILD_TARGET
for var in $(compgen -e); do
    case "$var" in
        CMAKE_* | *_ROOT | CARGO_BUILD_* | CARGO_TARGET_* | CARGO_PROFILE_*) unset "$var" ;;
    esac
done


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

template_meson() {
    local name="$1" work="$2" args=() stray
    mapfile -t args < <(recipe_values "$name" arg)
    meson setup "$work/build" "$work/src" --cross-file "$CROSS/meson.ini" \
        --prefix="$GUEST_PREFIX" --libdir=lib --buildtype=release \
        --default-library=shared --wrap-mode=nofallback \
        "${args[@]}" >"$work/configure.log" 2>&1 ||
        { tail -n 40 "$work/configure.log" >&2; die "$name: configure failed; see $work/configure.log"; }
    ninja -C "$work/build" -j "$JOBS" >"$work/build.log" 2>&1 ||
        { tail -n 40 "$work/build.log" >&2; die "$name: build failed; see $work/build.log"; }
    DESTDIR="$work/staged" meson install -C "$work/build" --no-rebuild >"$work/install.log" 2>&1 ||
        { tail -n 20 "$work/install.log" >&2; die "$name: install failed; see $work/install.log"; }
    unstage "$name" "$work"
}

# Staged under the guest prefix, like meson's. The link flags travel in a
# response file, so neither make nor the shell expands `$ORIGIN`.
template_autotools() {
    local name="$1" work="$2" args=()
    mapfile -t args < <(recipe_values "$name" arg)
    mkdir -p "$work/build"
    (cd "$work/build" &&
        "$work/src/configure" --build="$BUILD_TRIPLE" --host="$TARGET" ac_cv_host="$TARGET" \
            --prefix="$GUEST_PREFIX" --libdir="$GUEST_PREFIX/lib" \
            CC="$CC_WRAPPER" CXX="$CXX_WRAPPER" AR="$LLVM_AR" RANLIB="$LLVM_RANLIB" \
            CFLAGS=-O2 CXXFLAGS=-O2 CC_FOR_BUILD="$CLANG" \
            CPPFLAGS="-I$PREFIX/include" LDFLAGS="-L$PREFIX/lib @$CROSS/autotools-link.rsp" \
            "${args[@]}") >"$work/configure.log" 2>&1 ||
        { tail -n 40 "$work/configure.log" >&2; die "$name: configure failed; see $work/configure.log"; }
    make -C "$work/build" -j "$JOBS" >"$work/build.log" 2>&1 ||
        { tail -n 40 "$work/build.log" >&2; die "$name: build failed; see $work/build.log"; }
    make -C "$work/build" DESTDIR="$work/staged" install >"$work/install.log" 2>&1 ||
        { tail -n 20 "$work/install.log" >&2; die "$name: install failed; see $work/install.log"; }
    unstage "$name" "$work"
}

# Moves what a template installed under the guest prefix to where the other
# recipes install, so the rest of the driver sees one layout.
unstage() {
    local name="$1" work="$2" stray
    [ -d "$work/staged$GUEST_PREFIX" ] || die "$name: the install put nothing under $GUEST_PREFIX"
    mkdir -p "$(dirname "$work/dest$PREFIX")"
    mv "$work/staged$GUEST_PREFIX" "$work/dest$PREFIX"
    stray="$(find "$work/staged" ! -type d -print | head -n 5)"
    [ -z "$stray" ] || die "$name: the install wrote outside $GUEST_PREFIX: $stray"
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

# What a lock takes with no source, as `<name> <version>` lines: the crate
# itself and the packages patched in for crates.io.
sourceless() {
    awk '
        function flush() { if (n != "" && !s) print n, v }
        /^\[\[package\]\]$/ { flush(); n = ""; v = ""; s = 0; next }
        /^name = /          { n = $3; gsub(/"/, "", n) }
        /^version = /       { v = $3; gsub(/"/, "", v) }
        /^source = /        { s = 1 }
        END                 { flush() }
    ' "$1"
}

# The libc fork, or the port `toolchain/crates/PIN` pins at that name and
# version, unpacked into <dest> and patched as its PIN pins the patch.
stage_port() {
    local name="$1" cname="$2" version="$3" dest="$4" pin sha patch rel patches=()
    if [ "$cname" = libc ]; then
        pin="$TP_PIN_REL"
        [ "$version" = "$(tp_pin_value "$REPO_ROOT/$pin" libc_version)" ] ||
            die "$name: its lock takes libc $version with no source, and the fork is libc $(tp_pin_value "$REPO_ROOT/$pin" libc_version)"
        tp_unpack_libc_crate "$REPO_ROOT" "$dest" || die "$name: could not stage the libc fork"
        patches=("$REPO_ROOT/$TP_OVERLAY_REL/libc"/*.patch)
    else
        pin="$TP_CRATES_PIN_REL"
        sha="$(tp_pin_crates "$REPO_ROOT/$pin" | awk -v n="$cname" -v v="$version" '$1 == n && $2 == v { print $3 }')"
        [ -n "$sha" ] || die "$name: its lock takes $cname $version with no source, and $pin ports no such crate"
        tp_unpack_crate "$cname" "$version" "$sha" "$dest" "$pin" || die "$name: could not stage $cname $version"
        patches=("$REPO_ROOT/$TP_CRATES_OVERLAY_REL/$cname-$version.patch")
    fi
    for patch in "${patches[@]}"; do
        rel="${patch#"$REPO_ROOT"/}"
        [ "$(tp_sha256_file "$patch")" = "$(tp_pin_patch_sha "$REPO_ROOT/$pin" "$rel")" ] ||
            die "$name: $rel does not match its pin in $pin"
        (cd "$dest" && GIT_CEILING_DIRECTORIES="$(dirname "$dest")" git apply -p1 --whitespace=nowarn "$patch") ||
            die "$name: $rel does not apply to $cname $version"
    done
}

# std is built as `build_userland.sh` builds the apps': from the owned
# sysroot, `compiler_builtins` one intrinsic per object. A build script's C
# goes through the `cc` crate, which takes the target's compiler from
# `CC_<target>`.
template_cargo() {
    local name="$1" work="$2" crate cname version program config programs=() bins=()
    [ "$(cat "$(rustc +slopos --print sysroot)/$TP_LIBRARY_REL/$TP_STD_STAMP_NAME" 2>/dev/null)" = \
        "$(tp_std_stamp "$REPO_ROOT")" ] ||
        die "$name: the slopos sysroot carries another std fork than this tree pins — run scripts/ensure_toolchain.sh"
    crate="$(basename "$(recipe_value "$name" url)" "-$(recipe_value "$name" version).crate")"
    mapfile -t programs < <(recipe_values "$name" program)
    for program in "${programs[@]}"; do
        case "$program" in
            bin/*/* | bin/) die "$name: a cargo recipe installs bin/<binary>, not $program" ;;
            bin/*) bins+=(--bin "${program#bin/}") ;;
            *) die "$name: a cargo recipe installs bin/<binary>, not $program" ;;
        esac
    done
    config="$work/cargo.toml"
    {
        printf '[target.%s]\nlinker = "%s"\n\n[patch.crates-io]\n' "$TARGET" "$CC_WRAPPER"
        while read -r cname version; do
            [ "$cname" != "$crate" ] || continue
            stage_port "$name" "$cname" "$version" "$work/crates/$cname-$version"
            printf '%s = { path = "%s" }\n' "$cname" "$work/crates/$cname-$version"
        done < <(sourceless "$work/src/Cargo.lock")
    } >"$config"
    (cd "$work/src" && env RUSTFLAGS= "CC_${TARGET//-/_}=$CC_WRAPPER" "CXX_${TARGET//-/_}=$CXX_WRAPPER" \
        "AR_${TARGET//-/_}=$LLVM_AR" cargo +slopos build --release --locked -j "$JOBS" \
        --config "$config" --config profile.release.package.compiler_builtins.codegen-units=10000 \
        -Zbuild-std=std,panic_unwind -Zunstable-options -Zjson-target-spec \
        --target "$REPO_ROOT/targets/$TARGET.json" --target-dir "$work/build" "${bins[@]}") \
        >"$work/build.log" 2>&1 ||
        { tail -n 40 "$work/build.log" >&2; die "$name: build failed; see $work/build.log"; }
    for program in "${programs[@]}"; do
        install -D -m 0755 "$work/build/$TARGET/release/${program#bin/}" "$work/dest$PREFIX/$program"
    done
}

# `git apply` under a ceiling, so the checkout around the build tree is never
# taken for the repository the patch applies to.
# `--whitespace=nowarn` overrides an `apply.whitespace` a user's git config
# sets, which would rewrite the lines a patch adds.
apply_patches() {
    local name="$1" src="$2" patch
    for patch in $(recipe_values "$name" patch); do
        (cd "$src" && GIT_CEILING_DIRECTORIES="$(dirname "$src")" \
            git apply -p1 --whitespace=nowarn "$RECIPES/$name/$patch") ||
            die "$name: $patch does not apply to $(recipe_value "$name" version)"
        (cd "$src" && GIT_CEILING_DIRECTORIES="$(dirname "$src")" \
            git apply -p1 --reverse --check "$RECIPES/$name/$patch" >/dev/null 2>&1) ||
            die "$name: $patch reported success but is not applied"
    done
}

# Change times catch an edit the build makes and then undoes: no write can set
# one back.
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
    tar -xf "$tarball" -C "$work/src" --strip-components=1 --no-same-owner || die "$name: could not unpack $tarball"
    apply_patches "$name" "$work/src"
    if [ -n "$(recipe_value "$name" lock)" ]; then
        cp "$RECIPES/$name/$(recipe_value "$name" lock)" "$work/src/Cargo.lock"
    fi
    source_manifest "$work/src" >"$work/source.manifest"
    "template_$template" "$name" "$work"
    source_manifest "$work/src" | diff "$work/source.manifest" - >"$work/source.diff" || {
        head -n 20 "$work/source.diff" >&2
        die "$name: the build changed the source tree; recipes build it as the patches leave it — see $work/source.diff"
    }

    [ -d "$work/dest$PREFIX" ] || die "$name: the install put nothing under $PREFIX"
    # A recipe of programs alone links its libraries into them, and the target
    # sysroot takes the prefix's headers whole.
    [ -n "$(recipe_values "$name" soname)" ] || rm -rf "$work/dest$PREFIX/include" "$work/dest$PREFIX/lib"
    local text
    for text in $(recipe_values "$name" license_file); do
        [ -f "$work/src/$text" ] || die "$name: the tarball carries no licence text $text"
        install -D -m 0644 "$work/src/$text" "$work/dest$PREFIX/share/licenses/$name/$text"
    done
    (cd "$work/dest$PREFIX" && find . ! -type d -print | sed 's|^\./||' | LC_ALL=C sort) >"$work/manifest"
    cp -a "$work/dest$PREFIX/." "$PREFIX/"
    installed "$name" ||
        die "$name: installed no $(recipe_values "$name" soname | sed 's|^|lib/|' | tr '\n' ' ')$(recipe_values "$name" program | tr '\n' ' ')"
    rm -rf "$work/src" "$work/build" "$work/staged" "$work/dest" "$work/source.manifest" "$work/source.diff"
    printf '%s\n' "$want" >"$work/stamp"
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
