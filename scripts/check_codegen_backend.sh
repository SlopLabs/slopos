#!/usr/bin/env bash
# Hold a rustc codegen backend to the capabilities targets/x86_64-slos.json
# depends on, and fail when reality and scripts/gates/codegen/<backend>.txt
# disagree in either direction.
#
# Why a gate and not a paragraph: two of the seven capabilities below are ones
# a backend can *accept the flag for* and then not implement. rustc takes
# `-Zsanitizer=safestack` from any backend whose target spec lists the
# sanitizer as supported, and `-Zemit-stack-sizes` from any backend at all;
# a backend that ignores either leaves the build green, the ELF plausible,
# and S-5 (bounded kernel stack use) and the dual-stack split enforced by
# nothing. `check_stack_sizes.sh` catches the second through `min-records`.
# Nothing else catches the first.
#
# The direction that matters most is the *other* one. A `lacks` line here is
# a standing question — "can this tree be built without LLVM yet?" — and the
# gate is what re-asks it on every toolchain bump instead of leaving the
# answer to rot in a plan document. A capability the file records as absent
# and the probe finds present fails the run, which is the signal to go and
# re-decide.
#
# A backend whose codegen-backends directory holds no shared object for it is
# reported as `skipped`, not as failing: `cranelift` is an opt-in rustup
# component, and a CI job that never installs it still has a meaningful run.
#
# Two target-spec properties are deliberately not probed, and the residual is
# stated rather than left implied. `disable-redzone` needs an optimised build
# plus a disassembly heuristic to tell a red-zone spill from an ordinary one,
# because at this opt level every frame adjusts `%rsp` anyway. And
# `panic-strategy: unwind` cannot be expressed here at all: a standalone
# `no_std` probe with its own `#[panic_handler]` is refused with "unwinding
# panics are not supported without std", which is why the crate below pins
# `panic = "abort"`. `link.ld`'s own ASSERT on `.eh_frame_hdr` holds the
# second at every kernel link, which is stronger than a probe would be.
#
# Usage:
#     scripts/check_codegen_backend.sh --backend llvm
#     scripts/check_codegen_backend.sh --backend cranelift
#     scripts/check_codegen_backend.sh --backend cranelift --emit-allowlist
#     scripts/check_codegen_backend.sh --self-test
#
# --require turns a "skipped" into a failure, for the CI job whose only reason
# to exist is re-asking the question: an uninstalled candidate there is a
# question that stopped being asked, not a clean run.
#
# --gate-data-dir points the gate at another expectations directory, for
# reading a fresh measurement before promoting it over the tracked one.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BACKEND=""
EMIT_ALLOWLIST=0
SELF_TEST=0
REQUIRE=0
GATE_DATA_DIR="$SCRIPT_DIR/gates/codegen"
RUST_TARGET="$REPO_ROOT/targets/x86_64-slos.json"

while [ $# -gt 0 ]; do
    case "$1" in
        --backend)        BACKEND="${2:?--backend needs a value}"; shift 2 ;;
        --emit-allowlist) EMIT_ALLOWLIST=1; shift ;;
        --self-test)      SELF_TEST=1; shift ;;
        --require)        REQUIRE=1; shift ;;
        --gate-data-dir)  GATE_DATA_DIR="${2:?--gate-data-dir needs a value}"; shift 2 ;;
        *) echo "check_codegen_backend: unknown option $1" >&2; exit 2 ;;
    esac
done

RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
OBJDUMP=""
READOBJ=""

# The array probe_frame holds, and the floor its recorded frame is held to.
# What puts it on the unsafe stack is `black_box(&cells)`, not the size.
PROBE_FRAME_WORDS=64

# ---------------------------------------------------------------------------
# The probe crate
# ---------------------------------------------------------------------------
#
# Written out rather than tracked as a workspace member: a crate under
# scripts/ would either join the workspace and be built for the host on every
# `cargo build`, or sit outside it and drift from the assertions that read it.

