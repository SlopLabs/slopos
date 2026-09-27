# The bootstrap settings that decide what code a compiler built from
# `third_party/slopos-rustc-src` is made of.
#
# Two configurations build one: `scripts/bootstrap_slopos_toolchain.sh`, the
# compiler that runs on SlopOS, and `scripts/make_toolchain_profile.sh`, the
# Linux-hosted compiler whose runs are that one's PGO profile. A profile keys
# each function by its name and a hash of its control flow, so it fits only a
# compiler built the same way: the two configurations differ in the host
# triple and in what a cross build needs, never in a setting in here. The
# other half of the names is `scripts/rustc_neutral_metadata.sh`, which both
# builds run as `RUSTC_WRAPPER`.
#
# Each function prints the body of one TOML table.

# [build]
rbs_build_settings() {
    cat <<'EOF'
docs = false
submodules = false
# Empty rather than bootstrap's "built from a source tarball": the version
# string is hashed into every crate's StableCrateId, and a PGO profile taken on
# the Linux-hosted twin matches the SlopOS build's symbols only if both name
# themselves alike.
description = ""
vendor = false
# jemalloc is a C library nobody has ported here, and it is the default
# allocator for a unix host.
allocator = "system"
EOF
}

# [llvm], given the directory CMake's find root is confined to. A second
# argument `plain` leaves out the release-only settings (ThinLTO), which is
# the configuration `just toolchain` builds without `--pgo`.
rbs_llvm_settings() {
    local plain="${2:-}"
    {
        cat <<EOF
download-ci-llvm = false
link-shared = true
targets = "X86"
ninja = true
# ThinLTO across LLVM, as a Rust release is built. It needs clang and lld for
# LLVM's own compile and link, which both configurations name: the host's
# clang for the build triple and the wrapper for SlopOS, which links with
# lld already. \`ar\` and \`ranlib\` must be LLVM's, because the archives
# hold bitcode.
thin-lto = true
# libc++ is linked statically, with \`-Bsymbolic\`: upstream's release
# configuration (\`-Wl,-Bsymbolic -static-libstdc++\`), and 0004 in
# toolchain/compiler/ makes \`rustc_llvm\` take \`libc++.a\` the same way. The
# loader binds eagerly, so every interposable name libLLVM used of its own —
# 5,381 symbolic relocations — was looked up anew at every start, and a
# shared libc++ was one more object to map and bind. libLLVM exports the
# runtime it links, and libclang-cpp, clang and lld, which list libLLVM before
# \`libc++.a\`, bind to that one copy: libc++'s error categories are
# singletons compared by address, and with a copy each, clang took LLVM's
# ENOENT for an error that is not ENOENT and failed on the first header its
# resource directory does not carry. librustc_driver does keep its own copy
# (\`rustc_llvm\`'s archive is read before libLLVM), which is sound because
# the wrapper only tests the error codes LLVM hands it for zero and no
# exception crosses the two: LLVM builds with \`LLVM_ENABLE_EH\` off, and
# neither it nor the wrapper references a personality routine. On the build
# triple this links LLVM against the host's \`libstdc++.a\`, as a Rust release
# does.
static-libstdcpp = true
# A cross find_package searches the *host*: without an empty find root,
# FindZLIB and friends take /usr/include, and a wchar.h that reaches glibc's
# mbstate_t collides with slibc's. Measured: it is what stopped the cross
# LLVM. The build triple's LLVM gets the same options, so both are built
# without the same optional libraries.
#
# \`LLVM_LINKER_SUPPORTS_B_SYMBOLIC_FUNCTIONS\` off: libLLVM's and
# libclang-cpp's own link options add \`-Bsymbolic-functions\` after the
# \`-Bsymbolic\` above, and the linker keeps the last of the two, so the
# objects came out with every data reference to themselves still
# interposable — 3,857 \`GLOB_DAT\`s in libLLVM, measured.
build-config = { CMAKE_FIND_ROOT_PATH = "$1", CMAKE_FIND_ROOT_PATH_MODE_INCLUDE = "ONLY", CMAKE_FIND_ROOT_PATH_MODE_LIBRARY = "ONLY", CMAKE_FIND_ROOT_PATH_MODE_PROGRAM = "NEVER", LLVM_ENABLE_ZLIB = "OFF", LLVM_ENABLE_ZSTD = "OFF", LLVM_ENABLE_TERMINFO = "OFF", LLVM_ENABLE_LIBXML2 = "OFF", LLVM_ENABLE_LIBEDIT = "OFF", LLVM_ENABLE_LIBPFM = "OFF", LLVM_ENABLE_BACKTRACES = "OFF", LLVM_ENABLE_CRASH_OVERRIDES = "OFF", LLVM_LINKER_SUPPORTS_B_SYMBOLIC_FUNCTIONS = "OFF" }
EOF
    } | if [ "$plain" = plain ]; then sed '/^# ThinLTO across LLVM/,/^thin-lto = true$/d'; else cat; fi
}

