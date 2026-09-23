#!/usr/bin/env bash
set -euo pipefail

# Build SlopOS userland binaries.
#
# Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]
#
# With --test:    also builds userland test binaries (requires testbins feature)
#
# Environment:
#   CARGO           - cargo binary (default: cargo)
#   USERLAND_TARGET - target JSON (default: targets/x86_64-unknown-slopos.json)
#   BUILD_STD       - std crates -Zbuild-std compiles (default: core,alloc,std,panic_abort)
#
# The build runs on `+slopos`, not on the rustup channel: std for
# `x86_64-unknown-slopos` comes from the pinned std + libc forks that
# scripts/make_slopos_sysroot.sh materialises into an owned sysroot, and
# `-Zbuild-std` only ever reads std from the sysroot it was invoked under.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BUILD_DIR="${1:?Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]}"
CARGO_TARGET_DIR="${2:?Usage: build_userland.sh <build_dir> <cargo_target_dir> [--test]}"
TEST_MODE="${3:-}"

CARGO="${CARGO:-cargo}"
USERLAND_TARGET="${USERLAND_TARGET:-${REPO_ROOT}/targets/x86_64-unknown-slopos.json}"
# Cargo names the output directory after the target JSON's stem.
USERLAND_TRIPLE="$(basename "$USERLAND_TARGET" .json)"

BINS="init shell coreutils terminal compositor roulette halt editor file_manager image_viewer sysmon nmap ip keymap ss nc curl ping widget_gallery oops_smoke"
BUILD_STD="${BUILD_STD:-core,alloc,std,panic_abort}"

# Install the pinned channel and materialise the owned `slopos` sysroot.
"$SCRIPT_DIR/ensure_toolchain.sh"

# Cargo fingerprints `-Zbuild-std` units by compiler version, not by the
# sysroot sources, so a fork edit restaged into the owned sysroot would leave
# the previous std in place.
SYSROOT_STAMP="$(. "$SCRIPT_DIR/lib/toolchain_pin.sh" && cat "$REPO_ROOT/$TP_SYSROOT_REL/$TP_STAMP_NAME")"
STD_STAMP="$CARGO_TARGET_DIR/$USERLAND_TRIPLE/.slopos-sysroot-stamp"
if [ "$(cat "$STD_STAMP" 2>/dev/null)" != "$SYSROOT_STAMP" ]; then
    rm -rf "${CARGO_TARGET_DIR:?}/$USERLAND_TRIPLE"
    mkdir -p "$CARGO_TARGET_DIR/$USERLAND_TRIPLE"
    printf '%s\n' "$SYSROOT_STAMP" >"$STD_STAMP"
fi

# The C++ runtime's host toolchain, resolved before any Rust builds: a host
# with no usable LLVM would otherwise learn about it a whole userland later.
# The probes are then compiled by the toolchain that built the runtime they
# link, which on a host with several majors installed is not the unsuffixed
# one.
if [ "$TEST_MODE" = "--test" ]; then
    CXX_TOOLS="$("$SCRIPT_DIR/cxx_host_tools.sh")"
    eval "$CXX_TOOLS"
fi

mkdir -p "$BUILD_DIR"

# crt0.o, the C-program entry object: `_start` lives in the `slopos-crt0`
# crate (slibc's crt0), not in any binary, so `ENTRY(_start)` in
# userland/userland.ld resolves to nothing unless every binary links it.
# Emitted as a single object (`codegen-units=1`) and passed on the link line
# of the binary builds below.
CRT0_OBJ="$(cd "$BUILD_DIR" && pwd)/crt0.o"
# RUSTFLAGS rather than `cargo rustc` for the binaries, because they are built
# in one invocation and `cargo rustc` takes a single target. A link-arg is
# inert for the rlib units it also reaches, and carrying it on the crt0 build
# too keeps one `-Zbuild-std` fingerprint across every invocation below rather
# than rebuilding core for each.
#
# The linker script and `--emit-relocs` used to live in the target spec's
# `pre-link-args`, where every artifact built for this target got them. The
# shared objects below must not: `userland.ld` fixes an image at 0x400000 and
# discards `.interp`, which is the opposite of what a `.so` needs.
#
# `relocation-model=static` is here rather than in the target spec because the
# spec must say `pic`: a target that allows dynamic linking and does not is
# rejected by rustc's own builtin-target consistency check, and `libc.so` needs
# the permission. Everything built with these flags is a non-PIE image at a
# fixed address; the shared objects pin `pic` on their own build line.
#
# The target links through `cc`, which is what a program built *on* SlopOS
# wants. The system's own artifacts carry a hand-written link line instead,
# so they name the linker, and they keep `panic = abort`.
SYSTEM_RUSTFLAGS="-C linker=rust-lld -C linker-flavor=ld.lld -C panic=abort"
USERLAND_RUSTFLAGS="$SYSTEM_RUSTFLAGS -C relocation-model=static -C link-arg=$CRT0_OBJ -C link-arg=-Tuserland/userland.ld -C link-arg=--emit-relocs"