write_probe_crate() {
    local dir="$1"
    mkdir -p "$dir/src"
    cat > "$dir/Cargo.toml" <<EOF
[workspace]

[package]
name = "slopos-codegen-probe"
version = "0.0.0"
edition = "2024"

[lib]
crate-type = ["staticlib"]

[features]
asm-sym = []

[profile.dev]
panic = "abort"
EOF
    cat > "$dir/src/lib.rs" <<EOF
#![no_std]

#[panic_handler]
fn probe_panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn probe_float_add(a: f64, b: f64) -> f64 {
    a + b
}

#[unsafe(link_section = ".probe_registry")]
#[used]
static PROBE_REGISTRY_ENTRY: u64 = 0x5107_0501_5EC_0001;

#[unsafe(naked)]
#[unsafe(no_mangle)]
pub extern "C" fn probe_naked() -> ! {
    core::arch::naked_asm!("cli", "2:", "hlt", "jmp 2b");
}

#[unsafe(no_mangle)]
pub extern "C" fn probe_frame(seed: u64) -> u64 {
    let mut cells = [0u64; $PROBE_FRAME_WORDS];
    let mut i = 0;
    while i < cells.len() {
        cells[i] = seed.wrapping_add(i as u64);
        i += 1;
    }
    let mut sum = 0u64;
    for cell in core::hint::black_box(&cells) {
        sum = sum.wrapping_add(*cell);
    }
    sum
}

#[cfg(feature = "asm-sym")]
#[unsafe(no_mangle)]
pub extern "C" fn probe_asm_sym() -> usize {
    let address: usize;
    unsafe {
        core::arch::asm!(
            "lea {out}, [rip + {target}]",
            out = out(reg) address,
            target = sym probe_frame,
            options(nostack, nomem),
        );
    }
    address
}
EOF
}

# ---------------------------------------------------------------------------
# Building a probe
# ---------------------------------------------------------------------------
#
# One `-Zbuild-std` fingerprint per RUSTFLAGS set, cached under builddir/, so
# a re-run costs a cargo freshness check rather than a rebuild of core.
#
# Setting RUSTFLAGS overrides `.cargo/config.toml`'s `[target.…] rustflags`
# wholesale, which is the point: the soft-float probe then measures the
# guarantee `targets/x86_64-slos.json` carries on its own, not the belt-and-
# braces `-C target-feature=-sse,…` beside it.

BACKEND_FLAG=""
PROBE_DIR=""
PROBE_LOG=""
PROBE_ARCHIVE=""
BASE_ARCHIVE=""

# Updates PROBE_LOG and PROBE_ARCHIVE for one flag set, succeeding only when
# Cargo both exits cleanly and leaves the expected static archive.
build_probe() {
    local tag="$1" features="$2" extra_flags="$3"
    local target_dir="$PROBE_DIR/target-$tag"
    PROBE_LOG="$PROBE_DIR/$tag.log"

    # `${a[@]+"${a[@]}"}` below rather than `"${a[@]}"`: bash 3.2 under `set -u`
    # reads an empty array expansion as an unbound variable.
    local feature_args=()
    if [ -n "$features" ]; then
        feature_args=(--features "$features")
    fi

    if CARGO_TARGET_DIR="$target_dir" \
       RUSTFLAGS="$BACKEND_FLAG -Zunstable-options -Zemit-stack-sizes $extra_flags" \
       cargo +"$RUST_CHANNEL" build \
           -Zbuild-std=core \
           -Zbuild-std-features=compiler-builtins-mem \
           -Zunstable-options \
           -Zjson-target-spec \
           --target "$RUST_TARGET" \
           --manifest-path "$PROBE_DIR/Cargo.toml" \
           ${feature_args[@]+"${feature_args[@]}"} \
           > "$PROBE_LOG" 2>&1
    then
        PROBE_ARCHIVE="$(find "$target_dir" -name 'libslopos_codegen_probe.a' | sed -n '1p')"
        if [ -n "$PROBE_ARCHIVE" ]; then
            return 0
        fi
    fi
    PROBE_ARCHIVE=""
    return 1
}

