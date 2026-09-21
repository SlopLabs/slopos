#!/usr/bin/env bash
# Verify the pinned std, libc and compiler forks against what is on disk.
#
# This gate replaces the retired std-patching script's `cfg_select!` arm-order
# check, which existed because an arm placed after the `_` wildcard is dead
# code that still compiles — it once shipped as a `ud2` in
# `std::process::exit`.
# One slopos arm outlives that check: the std patch adds a
# `target_os = "slopos"` arm to the `cfg_select!` in
# `std/src/sys/random/mod.rs`. It needs no gate because that select's wildcard
# is `_ => {}`, empty — an arm placed after it leaves `fill_bytes` undefined
# and the build fails loudly. The retired check was needed for the opposite
# shape, where the fallback supplied a working symbol and the dead arm could
# ship unnoticed.
# The failure mode moves instead: a fork can drift from the compiler it was
# cut against, a patch can change without its pin changing, and a materialised
# tree can go stale or lose its registration. All of them are silent — a stale
# tree compiles, it just compiles last week's fork — so they are gated rather
# than reviewed.
#
# The four failures:
#   1. `toolchain/PIN`'s channel disagrees with `rust-toolchain.toml`.
#   2. A `toolchain/**/*.patch` file's sha256 disagrees with the line in its
#      own PIN — `toolchain/compiler/PIN` for the compiler fork, `toolchain/PIN`
#      for the other two — or has no line, or a line names a patch that is not
#      there.
#   3. A materialised tree — `third_party/rust-slopos` for the std and libc
#      forks, `third_party/slopos-rustc-src` for the compiler fork — carries a
#      stamp its overlay no longer hashes to, or is missing a file its patches
#      create.
#   4. A linked `slopos` toolchain points somewhere other than
#      `third_party/rust-slopos`.
#
# Checks 3 and 4 are conditional on the thing existing, so CI that materialises
# neither tree still passes — the pins themselves are what every checkout must
# agree on.
#
# Usage: check_toolchain_pin.sh [--self-test]
#        check_toolchain_pin.sh --root <dir>   (self-test plumbing)

set -euo pipefail

SELF="check_toolchain_pin"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

. "$SCRIPT_DIR/lib/toolchain_pin.sh"

SELF_TEST=0
ROOT="$REPO_ROOT"
case "${1:-}" in
    --self-test) SELF_TEST=1 ;;
    --root) ROOT="${2:?usage: $SELF.sh --root <dir>}" ;;
    "") ;;
    *) echo "usage: $SELF.sh [--self-test]" >&2; exit 2 ;;