rm -f "$CRT0_OBJ"
# `--emit=obj` is a side effect of *compiling*, so a warm fingerprint makes
# cargo print `Finished` and write no object at all — deleting crt0.o alone is
# not enough to get it back. Cleaning just this package forces the one
# compilation that emits it (~1 s; `core` stays cached).
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
$CARGO +slopos clean \
    --package slopos-crt0 \
    --release \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" >/dev/null
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="$USERLAND_RUSTFLAGS" \
$CARGO +slopos rustc --locked \
    -Zbuild-std=core \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" \
    --package slopos-crt0 \
    --release \
    -- --emit=obj="$CRT0_OBJ" -Ccodegen-units=1
if [ ! -f "$CRT0_OBJ" ]; then
    echo "build_userland: slopos-crt0 built but emitted no object at $CRT0_OBJ" >&2
    exit 1
fi

# Build main userland binaries
BIN_ARGS=()
for bin in $BINS; do
    BIN_ARGS+=(--bin "$bin")
done

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="$USERLAND_RUSTFLAGS" \
$CARGO +slopos build --locked \
    -Zbuild-std="$BUILD_STD" \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" \
    --package slopos-userland \
    "${BIN_ARGS[@]}" \
    --no-default-features \
    --release

# Copy built binaries
RELEASE_DIR="${CARGO_TARGET_DIR}/${USERLAND_TRIPLE}/release"
for bin in $BINS; do
    if [ -f "$RELEASE_DIR/$bin" ]; then
        cp "$RELEASE_DIR/$bin" "$BUILD_DIR/${bin}.elf"
    fi
done

echo "Userland binaries built: $(for b in $BINS; do printf '%s/%s.elf ' "$BUILD_DIR" "$b"; done)"