# An allowlist of compiler refusals, not a denylist of infrastructure noise:
# `unknown` fails the run, while a `lacks` the tracked file already records
# would turn a full disk into a green answer.
record_build_failure() {
    local capability="$1" reason
    reason="$(probe_failure_reason)"
    case "$reason" in
        *"not yet supported"*|*"not supported"*|*"unsupported"*|*"Unknown option"*|*"unknown option"*)
            record "$capability" lacks "$reason"
            ;;
        *)
            record "$capability" unknown \
                "the build failed without a recognisable refusal${reason:+: $reason}"
            ;;
    esac
}

# Cargo wraps the compiler's own message twice over — once as "failed to run
# rustc to learn about target-specific information", once as "could not
# compile" — and the backend's refusal is the indented line between them.
probe_failure_reason() {
    local reasons
    reasons="$(sed -E -n 's/^[[:space:]]*error(\[[A-Za-z0-9]*\])?: //p' "$PROBE_LOG")"
    printf '%s\n' "$reasons" \
        | awk '!/^(failed to run|could not compile|aborting due to)/ && !found { print; found = 1 }'
}

# ---------------------------------------------------------------------------
# The capability probes
# ---------------------------------------------------------------------------
#
# Each appends one `<capability> <TAB> has|lacks|unknown <TAB> <detail>` line.
# `unknown` matches no recorded verdict, so a probe whose build broke for an
# unrelated reason fails the run instead of reading as a measured absence.

FINDINGS=""

record() {
    FINDINGS="${FINDINGS}$(printf '%s\t%s\t%s' "$1" "$2" "$3")
"
}

