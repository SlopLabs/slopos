#!/usr/bin/env bash
# Hold a program SlopOS ships outside its base to the one interface it may
# use: `libc.so`, whose exports only ever grow (scripts/check_libc_exports.sh).
# A program outside the base changes on its own schedule, the base and the
# kernel on theirs, so it may not depend on what the base interface number
# guards: a system call made in place reaches the private call surface, or a
# Linux one, without the library between.
#
# Each program must be
#   dynamic   carry a PT_INTERP: a static one has slibc's own copy linked in;
#   on ld.so  name /lib/ld-slopos.so.1 as that interpreter;
#   on libc   list libc.so in DT_NEEDED;
#   clean     hold no `syscall` instruction in its own code.
#
# Usage: check_loose_programs.sh PROGRAM...
#        check_loose_programs.sh --self-test

set -euo pipefail

SELF="check_loose_programs"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INTERP="/lib/ld-slopos.so.1"

READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"
OBJDUMP="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objdump)"

# The findings for one program, one line each; nothing when it holds.
findings() {
    local program="$1" headers interp calls
    headers="$("$READOBJ" --elf-output-style=GNU --program-headers --dynamic-table "$program" 2>&1)" || {
        echo "$program: not an ELF file llvm-readobj reads: $headers"
        return
    }
    interp="$(printf '%s\n' "$headers" | sed -n 's/.*\[Requesting program interpreter: \(.*\)\]$/\1/p')"
    if [ -z "$interp" ]; then
        echo "$program: static, with no PT_INTERP: it carries its own C library"
    elif [ "$interp" != "$INTERP" ]; then
        echo "$program: its interpreter is $interp, not $INTERP"
    fi
    if ! printf '%s\n' "$headers" | grep -qE '\(NEEDED\)[[:space:]]+Shared library: \[libc\.so\]'; then
        echo "$program: libc.so is not among its DT_NEEDED"
    fi
    calls="$("$OBJDUMP" -d --no-show-raw-insn "$program" |
        awk '/^[[:space:]]*[0-9a-f]+:[[:space:]]+syscall$/ { n++ } END { print n + 0 }')" || {
        echo "$program: llvm-objdump cannot disassemble it"
        return
    }
    if [ "$calls" -gt 0 ]; then
        echo "$program: $calls syscall instruction(s) of its own"
    fi
}

check() {
    local program found failed=0
    [ $# -gt 0 ] || { echo "usage: $SELF.sh PROGRAM... | --self-test" >&2; exit 2; }
    for program in "$@"; do
        [ -f "$program" ] || { echo "$SELF: $program: no such file" >&2; exit 2; }
        found="$(findings "$program")"
        if [ -n "$found" ]; then
            printf '%s: %s\n' "$SELF" "$found" | sed "2,\$s/^/$SELF: /" >&2
            failed=1
        fi
    done
    [ "$failed" = 0 ] || exit 1
    echo "$SELF: OK ($# program(s) reach the system through libc.so alone)"
}

self_test() {
    local llc lld fail=0
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)"
    lld="$("$SCRIPT_DIR/llvm_tool.sh" rust-lld)"
    work="$(mktemp -d)"
    trap 'rm -rf "$work"' EXIT INT TERM

    object() {
        "$llc" -mtriple=x86_64-unknown-none-elf -relocation-model=pic -filetype=obj \
            -o "$work/$1.o" -
    }
    printf 'define void @f() {\n  ret void\n}\n' | object lib
    printf 'declare void @f()\ndefine void @_start() {\n  call void @f()\n  ret void\n}\n' |
        object calls
    printf 'declare void @f()\ndefine void @_start() {\n  call void @f()\n  call void asm sideeffect "syscall", ""()\n  ret void\n}\n' |
        object raw
    for soname in libc.so libother.so; do
        "$lld" -flavor gnu -shared --soname="$soname" -o "$work/$soname" "$work/lib.o"
    done
    link() {
        local out="$1"
        shift
        "$lld" -flavor gnu -o "$work/$out" "$@"
    }
    link good --dynamic-linker="$INTERP" "$work/calls.o" "$work/libc.so"
    link static -static "$work/lib.o" "$work/calls.o"
    link foreign --dynamic-linker=/lib64/ld-linux-x86-64.so.2 "$work/calls.o" "$work/libc.so"
    link other --dynamic-linker="$INTERP" "$work/calls.o" "$work/libother.so"
    link raw --dynamic-linker="$INTERP" "$work/raw.o" "$work/libc.so"

    expect() {
        local label="$1" want_exit="$2" want_text="$3" out status
        shift 3
        set +e
        out="$("$0" "$@" 2>&1)"
        status=$?
        set -e
        if [ "$status" -ne "$want_exit" ] || ! printf '%s\n' "$out" | grep -qF -- "$want_text"; then
            echo "$SELF --self-test: $label: want exit $want_exit and '$want_text', got $status:" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            fail=1
        fi
    }
    expect "a program on libc.so alone" 0 "OK (1 program" "$work/good"
    expect "a static program" 1 "static, with no PT_INTERP" "$work/static"
    expect "another interpreter" 1 "its interpreter is /lib64/ld-linux" "$work/foreign"
    expect "no libc.so" 1 "libc.so is not among its DT_NEEDED" "$work/other"
    expect "a syscall instruction" 1 "1 syscall instruction(s)" "$work/raw"
    expect "one bad among good" 1 "$work/raw: 1 syscall" "$work/good" "$work/raw"
    expect "no program" 2 "usage"
    [ "$fail" = 0 ] || exit 1
    echo "$SELF --self-test: OK"
}

if [ "${1:-}" = --self-test ]; then
    self_test
else
    check "$@"
fi