esac

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------
run_gate() {
    local root="$1"
    local pin="$root/$TP_PIN_REL"
    local bad=0
    fail() {
        echo "$SELF: $*" >&2
        bad=1
    }

    if [ ! -d "$root/$TP_OVERLAY_REL" ] || [ ! -f "$pin" ]; then
        echo "$SELF: missing $TP_PIN_REL — the fork overlay is tracked in-repo" >&2
        return 1
    fi

    local channel pin_channel libc_version libc_checksum
    channel="$(tp_channel "$root")"
    pin_channel="$(tp_pin_value "$pin" channel)"
    libc_version="$(tp_pin_value "$pin" libc_version)"
    libc_checksum="$(tp_pin_value "$pin" libc_checksum)"

    [ -n "$channel" ] || fail "cannot read the channel from rust-toolchain.toml"
    [ -n "$pin_channel" ] || fail "$TP_PIN_REL has no \`channel=\` line"
    [ -n "$libc_version" ] || fail "$TP_PIN_REL has no \`libc_version=\` line"
    if ! printf '%s' "$libc_checksum" | grep -Eq '^[0-9a-fA-F]{64}$'; then
        fail "$TP_PIN_REL has no \`libc_checksum=<sha256>\` line"
    fi

    # 1. The fork is cut against one compiler.
    if [ -n "$channel" ] && [ -n "$pin_channel" ] && [ "$channel" != "$pin_channel" ]; then
        fail "channel drift: $TP_PIN_REL pins $pin_channel, rust-toolchain.toml says $channel
       Re-cut the patches against $channel, then update the pin."
    fi

    # 2. Every patch file pinned, every pinned patch present.
    local rel pin_rel patch_pin want got count=0
    for rel in $(tp_patch_files "$root"); do
        count=$((count + 1))
        pin_rel="$(tp_patch_pin_file "$rel")"
        patch_pin="$root/$pin_rel"
        if [ ! -f "$patch_pin" ]; then
            fail "$rel is pinned by $pin_rel, which does not exist"
            continue
        fi
        want="$(tp_pin_patch_sha "$patch_pin" "$rel")"
        if [ -z "$want" ]; then
            fail "$rel has no \`patch_sha256=$rel:<sha256>\` line in $pin_rel"
            continue
        fi
        got="$(tp_sha256_file "$root/$rel")"
        if [ "$got" != "$want" ]; then
            fail "$rel does not match its pin
       expected: $want ($pin_rel)
       actual:   $got"
        fi
    done
    # The other direction: a pin line whose patch was deleted or renamed.
    for pin_rel in $(tp_pin_files); do
        [ -f "$root/$pin_rel" ] || continue
        while read -r rel _; do
            [ -n "$rel" ] || continue
            [ -f "$root/$rel" ] || fail "$pin_rel pins $rel, which does not exist"
        done <<EOF
$(tp_pin_patches "$root/$pin_rel")
EOF
    done

    # 3. Each materialised tree must match the overlay it was built from, and
    #    only that one: the two stamps are what keep a compiler-fork edit from
    #    restamping the sysroot.
    local tree_rel want maker stamp tree note=""
    for tree_rel in "$TP_SYSROOT_REL" "$TP_RUSTC_SRC_REL"; do
        tree="$root/$tree_rel"
        [ -d "$tree" ] || continue
        if [ "$tree_rel" = "$TP_SYSROOT_REL" ]; then
            want="$(tp_stamp "$root")"
            maker="scripts/make_slopos_sysroot.sh"
        elif [ ! -d "$root/$TP_COMPILER_OVERLAY_REL" ]; then
            fail "$tree_rel is materialised with no $TP_COMPILER_OVERLAY_REL/ to have built it — delete it"
            continue
        else
            want="$(tp_rustc_stamp "$root")"
            maker="scripts/make_rustc_src.sh"
        fi
        stamp="$tree/$TP_STAMP_NAME"
        if [ ! -f "$stamp" ]; then
            fail "$tree_rel exists with no $TP_STAMP_NAME — re-run $maker"
        elif [ "$(cat "$stamp")" != "$want" ]; then
            fail "$tree_rel is stale: stamp $(cat "$stamp") but its overlay hashes to $want
       Re-run $maker — a stale tree compiles, it just compiles the previous fork."
        else
            note="$note, ${tree_rel#third_party/} at $want"
        fi

        # The stamp describes the *overlay*, not the result, so a tree built
        # by a broken run — patched std, unpatched libc — carries a correct
        # stamp. Every file the patches create is therefore checked for: it
        # is the cheapest evidence that each half actually landed.
        local created applied_in missing=""
        for rel in $(tp_patch_files "$root"); do
            [ "$(tp_patch_tree_rel "$rel")" = "$tree_rel" ] || continue
            applied_in="$root/$(tp_patch_apply_dir "$rel")"
            for created in $(tp_patch_new_files "$root/$rel"); do
                [ -e "$applied_in/$created" ] || missing="$missing
         $(tp_patch_apply_dir "$rel")/$created ($rel)"
            done
        done
        if [ -n "$missing" ]; then
            fail "$tree_rel is stamped current but the patches did not land:$missing
       Delete $tree_rel and re-run $maker."
        fi
    done

    # 4. A registration that points elsewhere builds someone else's std.
    local sysroot linked link_note=""
    sysroot="$root/$TP_SYSROOT_REL"
    linked="$(tp_link_target)"
    if [ -n "$linked" ]; then
        if [ "$linked" != "$(tp_abspath "$sysroot")" ]; then
            fail "the linked \`$TP_TOOLCHAIN_NAME\` toolchain points at $linked
       expected: $(tp_abspath "$sysroot")
       Re-run scripts/make_slopos_sysroot.sh, which re-links it."
        else
            link_note=", linked as +$TP_TOOLCHAIN_NAME"
        fi
    fi

    if [ "$bad" -ne 0 ]; then
        echo "$SELF: FAIL — a fork no longer matches its pin" >&2
        return 1
    fi

    echo "$SELF: OK — channel $pin_channel, libc $libc_version, $count patch(es)$note$link_note"
    return 0
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$SELF_TEST" -eq 1 ]; then
    SCRATCH="${BUILD_DIR:-$REPO_ROOT/builddir}/check_toolchain_pin-selftest"
    rm -rf "$SCRATCH"
    mkdir -p "$SCRATCH"
    trap 'rm -rf "$SCRATCH"' EXIT INT TERM
    echo "$SELF: self-test against planted fixtures under $SCRATCH"
    FAILED=0

    # A minimal good tree: a channel, an overlay with one patch on each side,
    # and a PIN that agrees with both.
    plant() {
        local root="$1"
        mkdir -p "$root/toolchain/rust" "$root/toolchain/libc" "$root/toolchain/compiler"
        cat > "$root/rust-toolchain.toml" <<'FIXTURE'
[toolchain]
channel = "nightly-2026-09-03"
FIXTURE
        # Shaped like the real patches: one edit plus one file *created* in a
        # new directory, which is what the materialisation check looks for.
        cat > "$root/toolchain/rust/0001-slopos-std.patch" <<'FIXTURE'
--- a/std/build.rs
+++ b/std/build.rs
--- /dev/null
+++ b/std/src/os/slopos/mod.rs
FIXTURE
        cat > "$root/toolchain/libc/0001-slopos-libc.patch" <<'FIXTURE'
--- a/src/unix/mod.rs
+++ b/src/unix/mod.rs
--- /dev/null
+++ b/src/unix/slopos/mod.rs
FIXTURE
        cat > "$root/toolchain/compiler/0001-slopos-target.patch" <<'FIXTURE'
--- a/compiler/rustc_target/src/spec/mod.rs
+++ b/compiler/rustc_target/src/spec/mod.rs
--- /dev/null
+++ b/compiler/rustc_target/src/spec/targets/x86_64_unknown_slopos.rs
FIXTURE
        cat > "$root/toolchain/PIN" <<FIXTURE
# fixture
channel=nightly-2026-09-03
libc_version=0.2.189
libc_checksum=$(printf '%064d' 0)
patch_sha256=toolchain/rust/0001-slopos-std.patch:$(tp_sha256_file "$root/toolchain/rust/0001-slopos-std.patch")
patch_sha256=toolchain/libc/0001-slopos-libc.patch:$(tp_sha256_file "$root/toolchain/libc/0001-slopos-libc.patch")
FIXTURE
        cat > "$root/toolchain/compiler/PIN" <<FIXTURE
# fixture
rustc_src_sha256=$(printf '%064d' 0)
patch_sha256=toolchain/compiler/0001-slopos-target.patch:$(tp_sha256_file "$root/toolchain/compiler/0001-slopos-target.patch")
FIXTURE
        # The llvm fork, pinned in a third PIN beside the tarball it patches.
        # It creates no file, so the materialisation check has nothing to look
        # for; what this covers is the routing to `toolchain/cxx/PIN`.
        mkdir -p "$root/toolchain/llvm" "$root/toolchain/cxx"
        cat > "$root/toolchain/llvm/0001-slopos-support.patch" <<'FIXTURE'
--- a/llvm/include/llvm/ADT/bit.h
+++ b/llvm/include/llvm/ADT/bit.h
FIXTURE
        cat > "$root/toolchain/cxx/PIN" <<FIXTURE
# fixture
llvm_version=18.1.8
patch_sha256=toolchain/llvm/0001-slopos-support.patch:$(tp_sha256_file "$root/toolchain/llvm/0001-slopos-support.patch")
FIXTURE
    }

    # Each case gets its own root and its own RUSTUP_HOME: the developer's real
    # `slopos` link must not decide whether a fixture passes.
    fixture() {
        local name="$1"
        local root="$SCRATCH/$name"
        rm -rf "$root"
        mkdir -p "$root"
        plant "$root"
        mkdir -p "$root/rustup/toolchains"
        printf '%s\n' "$root"
    }

    materialise() {
        local root="$1" lib="$1/$TP_SYSROOT_REL/$TP_LIBRARY_REL"
        local src="$1/$TP_RUSTC_SRC_REL"
        mkdir -p "$lib/std/src/os/slopos" "$lib/libc/src/unix/slopos"
        touch "$lib/std/src/os/slopos/mod.rs" "$lib/libc/src/unix/slopos/mod.rs"
        tp_stamp "$root" > "$root/$TP_SYSROOT_REL/$TP_STAMP_NAME"
        mkdir -p "$src/compiler/rustc_target/src/spec/targets"
        touch "$src/compiler/rustc_target/src/spec/targets/x86_64_unknown_slopos.rs"
        tp_rustc_stamp "$root" > "$src/$TP_STAMP_NAME"
    }

    run_case() {
        local name="$1" want_rc="$2" want_re="$3" note="$4" root="$SCRATCH/$1"
        local out rc=0
        if out="$(RUSTUP_HOME="$root/rustup" "$SCRIPT_DIR/$SELF.sh" --root "$root" 2>&1)"; then
            rc=0
        else
            rc=$?
        fi
        if [ "$rc" -ne "$want_rc" ]; then
            echo "$SELF --self-test: case $name exited $rc, expected $want_rc" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            FAILED=1
            return
        fi
        if [ -n "$want_re" ] && ! printf '%s\n' "$out" | grep -Eq "$want_re"; then
            echo "$SELF --self-test: case $name did not report /$want_re/" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            FAILED=1
            return
        fi
        echo "  case $name: $note"
    }

    # The good state, twice: bare, and with a correctly stamped and linked
    # sysroot. The second is what proves the stamp this gate recomputes is the
    # one make_slopos_sysroot.sh writes, and that a right link is accepted.
    root="$(fixture good)"
    run_case good 0 "^$SELF: OK" "silent on a pin that agrees with its patches"

    root="$(fixture good-materialised)"
    materialise "$root"
    ln -s "$(tp_abspath "$root/$TP_SYSROOT_REL")" "$root/rustup/toolchains/$TP_TOOLCHAIN_NAME"
    run_case good-materialised 0 "rust-slopos at .*slopos-rustc-src at .*linked as" \
        "silent on two fresh trees stamped and linked where they belong"

    # The llvm fork's patch is pinned in `toolchain/cxx/PIN` rather than in
    # `toolchain/PIN`, so an edit to it must be caught there and nowhere else.
    root="$(fixture llvm-patch-edit)"
    printf 'edited\n' >>"$root/toolchain/llvm/0001-slopos-support.patch"
    run_case llvm-patch-edit 1 "toolchain/llvm/.*does not match its pin" \
        "rejects an llvm patch its own PIN no longer describes"

    root="$(fixture llvm-patch-unpinned)"
    sed -i.bak '/^patch_sha256=toolchain\/llvm\//d' "$root/toolchain/cxx/PIN"
    run_case llvm-patch-unpinned 1 "toolchain/llvm/.*has no .patch_sha256" \
        "rejects an llvm patch no PIN names"

    # The failure a stamp cannot see: one half of the fork missing from a tree
    # whose stamp is current, which is what a buggy materialiser produces.
    root="$(fixture half-applied)"
    materialise "$root"
    rm -f "$root/$TP_SYSROOT_REL/$TP_LIBRARY_REL/libc/src/unix/slopos/mod.rs"
    run_case half-applied 1 'stamped current but the patches did not land' \
        "rejects a sysroot whose libc half never got patched"

    # 1. The fork drifting from rust-toolchain.toml.
    root="$(fixture channel-drift)"
    sed -i.bak 's/^channel=.*/channel=nightly-2026-08-01/' "$root/toolchain/PIN"
    rm -f "$root/toolchain/PIN.bak"
    run_case channel-drift 1 'channel drift' "rejects a pin cut against another compiler"

    # 2. A patch edited without its pin.
    root="$(fixture patch-drift)"
    echo "+++ b/std/build.rs" >> "$root/toolchain/rust/0001-slopos-std.patch"
    run_case patch-drift 1 'toolchain/rust/0001-slopos-std\.patch does not match its pin' \
        "rejects an edited patch whose sha256 no longer matches"

    # 3. A materialised sysroot left behind by an older overlay.
    root="$(fixture stale-stamp)"
    mkdir -p "$root/$TP_SYSROOT_REL"
    printf '%s\n' "$(printf '%064d' 1)" > "$root/$TP_SYSROOT_REL/$TP_STAMP_NAME"
    run_case stale-stamp 1 'is stale' "rejects a sysroot whose stamp predates the overlay"

    root="$(fixture compiler-patch-drift)"
    echo "+++ b/compiler/rustc_target/src/spec/mod.rs" >> "$root/toolchain/compiler/0001-slopos-target.patch"
    run_case compiler-patch-drift 1 'toolchain/compiler/0001-slopos-target\.patch does not match its pin' \
        "rejects a compiler patch edited without its own PIN"

    root="$(fixture stale-rustc-src)"
    materialise "$root"
    printf '%s\n' "$(printf '%064d' 1)" > "$root/$TP_RUSTC_SRC_REL/$TP_STAMP_NAME"
    run_case stale-rustc-src 1 "$TP_RUSTC_SRC_REL is stale" \
        "rejects a source tree whose stamp predates the compiler fork"

    # The decoupling the second PIN buys: an edited std patch restamps the
    # sysroot and must leave the source tree alone, or every compiler-fork edit
    # would re-extract 656 MiB and every std edit would re-check a tree it did
    # not touch.
    root="$(fixture std-edit-spares-rustc-src)"
    materialise "$root"
    printf '\n# touched\n' >> "$root/toolchain/rust/0001-slopos-std.patch"
    sed -i.bak "s|^patch_sha256=toolchain/rust/.*|patch_sha256=toolchain/rust/0001-slopos-std.patch:$(tp_sha256_file "$root/toolchain/rust/0001-slopos-std.patch")|" "$root/toolchain/PIN"
    rm -f "$root/toolchain/PIN.bak"
    if out="$(RUSTUP_HOME="$root/rustup" "$SCRIPT_DIR/$SELF.sh" --root "$root" 2>&1)"; then
        echo "$SELF --self-test: case std-edit-spares-rustc-src accepted a stale sysroot" >&2
        FAILED=1
    elif ! printf '%s\n' "$out" | grep -q "$TP_SYSROOT_REL is stale"; then
        echo "$SELF --self-test: case std-edit-spares-rustc-src did not report the stale sysroot" >&2
        FAILED=1
    elif printf '%s\n' "$out" | grep -q "$TP_RUSTC_SRC_REL is stale"; then
        echo "$SELF --self-test: a std edit restamped the compiler source tree" >&2
        FAILED=1
    else
        echo "  case std-edit-spares-rustc-src: a std edit restamps the sysroot and spares the source tree"
    fi

    root="$(fixture rustc-src-without-overlay)"
    materialise "$root"
    rm -rf "$root/toolchain/compiler"
    run_case rustc-src-without-overlay 1 'to have built it' \
        "rejects a source tree whose overlay is no longer in the checkout"

    # A materialiser decides what lands in the tree it builds, so it is one of
    # the tree's stamped inputs. Each fixture gets a copy of both, and editing
    # one must stale exactly its own tree.
    for half in sysroot rustc-src; do
        case "$half" in
            sysroot) script=make_slopos_sysroot.sh; stale="$TP_SYSROOT_REL"; fresh="$TP_RUSTC_SRC_REL" ;;
            *)       script=make_rustc_src.sh;      stale="$TP_RUSTC_SRC_REL"; fresh="$TP_SYSROOT_REL" ;;
        esac
        root="$(fixture "materialiser-edit-$half")"
        mkdir -p "$root/scripts"
        cp "$SCRIPT_DIR/make_slopos_sysroot.sh" "$SCRIPT_DIR/make_rustc_src.sh" "$root/scripts/"
        materialise "$root"
        printf '\n# a different unpack rule\n' >> "$root/scripts/$script"
        out="$(RUSTUP_HOME="$root/rustup" "$SCRIPT_DIR/$SELF.sh" --root "$root" 2>&1 || true)"
        if ! printf '%s\n' "$out" | grep -q "$stale is stale"; then
            echo "$SELF --self-test: editing $script left $stale stamped current" >&2
            FAILED=1
        elif printf '%s\n' "$out" | grep -q "$fresh is stale"; then
            echo "$SELF --self-test: editing $script staled $fresh as well" >&2
            FAILED=1
        else
            echo "  case materialiser-edit-$half: an edited $script stales only $stale"
        fi
    done

    # 4. A `slopos` toolchain registered against some other directory.
    root="$(fixture wrong-link)"
    mkdir -p "$root/elsewhere"
    ln -s "$(tp_abspath "$root/elsewhere")" "$root/rustup/toolchains/$TP_TOOLCHAIN_NAME"
    run_case wrong-link 1 'points at .*elsewhere' "rejects a link pointing outside the owned sysroot"

    # A checkout with no overlay at all is a broken checkout, not a pass.
    root="$(fixture no-overlay)"
    rm -rf "$root/toolchain"
    run_case no-overlay 1 "missing $TP_PIN_REL" "rejects a tree with no fork overlay"

    rm -rf "$SCRATCH"
    trap - EXIT INT TERM
    if [ "$FAILED" -ne 0 ]; then
        echo "$SELF: SELF-TEST FAILED — the gate does not catch what it claims to" >&2
        exit 1
    fi
    echo "$SELF: self-test OK"
    exit 0
fi

run_gate "$ROOT"