# The same four classes check_kernel_softfloat.sh scans the kernel ELF for:
# XMM/YMM/ZMM, x87, MMX and the xsave pair all live under XCR0, and the
# kernel saves none of them on a syscall or fault entry.
XCR0_CLASSIFIER='
    $1 ~ /^[ ]*[0-9a-f]+: *$/ && NF >= 2 {
        mnem = $2;
        ops = (NF >= 3 ? $3 : "");
        sub(/ *#.*$/, "", ops);
        sub(/ *<[^>]*>$/, "", ops);
        if (ops ~ /%[xyz]mm[0-9]/ || mnem ~ /^v?(ld|st)mxcsr$/ || mnem ~ /^vzero(upper|all)$/) print mnem;
        else if (mnem ~ /^f[a-z0-9]*$/ || ops ~ /%st(\(|,|$)/) print mnem;
        else if (ops ~ /%mm[0-7]/ || mnem ~ /^f?emms$/) print mnem;
        else if (mnem ~ /^[xf](save|rstor)/) print mnem;
    }'

# Reports soft-float support only when the addition remains present without
# touching any XCR0-managed register class.
probe_soft_float() {
    local hits libcall
    hits="$("$OBJDUMP" -d --no-show-raw-insn --disassemble-symbols=probe_float_add \
        "$BASE_ARCHIVE" 2>/dev/null | awk -F'\t' "$XCR0_CLASSIFIER" | sort -u | tr '\n' ' ' || true)"
    if [ -n "$hits" ]; then
        record soft-float lacks "probe_float_add uses ${hits% }"
        return
    fi
    # The absence of a vector instruction is also what an empty body looks
    # like; the libcall is what says the addition still happens.
    libcall="$("$OBJDUMP" -dr --no-show-raw-insn --disassemble-symbols=probe_float_add \
        "$BASE_ARCHIVE" 2>/dev/null | grep -c '__adddf3' || true)"
    if [ "$libcall" -gt 0 ]; then
        record soft-float has "probe_float_add calls __adddf3"
    else
        record soft-float lacks "probe_float_add neither uses XMM nor calls __adddf3"
    fi
}

# The floor under the probes that read an absence: a tool that answers nothing
# looks exactly like a backend that emitted nothing.
require_probe_inputs() {
    local missing="" symbol seen sections
    if [ -z "$PROBE_ARCHIVE" ]; then
        echo "check_codegen_backend: no probe archive to read." >&2
        exit 2
    fi
    for symbol in probe_float_add probe_frame probe_naked; do
        seen="$("$OBJDUMP" -d --no-show-raw-insn --disassemble-symbols="$symbol" \
            "$PROBE_ARCHIVE" 2>/dev/null | grep -c "<$symbol>:" || true)"
        if [ "$seen" -eq 0 ]; then
            missing="$missing $symbol"
        fi
    done
    sections="$("$READOBJ" --sections "$PROBE_ARCHIVE" 2>/dev/null | grep -c 'Name: [.]text' || true)"
    if [ "$sections" -eq 0 ]; then
        missing="$missing .text(readobj)"
    fi
    if [ -n "$missing" ]; then
        echo "check_codegen_backend: the probe archive reads as carrying no${missing}." >&2
        echo "  Every verdict below reads an absence, so the run would be meaningless." >&2
        exit 2
    fi
}

# Field-exact, as in check_stack_sizes.sh: a bare /Size:/ also matches the
# `AddressSize:` line llvm-readobj prints for every archive member.
stack_size_of_probe_frame() {
    awk '/Functions: \[probe_frame\]/ { want = 1; next }
         want && $1 == "Size:" && !found { print $2; found = 1 }' "$1"
}

# Holds the emitted frame record above the probe array's known minimum size,
# so an accepted flag that emits no usable data cannot pass.
probe_stack_sizes() {
    local size
    "$READOBJ" --stack-sizes "$BASE_ARCHIVE" > "$PROBE_DIR/stack-sizes.txt" 2>/dev/null || true
    size="$(stack_size_of_probe_frame "$PROBE_DIR/stack-sizes.txt")"
    if [ -z "$size" ]; then
        record stack-sizes lacks "-Zemit-stack-sizes recorded no frame for probe_frame"
    elif [ "$(( size ))" -lt "$(( PROBE_FRAME_WORDS * 8 ))" ]; then
        record stack-sizes lacks "probe_frame recorded as $size, under its array"
    else
        record stack-sizes has "probe_frame's frame recorded as $size"
    fi
}

# Confirms that the backend preserves a requested registry section in the
# archive rather than merely accepting the source attribute.
probe_link_section() {
    local hits
    hits="$("$READOBJ" --sections "$BASE_ARCHIVE" 2>/dev/null | grep -c 'Name: [.]probe_registry' || true)"
    if [ "$hits" -gt 0 ]; then
        record link-section has "#[unsafe(link_section)] reaches the object"
    else
        record link-section lacks "no .probe_registry section in the object"
    fi
}

# Requires the naked function's first instructions to match its literal body;
# an accepted attribute with an empty or transformed body is a failed probe.
probe_naked_fn() {
    local body
    body="$("$OBJDUMP" -d --no-show-raw-insn --disassemble-symbols=probe_naked \
        "$BASE_ARCHIVE" 2>/dev/null | awk -F'\t' '$1 ~ /^[ ]*[0-9a-f]+: *$/ { printf "%s ", $2 }')"
    case "$body" in
        "cli hlt jmp "*) record naked-fn has "body emitted verbatim" ;;
        "")              record naked-fn lacks "probe_naked has no body" ;;
        *)               record naked-fn lacks "probe_naked body is '${body% }'" ;;
    esac
}

# The sanitizer first and its pointer-address option second, because a backend
# that implements neither fails on the *option* — a loud error that hides the
# silent one this probe exists for: a sanitizer flag accepted and ignored.
probe_safestack() {
    local hits
    if ! build_probe safestack-only "" "-Zsanitizer=safestack"; then
        record_build_failure safestack
        return
    fi
    require_probe_inputs
    hits="$("$OBJDUMP" -dr --no-show-raw-insn --disassemble-symbols=probe_frame \
        "$PROBE_ARCHIVE" 2>/dev/null | grep -c '__safestack_' || true)"
    if [ "$hits" -eq 0 ]; then
        record safestack lacks "-Zsanitizer=safestack accepted, probe_frame not instrumented"
        return
    fi
    if ! build_probe safestack "" "-Zsanitizer=safestack -Cllvm-args=-safestack-use-pointer-address"; then
        record_build_failure safestack
        return
    fi
    require_probe_inputs
    hits="$("$OBJDUMP" -dr --no-show-raw-insn --disassemble-symbols=probe_frame \
        "$PROBE_ARCHIVE" 2>/dev/null | grep -c '__safestack_pointer_address' || true)"
    if [ "$hits" -gt 0 ]; then
        record safestack has "probe_frame reads the unsafe stack pointer"
    else
        record safestack lacks "instrumented, but not through __safestack_pointer_address"
    fi
}

