#!/usr/bin/env bash
# The host's half of a kernel build: the gates that grade the ELF
# scripts/build_kernel.sh left in <build_dir>. They need LLVM's binutils, which
# the guest does not have, so every justfile recipe that builds a kernel runs
# this after the driver; a guest-built ELF is graded here once exported.
#
# Usage: check_kernel_elf_gates.sh <build_dir> <variant>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="${1:?usage: check_kernel_elf_gates.sh <build_dir> <variant>}"
VARIANT="${2:?usage: check_kernel_elf_gates.sh <build_dir> <variant>}"
KERNEL_ELF="$BUILD_DIR/kernel-${VARIANT}.elf"
[ -f "$KERNEL_ELF" ] || { echo "check_kernel_elf_gates: no $KERNEL_ELF" >&2; exit 1; }
RUST_CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"

# Source-discipline gates (vendor pin, unsafe-outside-ostd, no-async, alloc
# dep, Drop-panic-free, TCB ratio) are NOT run here: they scan the whole tree
# (~14 s) and would tax every interactive boot. They run in `just
# check-framekernel` and CI, which is the canonical enforcement point. Set
# KERNEL_BUILD_GATES=1 to also run them from a build.
if [ "${KERNEL_BUILD_GATES:-0}" = "1" ]; then
    "$SCRIPT_DIR/check_vendor_pin.sh"
    "$SCRIPT_DIR/check_unsafe_outside_ostd.sh"
    "$SCRIPT_DIR/check_no_kernel_async.sh"
    "$SCRIPT_DIR/check_alloc_dep.sh"
    "$SCRIPT_DIR/check_drop_panic_free.sh"
    "$SCRIPT_DIR/tcb_ratio.sh" --max 1.0
fi

# Single-writer gate for the kernel master PML4. A source scan, but a few
# greps rather than a tree walk, and the regression it catches is silent:
# a second raw writer over the master compiles, boots, and only loses a
# leaf when two CPUs happen to line up.
"$SCRIPT_DIR/check_kernel_pml4_writer.sh"

# All four ELF gates run for every variant, including tests — that is the
# image the whole suite executes on. The stamp key covers the gate scripts and
# their allowlists as well as the ELF, so an edited gate re-runs instead of
# waiting for the next unrelated kernel change.
GATE_STAMP="$BUILD_DIR/.kernel-elf-gates-${VARIANT}.stamp"
GATE_INPUTS=(
    "$KERNEL_ELF"
    "$SCRIPT_DIR/check_stack_sizes.sh"
    "$SCRIPT_DIR/check_kernel_softfloat.sh"
    "$SCRIPT_DIR/check_registry_sections.sh"
    "$SCRIPT_DIR/check_bootstrap_stack_rewind.sh"
    "$SCRIPT_DIR/llvm_tool.sh"
    "$SCRIPT_DIR/gates/stack/${VARIANT}.txt"
    "$SCRIPT_DIR/gates/vector/${VARIANT}.txt"
)

# GNU coreutils on Linux, BSD `shasum` on macOS. An empty digest degrades to
# always-run, never always-skip. The env line covers the two settings that
# change a verdict without changing a tracked file.
gate_input_digest() {
    local hash
    if command -v sha256sum >/dev/null 2>&1; then
        hash="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        hash="shasum -a 256"
    else
        return 0
    fi
    {
        $hash "$@" 2>/dev/null
        printf 'variant=%s threshold=%s channel=%s\n' \
            "$VARIANT" "${STACK_SIZE_THRESHOLD:-2048}" "$RUST_CHANNEL"
    } | $hash 2>/dev/null | awk '{print $1}'
}

GATE_KEY="$(gate_input_digest "${GATE_INPUTS[@]}")"
if [ -n "$GATE_KEY" ] && [ "$(cat "$GATE_STAMP" 2>/dev/null)" = "$GATE_KEY" ]; then
    echo "check_stack_sizes: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_kernel_softfloat: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_registry_sections: skipped (${VARIANT} kernel + gates unchanged since last pass)"
    echo "check_bootstrap_stack_rewind: skipped (${VARIANT} kernel + gates unchanged since last pass)"
else
    "$SCRIPT_DIR/check_stack_sizes.sh" --variant "$VARIANT" "$KERNEL_ELF"
    "$SCRIPT_DIR/check_kernel_softfloat.sh" --variant "$VARIANT" "$KERNEL_ELF"
    "$SCRIPT_DIR/check_registry_sections.sh" "$KERNEL_ELF"
    # Release-only in effect, but run for every variant: in dev/tests the
    # rewinding function is itself instrumented and its epilogue restores the
    # slot, so the bug is invisible at runtime there. The ELF still shows the
    # store, which is the whole reason this is a gate and not a kernel test.
    "$SCRIPT_DIR/check_bootstrap_stack_rewind.sh" --variant "$VARIANT" "$KERNEL_ELF"
    if [ -n "$GATE_KEY" ]; then
        printf '%s\n' "$GATE_KEY" >"$GATE_STAMP"
    fi
fi