# Build test binaries if requested
if [ "$TEST_MODE" = "--test" ]; then
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    RUSTFLAGS="$USERLAND_RUSTFLAGS" \
    $CARGO +slopos build --locked \
        -Zbuild-std="$BUILD_STD" \
        -Zbuild-std-features=compiler-builtins-mem \
        -Zunstable-options \
        -Zjson-target-spec \
        --target "$USERLAND_TARGET" \
        --package slopos-userland \
        --bin dl_test \
        --bin cxx_test \
        --bin fork_test \
        --bin io_capture_test \
        --bin heap_allocator_test \
        --bin image_test \
        --bin curl_recv_repro_test \
        --bin curl_e2e_test \
        --bin cd_test \
        --bin buildctl_test \
        --bin coreutils_test \
        --bin ring_test \
        --bin pidfd_e2e_test \
        --bin signalfd_test \
        --bin slopfut_test \
        --bin multishot_test \
        --bin tls_independence_test \
        --bin percore_reactor_test \
        --bin signal_handler_test \
        --bin sigwinch_default_test \
        --bin ctrlc_flood_test \
        --bin pty_flow_test \
        --bin mm_stress_test \
        --bin bigprog_test \
        --bin buildloop_test \
        --bin exit_stress_test \
        --bin spin_signal_test \
        --bin terminal_grid_test \
        --bin sysmon_selection_test \
        --bin clipboard_test \
        --bin keymap_test \
        --bin appkit_test \
        --bin editor_test \
        --bin spawn_privilege_test \
        --bin seat_test \
        --bin mount_test \
        --bin devdisk_test \
        --bin selfhost_test \
        --bin shell_script_test \
        --bin stdio_stream_test \
        --bin ip_e2e_test \
        --bin rlimit_test \
        --bin session_smoke_test \
        --bin spawn_output_test \
        --bin dns_resolve_test \
        --bin dns_concurrent_test \
        --bin transfer_test \
        --bin persist_test \
        --bin libc_abi_test \
        --features testbins \
        --no-default-features \
        --release

    if [ -f "$RELEASE_DIR/dl_test" ]; then
        cp "$RELEASE_DIR/dl_test" "$BUILD_DIR/dl_test.elf"
    fi
    if [ -f "$RELEASE_DIR/cxx_test" ]; then
        cp "$RELEASE_DIR/cxx_test" "$BUILD_DIR/cxx_test.elf"
    fi
    if [ -f "$RELEASE_DIR/fork_test" ]; then
        cp "$RELEASE_DIR/fork_test" "$BUILD_DIR/fork_test.elf"
    fi
    if [ -f "$RELEASE_DIR/io_capture_test" ]; then
        cp "$RELEASE_DIR/io_capture_test" "$BUILD_DIR/io_capture_test.elf"
    fi
    if [ -f "$RELEASE_DIR/heap_allocator_test" ]; then
        cp "$RELEASE_DIR/heap_allocator_test" "$BUILD_DIR/heap_allocator_test.elf"
    fi
    if [ -f "$RELEASE_DIR/image_test" ]; then
        cp "$RELEASE_DIR/image_test" "$BUILD_DIR/image_test.elf"
    fi
    if [ -f "$RELEASE_DIR/curl_recv_repro_test" ]; then
        cp "$RELEASE_DIR/curl_recv_repro_test" "$BUILD_DIR/curl_recv_repro_test.elf"
    fi
    if [ -f "$RELEASE_DIR/curl_e2e_test" ]; then
        cp "$RELEASE_DIR/curl_e2e_test" "$BUILD_DIR/curl_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/cd_test" ]; then
        cp "$RELEASE_DIR/cd_test" "$BUILD_DIR/cd_test.elf"
    fi
    if [ -f "$RELEASE_DIR/buildctl_test" ]; then
        cp "$RELEASE_DIR/buildctl_test" "$BUILD_DIR/buildctl_test.elf"
    fi
    if [ -f "$RELEASE_DIR/coreutils_test" ]; then
        cp "$RELEASE_DIR/coreutils_test" "$BUILD_DIR/coreutils_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ring_test" ]; then
        cp "$RELEASE_DIR/ring_test" "$BUILD_DIR/ring_test.elf"
    fi
    if [ -f "$RELEASE_DIR/pidfd_e2e_test" ]; then
        cp "$RELEASE_DIR/pidfd_e2e_test" "$BUILD_DIR/pidfd_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/signalfd_test" ]; then
        cp "$RELEASE_DIR/signalfd_test" "$BUILD_DIR/signalfd_test.elf"
    fi
    if [ -f "$RELEASE_DIR/slopfut_test" ]; then
        cp "$RELEASE_DIR/slopfut_test" "$BUILD_DIR/slopfut_test.elf"
    fi
    if [ -f "$RELEASE_DIR/multishot_test" ]; then
        cp "$RELEASE_DIR/multishot_test" "$BUILD_DIR/multishot_test.elf"
    fi
    if [ -f "$RELEASE_DIR/tls_independence_test" ]; then
        cp "$RELEASE_DIR/tls_independence_test" "$BUILD_DIR/tls_independence_test.elf"
    fi
    if [ -f "$RELEASE_DIR/percore_reactor_test" ]; then
        cp "$RELEASE_DIR/percore_reactor_test" "$BUILD_DIR/percore_reactor_test.elf"
    fi
    if [ -f "$RELEASE_DIR/signal_handler_test" ]; then
        cp "$RELEASE_DIR/signal_handler_test" "$BUILD_DIR/signal_handler_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ctrlc_flood_test" ]; then
        cp "$RELEASE_DIR/ctrlc_flood_test" "$BUILD_DIR/ctrlc_flood_test.elf"
    fi
    if [ -f "$RELEASE_DIR/pty_flow_test" ]; then
        cp "$RELEASE_DIR/pty_flow_test" "$BUILD_DIR/pty_flow_test.elf"
    fi
    if [ -f "$RELEASE_DIR/mm_stress_test" ]; then
        cp "$RELEASE_DIR/mm_stress_test" "$BUILD_DIR/mm_stress_test.elf"
    fi
    if [ -f "$RELEASE_DIR/bigprog_test" ]; then
        cp "$RELEASE_DIR/bigprog_test" "$BUILD_DIR/bigprog_test.elf"
    fi
    if [ -f "$RELEASE_DIR/buildloop_test" ]; then
        cp "$RELEASE_DIR/buildloop_test" "$BUILD_DIR/buildloop_test.elf"
    fi
    if [ -f "$RELEASE_DIR/exit_stress_test" ]; then
        cp "$RELEASE_DIR/exit_stress_test" "$BUILD_DIR/exit_stress_test.elf"
    fi
    if [ -f "$RELEASE_DIR/sigwinch_default_test" ]; then
        cp "$RELEASE_DIR/sigwinch_default_test" "$BUILD_DIR/sigwinch_default_test.elf"
    fi
    if [ -f "$RELEASE_DIR/spin_signal_test" ]; then
        cp "$RELEASE_DIR/spin_signal_test" "$BUILD_DIR/spin_signal_test.elf"
    fi
    if [ -f "$RELEASE_DIR/terminal_grid_test" ]; then
        cp "$RELEASE_DIR/terminal_grid_test" "$BUILD_DIR/terminal_grid_test.elf"
    fi
    if [ -f "$RELEASE_DIR/sysmon_selection_test" ]; then
        cp "$RELEASE_DIR/sysmon_selection_test" "$BUILD_DIR/sysmon_selection_test.elf"
    fi
    if [ -f "$RELEASE_DIR/clipboard_test" ]; then
        cp "$RELEASE_DIR/clipboard_test" "$BUILD_DIR/clipboard_test.elf"
    fi
    if [ -f "$RELEASE_DIR/keymap_test" ]; then
        cp "$RELEASE_DIR/keymap_test" "$BUILD_DIR/keymap_test.elf"
    fi
    if [ -f "$RELEASE_DIR/appkit_test" ]; then
        cp "$RELEASE_DIR/appkit_test" "$BUILD_DIR/appkit_test.elf"
    fi
    if [ -f "$RELEASE_DIR/editor_test" ]; then
        cp "$RELEASE_DIR/editor_test" "$BUILD_DIR/editor_test.elf"
    fi
    if [ -f "$RELEASE_DIR/spawn_privilege_test" ]; then
        cp "$RELEASE_DIR/spawn_privilege_test" "$BUILD_DIR/spawn_privilege_test.elf"
    fi
    if [ -f "$RELEASE_DIR/seat_test" ]; then
        cp "$RELEASE_DIR/seat_test" "$BUILD_DIR/seat_test.elf"
    fi
    if [ -f "$RELEASE_DIR/mount_test" ]; then
        cp "$RELEASE_DIR/mount_test" "$BUILD_DIR/mount_test.elf"
    fi
    if [ -f "$RELEASE_DIR/devdisk_test" ]; then
        cp "$RELEASE_DIR/devdisk_test" "$BUILD_DIR/devdisk_test.elf"
    fi
    if [ -f "$RELEASE_DIR/selfhost_test" ]; then
        cp "$RELEASE_DIR/selfhost_test" "$BUILD_DIR/selfhost_test.elf"
    fi
    if [ -f "$RELEASE_DIR/stdio_stream_test" ]; then
        cp "$RELEASE_DIR/stdio_stream_test" "$BUILD_DIR/stdio_stream_test.elf"
    fi
    if [ -f "$RELEASE_DIR/shell_script_test" ]; then
        cp "$RELEASE_DIR/shell_script_test" "$BUILD_DIR/shell_script_test.elf"
    fi
    if [ -f "$RELEASE_DIR/stdio_stream_test" ]; then
        cp "$RELEASE_DIR/stdio_stream_test" "$BUILD_DIR/stdio_stream_test.elf"
    fi
    if [ -f "$RELEASE_DIR/ip_e2e_test" ]; then
        cp "$RELEASE_DIR/ip_e2e_test" "$BUILD_DIR/ip_e2e_test.elf"
    fi
    if [ -f "$RELEASE_DIR/rlimit_test" ]; then
        cp "$RELEASE_DIR/rlimit_test" "$BUILD_DIR/rlimit_test.elf"
    fi
    if [ -f "$RELEASE_DIR/session_smoke_test" ]; then
        cp "$RELEASE_DIR/session_smoke_test" "$BUILD_DIR/session_smoke_test.elf"
    fi
    if [ -f "$RELEASE_DIR/spawn_output_test" ]; then
        cp "$RELEASE_DIR/spawn_output_test" "$BUILD_DIR/spawn_output_test.elf"
    fi
    if [ -f "$RELEASE_DIR/dns_resolve_test" ]; then
        cp "$RELEASE_DIR/dns_resolve_test" "$BUILD_DIR/dns_resolve_test.elf"
    fi
    if [ -f "$RELEASE_DIR/dns_concurrent_test" ]; then
        cp "$RELEASE_DIR/dns_concurrent_test" "$BUILD_DIR/dns_concurrent_test.elf"
    fi
    if [ -f "$RELEASE_DIR/transfer_test" ]; then
        cp "$RELEASE_DIR/transfer_test" "$BUILD_DIR/transfer_test.elf"
    fi
    if [ -f "$RELEASE_DIR/persist_test" ]; then
        cp "$RELEASE_DIR/persist_test" "$BUILD_DIR/persist_test.elf"
    fi
    if [ -f "$RELEASE_DIR/libc_abi_test" ]; then
        cp "$RELEASE_DIR/libc_abi_test" "$BUILD_DIR/libc_abi_test.elf"
    fi

    echo "Userland test binaries built:$BUILD_DIR/fork_test.elf $BUILD_DIR/io_capture_test.elf $BUILD_DIR/heap_allocator_test.elf $BUILD_DIR/image_test.elf $BUILD_DIR/curl_recv_repro_test.elf $BUILD_DIR/curl_e2e_test.elf $BUILD_DIR/cd_test.elf $BUILD_DIR/ring_test.elf $BUILD_DIR/pidfd_e2e_test.elf $BUILD_DIR/signalfd_test.elf $BUILD_DIR/slopfut_test.elf $BUILD_DIR/multishot_test.elf $BUILD_DIR/tls_independence_test.elf $BUILD_DIR/percore_reactor_test.elf $BUILD_DIR/signal_handler_test.elf $BUILD_DIR/ctrlc_flood_test.elf $BUILD_DIR/pty_flow_test.elf $BUILD_DIR/mm_stress_test.elf $BUILD_DIR/bigprog_test.elf $BUILD_DIR/sigwinch_default_test.elf $BUILD_DIR/spin_signal_test.elf $BUILD_DIR/terminal_grid_test.elf $BUILD_DIR/sysmon_selection_test.elf $BUILD_DIR/clipboard_test.elf $BUILD_DIR/keymap_test.elf $BUILD_DIR/appkit_test.elf $BUILD_DIR/editor_test.elf $BUILD_DIR/spawn_privilege_test.elf $BUILD_DIR/seat_test.elf $BUILD_DIR/mount_test.elf $BUILD_DIR/stdio_stream_test.elf $BUILD_DIR/shell_script_test.elf $BUILD_DIR/ip_e2e_test.elf"