# Uses the relocation to probe_frame as evidence that an asm `sym` operand was
# resolved, rather than treating a successful feature build as sufficient.
probe_asm_sym() {
    if ! build_probe asmsym asm-sym ""; then
        record_build_failure asm-sym
        return
    fi
    require_probe_inputs
    local hits
    hits="$("$OBJDUMP" -dr --no-show-raw-insn --disassemble-symbols=probe_asm_sym \
        "$PROBE_ARCHIVE" 2>/dev/null | grep -c 'probe_frame' || true)"
    if [ "$hits" -gt 0 ]; then
        record asm-sym has "the sym operand resolves to probe_frame"
    else
        record asm-sym lacks "probe_asm_sym compiled without a reference to probe_frame"
    fi
}

# Runs every capability from one validated base archive; a failed base build
# makes each dependent verdict unknown instead of falsely unsupported.
run_probes() {
    if ! build_probe base "" ""; then
        record object-format unknown "$(probe_failure_reason)"
        for capability in soft-float stack-sizes link-section naked-fn safestack asm-sym; do
            record "$capability" unknown "not reached: the base probe did not build"
        done
        return
    fi
    record object-format has "the backend emits an object for this target"
    BASE_ARCHIVE="$PROBE_ARCHIVE"
    require_probe_inputs
    probe_soft_float
    probe_stack_sizes
    probe_link_section
    probe_naked_fn
    probe_safestack
    probe_asm_sym
}

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

# Locates a backend only within the pinned toolchain's host sysroot, avoiding
# an unrelated shared object from PATH or another installed toolchain.
backend_shared_object() {
    local sysroot host
    sysroot="$(rustc +"$RUST_CHANNEL" --print sysroot)"
    host="$(rustc +"$RUST_CHANNEL" -vV | sed -n 's/^host: //p')"
    find "$sysroot/lib/rustlib/$host/codegen-backends" \
        -name "librustc_codegen_$1-*.so" 2>/dev/null | sed -n '1p'
}

emit_allowlist() {
    # An `unknown` recorded as an expectation would make every later run
    # pass on a measurement that never happened.
    if printf '%s' "$FINDINGS" | awk -F'\t' '$2 == "unknown" { found = 1 } END { exit !found }'; then
        echo "check_codegen_backend: refusing to emit — some verdicts are unknown:" >&2
        printf '%s' "$FINDINGS" | awk -F'\t' '$2 == "unknown" { printf "      %s: %s\n", $1, $3 }' >&2
        exit 2
    fi
    printf '# check_codegen_backend expectations — backend: %s\n#\n' "$BACKEND"
    printf '# Measured by: scripts/check_codegen_backend.sh --backend %s --emit-allowlist\n' "$BACKEND"
    printf '# against %s on %s.\n#\n' "$(basename "$RUST_TARGET")" "$RUST_CHANNEL"
    printf '# <capability> <TAB> has|lacks\n#\n'
    printf '# A `lacks` the probe finds present fails the run just as a `has` that\n'
    printf '# regressed does: this file is what makes "not yet" a measurement.\n#\n'
    printf '# Re-emitting prints the rows, not the reasons — the tracked file says\n'
    printf '# what each verdict costs, and that prose is written by hand.\n\n'
    printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 2 { printf "%s\t%s\n", $1, $2 }'
}

# An empty side would otherwise reach diff as one blank line and come back as
# a finding with nothing in it.
verdict_lines() {
    printf '%s\n' "$1" | sed '/^$/d'
}