# [rust]; `plain` as above (no ThinLTO, cargo's codegen units).
rbs_rust_settings() {
    local plain="${1:-}"
    {
        cat <<'EOF'
channel = "nightly"
lld = true
rpath = true
# The pinned libc fork is upstream's release plus one module, and a newer
# rustc lints it; denying would make that fork's warnings this build's
# problem.
deny-warnings = false
# The compiler's own crates as a Rust release builds them: ThinLTO across
# librustc_driver's crates (bootstrap applies it from stage 2, the compiler
# that ships) and one codegen unit per crate. `codegen-units` also reaches
# std, which has no `codegen-units-std` here; that is std's own
# `profile.dist` value (1), so std builds as it did.
lto = "thin"
codegen-units = 1
EOF
    } | if [ "$plain" = plain ]; then sed '/^# The compiler.s own crates as a Rust release/,/^codegen-units = 1$/d'; else cat; fi
}

# `llvm-ar` and `llvm-ranlib` of the host C++ toolchain, as absolute paths:
# bootstrap hands CMake an archiver only when its path is absolute, and
# CMake's own pick for the build triple is binutils' `ar`, which cannot index
# a bitcode archive. `$1` is `LLVM_AR` as `scripts/cxx_host_tools.sh`
# resolved it; `llvm-ranlib` carries the same suffix.
rbs_llvm_archivers() {
    local ar ranlib
    ar="$(command -v "$1")" || return 1
    ranlib="$(command -v "${1/llvm-ar/llvm-ranlib}")" || return 1
    printf 'RBS_AR=%s\nRBS_RANLIB=%s\n' "$ar" "$ranlib"
}

# Clear every Rust stage out of bootstrap build directory `$1` (build triple
# `$2`) unless its crates were compiled through the `RUSTC_WRAPPER` at `$3`
# as it is now. Cargo does not fingerprint a wrapper, so without this a
# directory built before it keeps its old crate names wherever nothing else
# changed — the std a profiled compiler links against, typically — and the
# profile then fits half of the compiler. The downloaded stage0 is kept.
# With `$3` empty — a build without the wrapper — a directory that never had
# one is left as it is, and one that had is cleared back.
rbs_invalidate_rust() {
    local stamp="$1/.slopos-crate-names" want=""
    [ -z "$3" ] || want="$(sha256sum <"$3")"
    [ "$(cat "$stamp" 2>/dev/null)" != "$want" ] || return 0
    echo "rustc_build_settings: $1 was built with other crate names; clearing its Rust stages" >&2
    rm -rf "$1"/*/stage[1-9]*
    mkdir -p "$1"
    if [ -n "$want" ]; then
        printf '%s\n' "$want" >"$stamp"
    else
        rm -f "$stamp"
    fi
}

# Clear the LLVM and lld of triple `$2` out of bootstrap build directory `$1`
# unless they were built by the compiler `$3` from the inputs `$4...` (the
# settings text, a profile's checksum, a mode). Bootstrap keys LLVM's stamp on
# an llvm-project commit a tarball does not carry, and passes CMake only the
# options that are on, so a stamp removed alone would leave a switched-off
# `LLVM_BUILD_INSTRUMENTED` or `LLVM_PROFDATA_FILE` in CMakeCache.txt: the
# directories go, as opt-dist clears them between its PGO steps.
rbs_invalidate_llvm() {
    local dir="$1/$2" triple="$2" compiler="$3" stamp want
    shift 3
    stamp="$dir/.slopos-llvm-inputs"
    want="$({
        "$compiler" --version
        printf '%s\n' "$@"
    } | sha256sum)"
    [ "$(cat "$stamp" 2>/dev/null)" != "$want" ] || return 0
    if [ -e "$dir/llvm" ] || [ -e "$dir/lld" ]; then
        echo "rustc_build_settings: $triple's LLVM in $dir was built from other inputs; clearing it" >&2
    fi
    rm -rf "$dir/llvm" "$dir/lld"
    mkdir -p "$dir"
    printf '%s\n' "$want" >"$stamp"
}

# The other direction: a plain build clears the LLVM and lld of triple `$2`
# out of `$1` only if an optimised build left them there (they carry its
# stamp), and otherwise touches nothing.
rbs_forget_llvm() {
    local dir="$1/$2"
    [ -f "$dir/.slopos-llvm-inputs" ] || return 0
    echo "rustc_build_settings: $2's LLVM in $dir was built with PGO settings; clearing it" >&2
    rm -rf "$dir/llvm" "$dir/lld" "$dir/.slopos-llvm-inputs"
}