fi

# libc.a, the archive a `cc`-style link line finds via `-lc`. No binary in this
# tree links it — the `slopos-slibc-staticlib` wrapper exists for C consumers —
# so without this step nothing ever compiles it, and the `#[panic_handler]` the
# archive has to carry (a duplicate lang item for slibc's rlib users, which is
# why it lives in a wrapper package) rots unobserved.
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="$USERLAND_RUSTFLAGS -C force-unwind-tables" \
$CARGO +slopos build --locked \
    -Zbuild-std="$BUILD_STD" \
    -Zbuild-std-features=compiler-builtins-mem \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" \
    --package slopos-slibc-staticlib \
    --release
if [ ! -f "$RELEASE_DIR/libc.a" ]; then
    echo "build_userland: slopos-slibc-staticlib built but emitted no archive at $RELEASE_DIR/libc.a" >&2
    exit 1
fi

echo "C archive built: $RELEASE_DIR/libc.a"

# libc.so, which is both the shared C library and the program interpreter.
#
#   -Bsymbolic  binds its own references at link time, so the only relocation
#               its pre-relocation bootstrap has to apply is RELATIVE.
#   -z now      eager binding; the loader writes no GOT slot after startup,
#               which is what makes full RELRO free.
#   --soname    what a DT_NEEDED on the C library resolves to: the already
#               mapped interpreter rather than a second copy of it.
#   --entry     the interpreter entry the kernel jumps to.
# No linker script and no crt0.o: neither belongs in a shared object, and no
# `compiler-builtins-mem`: it defines memcpy, memset, memcmp and strlen with
# hidden visibility, which wins over slibc's own and leaves them out of
# `.dynsym` — a C program linking libc.so could not call them.
# `-C force-unwind-tables` because this artifact carries the Level-1 unwinder:
# `_Unwind_RaiseException` saves its context and looks the resulting return
# address up first, so the very first frame of every unwind is one of its own.
# A `panic = abort` Rust artifact emits no `.eh_frame` at all, and without
# these an exception ends at frame zero with `_URC_END_OF_STACK` — measured,
# and indistinguishable from a program with no handler.
SO_RUSTFLAGS="$SYSTEM_RUSTFLAGS -C relocation-model=pic -Z tls-model=initial-exec -C force-unwind-tables"

