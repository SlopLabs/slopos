#!/usr/bin/env bash
# Hold the kernel master's page tables to one writer.
#
# Every kernel-half page-table write goes through
# `slopos_ostd::mm::vm_space::CursorMut` under the `KERNEL_VM_SPACE` lock, and
# `slopos_mm::kernel_mappings` is the one module outside OSTD that takes that
# cursor. The cursor's exclusivity comes from the borrow checker over one
# `VmSpace` object; a second module driving it, or a second handle on the
# static, is a writer the review of that one module does not cover.
# `verification/STATUS.md`'s tier-2 argument for `vm_space` rests on this.
#
# Outside `slopos-ostd`, which owns the tables and is audited as the TCB:
#
#   static  `KERNEL_VM_SPACE` itself is named only where it is defined
#           (`kernel-services/src/kernel_vm_space.rs`) and installed once at
#           boot (`boot/src/boot_memory.rs`). Everything else reaches it
#           through the `kernel_vm_space()` accessors.
#   writer  A file that names the space (`kernel_vm_space`, `KERNEL_VM_SPACE`)
#           and takes a mutating cursor (`cursor_mut`, `CursorMut`) is
#           `mm/src/kernel_mappings.rs`. Reads (`cursor`, `pml4_paddr`) and
#           CR3 loads (`activate_*`) are allowed anywhere.
#   store   A file that reaches a page-table word through
#           `ptr_buf::with_atomic_u64_in_page` only loads it.
#   floor   `mm/src/kernel_mappings.rs` does both, so the check cannot pass on
#           a writer that moved or was renamed.
#
# Comments are stripped before matching. Runs on every kernel build from
# `check_kernel_elf_gates.sh` (a few greps) and from
# `just check-framekernel-gates`.
#
# Usage:
#     scripts/check_kernel_pml4_writer.sh
#     scripts/check_kernel_pml4_writer.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_kernel_pml4_writer "$@"

WRITER=mm/src/kernel_mappings.rs
STATIC_HOMES='kernel-services/src/kernel_vm_space.rs boot/src/boot_memory.rs'

code_of() {
    sed 's|//.*$||' "$1"
}

in_list() {
    local needle="$1" item
    shift
    for item in "$@"; do
        [ "$item" = "$needle" ] && return 0
    done
    return 1
}

run_scan() {
    local root="$1" files file code found_writer=0
    files="$(gate_collect_rs_files "$root" |
        grep -vE '^(slopos-ostd|slopos-ostd-derive|verification|vendor|third_party|builddir)/' || true)"
    gate_require_nonempty check_kernel_pml4_writer "$root" "$files"
    # shellcheck disable=SC2086
    set -- $STATIC_HOMES
    # Only files that name the space at all, comments included, are read again.
    while IFS= read -r file; do
        [ -n "$file" ] || continue
        code="$(code_of "$root/$file")"
        if printf '%s\n' "$code" | grep -qE '(^|[^A-Za-z0-9_])KERNEL_VM_SPACE([^A-Za-z0-9_]|$)' &&
            ! in_list "$file" "$@"; then
            printf 'static\t%s\n' "$file"
        fi
        if printf '%s\n' "$code" | grep -q 'with_atomic_u64_in_page' &&
            printf '%s\n' "$code" | grep -qE '\.(store|swap|compare_exchange(_weak)?|fetch_[a-z]+)\('; then
            printf 'store\t%s\n' "$file"
        fi
        printf '%s\n' "$code" | grep -qE '(^|[^A-Za-z0-9_])((try_)?kernel_vm_space|KERNEL_VM_SPACE)([^A-Za-z0-9_]|$)' ||
            continue
        printf '%s\n' "$code" | grep -qE '(^|[^A-Za-z0-9_])(cursor_mut|CursorMut)([^A-Za-z0-9_]|$)' ||
            continue
        if [ "$file" = "$WRITER" ]; then
            found_writer=1
        else
            printf 'writer\t%s\n' "$file"
        fi
    done < <(cd "$root" && printf '%s\n' "$files" | tr '\n' '\0' |
        xargs -0 grep -lE 'KERNEL_VM_SPACE|kernel_vm_space|with_atomic_u64_in_page' 2>/dev/null || true)
    if [ "$found_writer" -eq 0 ]; then
        printf 'floor\t%s takes no kernel_vm_space() cursor_mut\n' "$WRITER"
    fi
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_kernel_pml4_writer

    cat >"$(gate_fixture "$WRITER")" <<'RS'