compare_against_allowlist() {
    local allowlist="$GATE_DATA_DIR/$BACKEND.txt"
    if [ ! -f "$allowlist" ]; then
        echo "check_codegen_backend: no expectations for backend '$BACKEND' at $allowlist" >&2
        echo "  Record them with --emit-allowlist." >&2
        exit 2
    fi

    # A space where a tab belongs reads as a set difference on every row, which
    # points at the measurement instead of at the typo.
    local malformed
    # awk, not grep: an ERE has no \t, so a grep pattern would read every row
    # as malformed.
    malformed="$(awk -F'\t' '
        /^[[:space:]]*(#|$)/ { next }
        NF == 2 && $1 ~ /^[a-z0-9-]+$/ && ($2 == "has" || $2 == "lacks") { next }
        { bad = bad "      " FNR ": " $0 "\n"; n++ }
        END { printf "%d\n%s", n + 0, bad }' "$allowlist")"
    if [ "$(printf '%s' "$malformed" | sed -n '1p')" != "0" ]; then
        echo "check_codegen_backend: $allowlist has row(s) that are not '<name><TAB>has|lacks'" >&2
        printf '%s' "$malformed" | sed -n '2,$p' >&2
        return 1
    fi

    local expected observed
    expected="$(awk -F'\t' '!/^[[:space:]]*(#|$)/ && NF >= 2 { printf "%s\t%s\n", $1, $2 }' \
        "$allowlist" | LC_ALL=C sort)"
    observed="$(printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 2 { printf "%s\t%s\n", $1, $2 }' | LC_ALL=C sort)"

    if [ "$expected" = "$observed" ]; then
        local has_count lacks_count
        has_count="$(printf '%s\n' "$observed" | grep -c '	has$' || true)"
        lacks_count="$(printf '%s\n' "$observed" | grep -c '	lacks$' || true)"
        printf 'check_codegen_backend: OK — backend=%s, %d capability(ies) present, %d absent\n' \
            "$BACKEND" "$has_count" "$lacks_count"
        printf '  measured with: %s\n' "$(rustc +"$RUST_CHANNEL" --version)"
        printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 3 { printf "  %-14s %-6s %s\n", $1, $2, $3 }'
        return 0
    fi

    echo "check_codegen_backend: FAIL — backend=$BACKEND disagrees with $allowlist" >&2
    # `diff` exits 1 exactly when this branch is reached, and pipefail would
    # make that the pipeline's status and end the run before the remedy line.
    { diff <(verdict_lines "$expected") <(verdict_lines "$observed") || true; } \
        | sed -n 's/^< /  recorded but not observed: /p; s/^> /  observed but not recorded: /p' >&2
    printf '%s' "$FINDINGS" | awk -F'\t' 'NF >= 3 { printf "  %-14s %-6s %s\n", $1, $2, $3 }' >&2
    echo "  Re-measure with --emit-allowlist, restore the per-row prose it does not" >&2
    echo "  print, and say in the commit message what moved." >&2
    return 1
}

main() {
    if [ -z "$BACKEND" ]; then
        echo "check_codegen_backend: --backend is required" >&2
        exit 2
    fi

    case "$BACKEND" in
        llvm) BACKEND_FLAG="" ;;
        cranelift)
            if [ -z "$(backend_shared_object "$BACKEND")" ]; then
                echo "check_codegen_backend: skipped — no codegen backend '$BACKEND' in the $RUST_CHANNEL sysroot" >&2
                echo "  Install it with: rustup component add rustc-codegen-${BACKEND}-preview --toolchain $RUST_CHANNEL" >&2
                # A shell redirection has already emptied the tracked file by
                # the time this runs; exit 2 says so rather than leaving the
                # empty file looking like a measurement. Restore it from git.
                if [ "$EMIT_ALLOWLIST" = "1" ] || [ "$REQUIRE" = "1" ]; then
                    exit 2
                fi
                exit 0
            fi
            BACKEND_FLAG="-Zcodegen-backend=$BACKEND"
            ;;
        *)
            echo "check_codegen_backend: unknown backend '$BACKEND' (known: llvm, cranelift)" >&2
            echo "  An unknown name must not read as one that is merely not installed." >&2
            exit 2
            ;;
    esac

    OBJDUMP="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objdump)"
    READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"

    PROBE_DIR="$REPO_ROOT/builddir/gates/codegen-probe/$BACKEND"
    write_probe_crate "$PROBE_DIR"
    run_probes

    if [ "$EMIT_ALLOWLIST" = "1" ]; then
        emit_allowlist
        exit 0
    fi
    compare_against_allowlist
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
#
# Three layers, none of which needs a real backend: the verdict comparison
# over crafted findings, the build-failure classifier, and the parsers against
# captured tool output. A check that has never been observed to reject has not
# been observed to work.