# compiler-rt, position independent, for the shared objects. `libc.a` has the
# same routines and cannot supply them: it is built for the fixed-address
# images, so its relocations are the ones a `.so` may not carry.
CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="$SYSTEM_RUSTFLAGS -C relocation-model=pic" \
$CARGO +slopos build --locked \
    -Zbuild-std=core \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" \
    --package slopos-slibc-builtins \
    --release
if [ ! -f "$RELEASE_DIR/libbuiltins.a" ]; then
    echo "build_userland: slopos-slibc-builtins built but emitted no archive at $RELEASE_DIR/libbuiltins.a" >&2
    exit 1
fi
# Staged beside `libc.so`, because the C++ runtime's stamp names it and the
# gate that reads that stamp knows only the staging directory.
cp "$RELEASE_DIR/libbuiltins.a" "$BUILD_DIR/libbuiltins.a"

CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
RUSTFLAGS="$SO_RUSTFLAGS -C link-arg=-Bsymbolic -C link-arg=-znow -C link-arg=--soname=libc.so -C link-arg=--entry=_dlstart" \
$CARGO +slopos build --locked \
    -Zbuild-std=core,alloc \
    -Zunstable-options \
    -Zjson-target-spec \
    --target "$USERLAND_TARGET" \
    --package slopos-slibc-cdylib \
    --release