use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
pub fn map_kernel_page() {
    let mut guard = kernel_vm_space().lock();
    let mut cursor = guard.cursor_mut(range).unwrap();
}
RS
    cat >"$(gate_fixture kernel-services/src/kernel_vm_space.rs)" <<'RS'
pub static KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>> = OnceLock::new();
pub fn kernel_vm_space() -> &'static SpinLock<VmSpace> { KERNEL_VM_SPACE.get().unwrap() }
RS
    cat >"$(gate_fixture slopos-ostd/src/mm/vm_space.rs)" <<'RS'
fn ostd_owns_the_tables() { KERNEL_VM_SPACE; kernel_vm_space(); let c: CursorMut; }
RS
    # A second module taking the kernel space's mutating cursor.
    cat >"$(gate_fixture drivers/src/second_writer.rs)" <<'RS'
fn remap() {
    let mut guard = slopos_kernel_services::kernel_vm_space::kernel_vm_space().lock();
    let cursor = guard.cursor_mut(range);
}
RS
    # A second handle on the static.
    cat >"$(gate_fixture net/src/second_handle.rs)" <<'RS'
fn peek() -> bool { slopos_kernel_services::kernel_vm_space::KERNEL_VM_SPACE.get().is_some() }
RS
    # A static home driving the cursor through the static.
    cat >"$(gate_fixture boot/src/boot_memory.rs)" <<'RS'
fn install() { KERNEL_VM_SPACE.call_once(|| SpinLock::new(space)); }
fn remap() { KERNEL_VM_SPACE.get().unwrap().lock().cursor_mut(range); }
RS
    # A store through a page-table word outside OSTD.
    cat >"$(gate_fixture mm/src/paging/table_store.rs)" <<'RS'
fn set(phys: u64, i: usize, v: u64) {
    with_atomic_u64_in_page(base(phys), i, |slot| slot.store(v, Ordering::Release));
}
RS
    cat >"$(gate_fixture mm/src/paging/table_load.rs)" <<'RS'
fn get(phys: u64, i: usize) -> u64 {
    with_atomic_u64_in_page(base(phys), i, |slot| slot.load(Ordering::Acquire))
}
RS
    # Reads, CR3 loads and comments, which must stay silent.
    cat >"$(gate_fixture core/src/reader.rs)" <<'RS'
fn read_side() {
    let guard = kernel_vm_space().lock();
    let cur = guard.cursor(range);
    kernel_vm_space().lock().activate_kernel_master();
}
RS
    cat >"$(gate_fixture boot/src/commented.rs)" <<'RS'
// Every kernel_vm_space() write goes through CursorMut in kernel_mappings;
// KERNEL_VM_SPACE is installed by boot_memory.
fn nothing() {}
RS
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT")"
    gate_expect writer 2 "a second module, or a static home, driving the kernel space's CursorMut"
    gate_expect store 1 "a page-table word stored outside OSTD"
    gate_expect static 1 "a second handle on KERNEL_VM_SPACE"
    gate_expect floor 0 "the writer is where it should be"
    gate_expect_silent 'reader\.rs|commented\.rs|slopos-ostd|kernel_vm_space\.rs|table_load\.rs' \
        "reads, loads, CR3 loads, comments, OSTD and the static's definition"

    printf 'fn moved() {}\n' >"$GATE_FIXTURE_ROOT/$WRITER"
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT")"
    gate_expect floor 1 "kernel_mappings takes no cursor"

    gate_selftest_end
fi

GATE_FINDINGS="$(run_scan "$REPO_ROOT")"
if [ -n "$GATE_FINDINGS" ]; then
    echo "check_kernel_pml4_writer: FAIL — the kernel master's page tables have one writer:" >&2
    printf '%s\n' "$GATE_FINDINGS" | sed 's/^/  /' >&2
    cat >&2 <<'MSG'

  Kernel-half writes go through slopos_mm::kernel_mappings, which takes
  CursorMut under the KERNEL_VM_SPACE lock; reach it through the
  kernel_vm_space() accessors, and add a kernel mapping helper there rather
  than a cursor anywhere else.
MSG
    exit 1
fi

echo "check_kernel_pml4_writer: OK — the kernel master's page tables have one writer"