self_test() {
    local root fail=0
    root="$(mktemp -d)"
    trap 'rm -rf "$root"' EXIT INT TERM
    echo "check_codegen_backend: self-test against built-in fixtures"

    GATE_DATA_DIR="$root"
    BACKEND="fixture"
    printf 'object-format\thas\nsoft-float\tlacks\nstack-sizes\thas\n' > "$root/fixture.txt"

    # `set +e` around the subshell rather than `|| got=$?`: the errexit
    # suppression an `&&`/`||` list applies reaches inside it, and `set -e`
    # there does not undo it — so the failure path would not be under errexit.
    expect() {
        local name="$1" want="$2" got
        set +e
        ( set -e; compare_against_allowlist ) > "$root/out" 2>&1
        got=$?
        set -e
        if [ "$got" -ne "$want" ]; then
            echo "check_codegen_backend --self-test: $name expected exit $want, got $got" >&2
            sed 's/^/      /' "$root/out" >&2
            fail=1
            return
        fi
        # Only the disagreement path (1) names that remedy; the operator-error
        # path (2) tells you to record a file instead.
        if [ "$want" -eq 1 ] && ! grep -q 'Re-measure with --emit-allowlist' "$root/out"; then
            echo "check_codegen_backend --self-test: $name rejected without reaching the remedy line" >&2
            sed 's/^/      /' "$root/out" >&2
            fail=1
            return
        fi
        echo "  $name: exits $got as expected"
    }

    FINDINGS="$(printf 'object-format\thas\td\nsoft-float\tlacks\td\nstack-sizes\thas\td\n')
"
    expect "an exact match passes" 0

    FINDINGS="$(printf 'object-format\thas\td\nsoft-float\thas\td\nstack-sizes\thas\td\n')
"
    expect "a capability gained since the last measurement fails" 1

    FINDINGS="$(printf 'object-format\thas\td\nsoft-float\tlacks\td\nstack-sizes\tlacks\td\n')
"
    expect "a capability lost since the last measurement fails" 1

    FINDINGS="$(printf 'object-format\thas\td\nsoft-float\tlacks\td\n')
"
    expect "a capability the probe stopped reporting fails" 1

    FINDINGS="$(printf 'object-format\thas\td\nsoft-float\tlacks\td\nstack-sizes\thas\td\nasm-sym\thas\td\n')
"
    expect "a capability the file does not record fails" 1

    FINDINGS="$(printf 'object-format\tunknown\td\nsoft-float\tunknown\td\nstack-sizes\tunknown\td\n')
"
    expect "a probe build that broke fails rather than reading as absence" 1

    # The remedy the failure message names has to work, or it is not a remedy.
    FINDINGS="$(printf 'alpha\thas\td\nbeta\tlacks\td\ngamma\thas\td\n')
"
    emit_allowlist > "$root/fixture.txt"
    expect "a re-emitted allowlist compares equal to what produced it" 0

    rm -f "$root/fixture.txt"
    expect "a missing expectations file is an operator error, not a finding" 2

    # The parsers, against captured cargo and llvm output. Everything above
    # tests the comparison; a parser that silently stops answering turns every
    # verdict into `lacks` and only the all-`has` tracked file would notice.
    expect_parse() {
        local name="$1" want="$2" got="$3"
        if [ "$got" != "$want" ]; then
            echo "check_codegen_backend --self-test: $name gave [$got], wanted [$want]" >&2
            fail=1
            return
        fi
        echo "  parser $name: [$got]"
    }

    PROBE_LOG="$root/probe.log"
    cat > "$PROBE_LOG" <<'EOF'
error: failed to run `rustc` to learn about target-specific information

Caused by:
  process didn't exit successfully: `rustc - --crate-name ___` (exit status: 1)
  --- stderr
  error: Unknown option `-safestack-use-pointer-address`
error: could not compile `slopos-codegen-probe` (lib)
EOF
    expect_parse "probe_failure_reason" \
        'Unknown option `-safestack-use-pointer-address`' "$(probe_failure_reason)"
    printf 'error[E0432]: unresolved import\n' > "$PROBE_LOG"
    expect_parse "probe_failure_reason coded" "unresolved import" "$(probe_failure_reason)"
    : > "$PROBE_LOG"
    expect_parse "probe_failure_reason silent" "" "$(probe_failure_reason)"

    # The classifier, in both directions, and through the assignment that once
    # ended the run under pipefail when every line was a cargo wrapper.
    expect_class() {
        local name="$1" want="$2" got
        FINDINGS=""
        record_build_failure probe
        got="$(printf '%s' "$FINDINGS" | awk -F'\t' 'NR == 1 { print $2 }')"
        if [ "$got" != "$want" ]; then
            echo "check_codegen_backend --self-test: $name classified $got, wanted $want" >&2
            fail=1
            return
        fi
        echo "  classify $name: $got"
    }
    printf 'error: Unknown option `-x`\n' > "$PROBE_LOG"
    expect_class "a backend refusal" lacks
    printf 'error: asm! sym operands are not yet supported\n' > "$PROBE_LOG"
    expect_class "an unimplemented feature" lacks
    printf 'error: could not compile `x`\nerror: aborting due to 1 previous error\n' > "$PROBE_LOG"
    expect_class "only cargo wrappers" unknown
    printf 'error: failed to write `/x`: No space left on device\n' > "$PROBE_LOG"
    expect_class "a full disk" unknown
    : > "$PROBE_LOG"
    expect_class "nothing at all" unknown

    cat > "$root/disasm.txt" <<'EOF'
0000000000000000 <probe_float_add>:
       0:	addsd	%xmm1, %xmm0
       4:	fninit
       6:	movq	%mm0, %mm1
       9:	xsave64	(%rsp)
       d:	movq	%rdi, 0x8(%rsp)
      12:	retq
EOF
    expect_parse "XCR0 classifier" "addsd fninit movq xsave64" \
        "$(awk -F'\t' "$XCR0_CLASSIFIER" "$root/disasm.txt" | sort -u | tr '\n' ' ' | sed 's/ $//')"
    printf '0000000000000000 <probe_float_add>:\n       0:\tcallq\t0x5\n       5:\tretq\n' \
        > "$root/disasm.txt"
    expect_parse "XCR0 classifier silent" "" \
        "$(awk -F'\t' "$XCR0_CLASSIFIER" "$root/disasm.txt" | tr '\n' ' ' | sed 's/ $//')"

    cat > "$root/sizes.txt" <<'EOF'
Format: elf64-x86-64
AddressSize: 64bit
StackSizes [
  Entry {
    Functions: [probe_float_add]
    Size: 0x18
  }
  Entry {
    Functions: [probe_frame]
    Size: 0x278
  }
]
EOF
    expect_parse "stack_size_of_probe_frame" "0x278" \
        "$(stack_size_of_probe_frame "$root/sizes.txt")"

    rm -rf "$root"
    trap - EXIT INT TERM
    if [ "$fail" -ne 0 ]; then
        echo "check_codegen_backend: SELF-TEST FAILED — the verdict comparison is wrong" >&2
        exit 1
    fi
    echo "check_codegen_backend: self-test OK"
    exit 0
}

[ "$SELF_TEST" = "1" ] && self_test
main