if [ ! -f "$RELEASE_DIR/libc.so" ]; then
    echo "build_userland: slopos-slibc-cdylib built but emitted no shared object at $RELEASE_DIR/libc.so" >&2
    exit 1
fi
cp "$RELEASE_DIR/libc.so" "$BUILD_DIR/libc.so"

echo "C shared library built: $BUILD_DIR/libc.so"

if [ "$TEST_MODE" = "--test" ]; then
    # The dynamically linked probe and the object it dlopens. Built against
    # libc.so rather than the slibc rlib, which is the whole point of them:
    # two copies of the C library in one process is the bug `libc.so` exists
    # to prevent, and only a program that links none of it can prove it.
    #
    # --export-dynamic on the probe is what lets the shared object bind back
    # to a symbol the executable defines.
    DL_LINK="-C link-arg=-L$RELEASE_DIR -C link-arg=-lc"
    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    RUSTFLAGS="$SO_RUSTFLAGS -Z tls-model=global-dynamic $DL_LINK -C link-arg=-znow -C link-arg=--soname=libdltest.so" \
    $CARGO +slopos build --locked \
        -Zbuild-std=core \
        -Zunstable-options \
        -Zjson-target-spec \
        --target "$USERLAND_TARGET" \
        --package slopos-dltest \
        --lib \
        --release

    CARGO_TARGET_DIR="$CARGO_TARGET_DIR" \
    RUSTFLAGS="$SYSTEM_RUSTFLAGS -C relocation-model=static -C link-arg=$CRT0_OBJ $DL_LINK -C link-arg=--image-base=0x400000 -C link-arg=--dynamic-linker=/lib/ld-slopos.so.1 -C link-arg=--export-dynamic -C link-arg=-znow" \
    $CARGO +slopos build --locked \
        -Zbuild-std=core \
        -Zunstable-options \
        -Zjson-target-spec \
        --target "$USERLAND_TARGET" \
        --package slopos-dltest \
        --bin dl_probe \
        --release

    for artifact in libdltest.so dl_probe; do
        if [ ! -f "$RELEASE_DIR/$artifact" ]; then
            echo "build_userland: slopos-dltest emitted no $artifact" >&2
            exit 1
        fi
    done
    cp "$RELEASE_DIR/libdltest.so" "$BUILD_DIR/libdltest.so"
    cp "$RELEASE_DIR/dl_probe" "$BUILD_DIR/dl_probe.elf"

    echo "Dynamic probe built: $BUILD_DIR/dl_probe.elf $BUILD_DIR/libdltest.so"

    # The library-search probes. One C program linked four ways, so each
    # carries the DT_RPATH or DT_RUNPATH a dl_test case needs; dl_test lays
    # copies out under /tmp. `libdlsearch.so` ships as
    # `libdlsearch-fixture.so` so the /lib fallback can never find it by name:
    # a case passes only through the path it is about.
    DLS_CC=("$CLANG" "--target=${USERLAND_TRIPLE}" -nostdlibinc
        -isystem "${REPO_ROOT}/slibc/include" -std=c11 -O2 -Wall -Wextra -Werror)
    DLS_FIXTURE="$BUILD_DIR/libdlsearch-fixture.so"
    "${DLS_CC[@]}" -fPIC -c "${REPO_ROOT}/userland/dltest/search_lib.c" -o "$BUILD_DIR/dlsearch-lib.o"
    "${DLS_CC[@]}" -fPIC -c "${REPO_ROOT}/userland/dltest/search_dep.c" -o "$BUILD_DIR/dlsearch-dep.o"
    "${DLS_CC[@]}" -c "${REPO_ROOT}/userland/dltest/search_probe.c" -o "$BUILD_DIR/dlsearch-probe.o"
    "$LD_LLD" -shared -znow -o "$DLS_FIXTURE" "$BUILD_DIR/dlsearch-lib.o" --soname=libdlsearch.so
    "$LD_LLD" -shared -znow -o "$BUILD_DIR/libdlrunpath.so" "$BUILD_DIR/dlsearch-dep.o" \
        --soname=libdlrunpath.so "$DLS_FIXTURE" --enable-new-dtags -rpath '$ORIGIN'
    "$LD_LLD" -shared -znow -o "$BUILD_DIR/libdlplain.so" "$BUILD_DIR/dlsearch-dep.o" \
        --soname=libdlplain.so "$DLS_FIXTURE"
    DLS_LINK=("$CRT0_OBJ" "$BUILD_DIR/dlsearch-probe.o" --eh-frame-hdr -znow
        --image-base=0x400000 --dynamic-linker=/lib/ld-slopos.so.1
        -L "$RELEASE_DIR" -lc "$RELEASE_DIR/libbuiltins.a")
    "$LD_LLD" -o "$BUILD_DIR/dl_search_origin.elf" "${DLS_LINK[@]}" "$DLS_FIXTURE" \
        --enable-new-dtags -rpath '$ORIGIN/../lib'
    "$LD_LLD" -o "$BUILD_DIR/dl_search_rpath.elf" "${DLS_LINK[@]}" \
        --disable-new-dtags -rpath '${ORIGIN}/rpath'
    "$LD_LLD" -o "$BUILD_DIR/dl_search_runpath.elf" "${DLS_LINK[@]}" \
        --enable-new-dtags -rpath '$ORIGIN/runpath'
    # Granted in core/src/exec/grants.rs, so it runs with AT_SECURE: both
    # entries it would honour otherwise point at a copy it must not load.
    "$LD_LLD" -o "$BUILD_DIR/dl_secure_probe.elf" "${DLS_LINK[@]}" \
        --enable-new-dtags -rpath '$ORIGIN/../tmp/dl_secure:/tmp/dl_secure_abs'
    echo "Search probes built: dl_search_{origin,rpath,runpath} dl_secure_probe"

    # The C library's own surface, compiled from C against the generated
    # headers, so a header that disagrees with its export fails here at
    # compile time. `-Wsystem-headers` is what makes those diagnostics
    # reachable: clang silences warnings raised inside an `-isystem`
    # directory, which is how a C consumer names this one.
    "$CLANG" "--target=${USERLAND_TRIPLE}" -nostdlibinc \
        -isystem "${REPO_ROOT}/slibc/include" -std=c11 -O2 \
        -Wall -Wextra -Wsystem-headers -Werror \
        -c "${REPO_ROOT}/userland/libctest/probe.c" -o "$BUILD_DIR/libctest-probe.o"
    "$LD_LLD" -static -o "$BUILD_DIR/libc_probe.elf" \
        "$CRT0_OBJ" "$BUILD_DIR/libctest-probe.o" --eh-frame-hdr \
        --image-base=0x400000 -L "$RELEASE_DIR" -lc

    echo "C probe built: $BUILD_DIR/libc_probe.elf"

    # The C++ runtime and the three artifacts that prove it works. Cross-built
    # from this host and never in the guest, and staged only here, because the
    # shipped appliance root runs no C++ program.
    # `CLANG*`/`LD_LLD` are the ones resolved at the top of this script.
    "$SCRIPT_DIR/make_slopos_cxx.sh" "$RELEASE_DIR"
    CXX_DIR="${REPO_ROOT}/third_party/slopos-cxx"
    cp "$CXX_DIR/lib/libc++.so" "$BUILD_DIR/libc++.so"
    rm -rf "$BUILD_DIR/libc++-licenses"
    cp -r "$CXX_DIR/licenses" "$BUILD_DIR/libc++-licenses"

    # The libc++ include directory comes before slibc's: `<cstdlib>` is a
    # libc++ header that reaches the C one through `#include_next`, and the
    # other order makes it find slibc's `<stdlib.h>` first and stop with a
    # diagnostic about exactly this.
    # Captured rather than substituted inside the array: an array assignment
    # does not propagate a failing command substitution even under `set -e`,
    # so a helper that broke would silently drop the ABI flag and build the
    # probes against a `ctype_base::mask` the runtime does not share.
    read -ra CXX_ABI_FLAGS <<<"$("$SCRIPT_DIR/make_slopos_cxx.sh" --print-abi-flags)"
    [ "${#CXX_ABI_FLAGS[@]}" -gt 0 ] ||
        { echo "build_userland: make_slopos_cxx.sh --print-abi-flags printed nothing" >&2; exit 1; }
    CXX_COMPILE=(
        "$CLANGXX"
        "--target=${USERLAND_TRIPLE}"
        -nostdlibinc
        -nostdinc++
        -isystem "$CXX_DIR/include/c++/v1"
        -isystem "${REPO_ROOT}/slibc/include"
        "${CXX_ABI_FLAGS[@]}"
        -std=c++20
        -O2
    )
    # `--eh-frame-hdr` on both: the unwinder's frame finder looks for a
    # `PT_GNU_EH_FRAME` per object, and an object without one is one a throw
    # cannot unwind out of. `--export-dynamic` on the probe is what lets the
    # loaded object bind back to the type it throws.
    # `libbuiltins.a` last, for the reason `make_slopos_cxx.sh` reads it last:
    # a C++ program that calls a 128-bit helper finds it in an archive rather
    # than failing to link with nothing to point at.
    CXX_LINK=(
        --eh-frame-hdr
        -znow
        -zrelro
        -L "$RELEASE_DIR"
        -lc
        -L "$CXX_DIR/lib"
        -lc++
        "$RELEASE_DIR/libbuiltins.a"
    )

    "${CXX_COMPILE[@]}" -fPIC -c "${REPO_ROOT}/userland/cxxtest/lib.cpp" \
        -o "$BUILD_DIR/cxxtest-lib.o"
    "${CXX_COMPILE[@]}" -c "${REPO_ROOT}/userland/cxxtest/probe.cpp" \
        -o "$BUILD_DIR/cxxtest-probe.o"

    "$LD_LLD" -shared -o "$BUILD_DIR/libcxxtest.so" "$BUILD_DIR/cxxtest-lib.o" \
        --soname=libcxxtest.so "${CXX_LINK[@]}"
    "$LD_LLD" -o "$BUILD_DIR/cxx_probe.elf" "$CRT0_OBJ" "$BUILD_DIR/cxxtest-probe.o" \
        --image-base=0x400000 --dynamic-linker=/lib/ld-slopos.so.1 --export-dynamic \
        "${CXX_LINK[@]}"

    # The same runtime as archives. `--start-group` because libc++abi calls
    # back into libc++ and `libc.a` carries the unwinder both of them call, so
    # no single order of three archives resolves.
    "${CXX_COMPILE[@]}" -c "${REPO_ROOT}/userland/cxxtest/static_probe.cpp" \
        -o "$BUILD_DIR/cxxtest-static-probe.o"
    "$LD_LLD" -static -o "$BUILD_DIR/cxx_static_probe.elf" \
        "$CRT0_OBJ" "$BUILD_DIR/cxxtest-static-probe.o" --eh-frame-hdr \
        --image-base=0x400000 \
        -L "$CXX_DIR/lib" -L "$RELEASE_DIR" --start-group -lc++ -lc --end-group

    echo "C++ probe built: $BUILD_DIR/cxx_probe.elf $BUILD_DIR/libcxxtest.so" \
        "$BUILD_DIR/cxx_static_probe.elf"
fi
