#!/usr/bin/env bash
# Hold the private call surface to slopos_abi::BASE_INTERFACE.
#
# A base is built against one interface number and a kernel serves a range of
# them; the number is the only thing that says whether the two can meet. That
# promise is worth nothing if the private calls can change shape while the
# number stays — the base and the kernel would agree on 1 and disagree on what
# 1 means, and nothing in either build would notice.
#
# So the surface is digested from the compiler's own view of slopos-abi
# (rustdoc JSON): every SlopOS-private syscall's number and the signature its
# docs give in a `name(...)` code span; every abi item a signature names or a
# call's docs link; the `root` globs in scripts/gates/base-interface.txt for
# what a call reads or writes behind a byte buffer or an op word; and every
# item those reach through field, variant, alias and constant types. Each
# item is rendered without docs or rustdoc ids (structs with repr and fields
# in order, enums with discriminants, constants with their evaluated value,
# functions and statics as their source tokens), so prose and formatting
# never move the digest. The ledger in the same file records
# `interface <n> <sha256>` for every number ever issued.
#
# ---------------------------------------------------------------------------
# The checks
# ---------------------------------------------------------------------------
#
#   bump        the ledger's row for BASE_INTERFACE has another digest: the
#               surface changed without a new number.
#   unrecorded  BASE_INTERFACE has no row: bumped, not yet recorded.
#   ledger      a malformed or out-of-order row, a row past BASE_INTERFACE,
#               a number skipped, or BASE_INTERFACE_MIN outside
#               1..=BASE_INTERFACE.
#   signature   a private call whose docs carry no `name(...)` span.
#   unresolved  a capitalised identifier in a signature that names no abi
#               item, or more than one.
#   dead        a `root` glob that reaches nothing the surface renders.
#   meta        SYSCALL_PRIVATE_BASE/END or BASE_INTERFACE(_MIN) unreadable.
#   floor       fewer than 20 private calls or 200 surface lines: a parse that
#               read nothing would otherwise digest an empty surface forever.
#
# bump and unrecorded are only judged on a surface read in full. A parser
# that fails, or a rustdoc JSON format_version other than the one it was
# written against, exits 2: a toolchain bump re-validates the parser rather
# than shifting digests.
#
# Modes: no argument checks; --record appends `interface <BASE_INTERFACE>
# <digest>` when that is the ledger's next row and refuses anything else (a
# recorded row is never rewritten); --print writes the surface lines, to diff
# before and after a change; --self-test runs the gate against fixture crates.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"

usage() {
    echo "usage: check_base_interface.sh [--self-test | --record | --print]" >&2
    exit 2
}

MODE=check
[ $# -le 1 ] || usage
case "${1:-}" in
    --self-test) MODE=self-test ;;
    --record) MODE=record ;;
    --print) MODE=print ;;
    "") ;;
    *) usage ;;
esac

PARSER="$SCRIPT_DIR/lib/base_interface.py"
GATE_FILE="$REPO_ROOT/scripts/gates/base-interface.txt"
TARGET_DIR="$REPO_ROOT/builddir/gates/base-interface"
CHANNEL="$(sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"

MIN_CALLS=20
MIN_LINES=200

die_env() {
    echo "check_base_interface: $*" >&2
    echo "  Refusing to report OK on a surface that was never read." >&2
    exit 2
}

command -v python3 >/dev/null 2>&1 || die_env "python3 is not installed"
[ -n "$CHANNEL" ] || die_env "no channel in rust-toolchain.toml"

run_parser() {
    local json="$1" src_root="$2" gate="$3" min_calls="$4" min_lines="$5" mode="$6"
    python3 "$PARSER" --json "$json" --src-root "$src_root" --gate-file "$gate" \
        --min-calls "$min_calls" --min-lines "$min_lines" "$mode"
}

# ---------------------------------------------------------------------------
# Self-test fixtures: a miniature slopos_abi — three private calls, a struct
# named by a signature with a nested struct, a module rooted by a doc link, a
# family rooted by a glob, and a Linux-ish struct nothing reaches.
# ---------------------------------------------------------------------------

fx_lib() {
    cat > "$1" <<'RS'
#![no_std]
pub mod base_interface {
pub const BASE_INTERFACE: u32 = 1;
pub const BASE_INTERFACE_MIN: u32 = 1;
}
pub mod syscall {
pub mod numbers {
/// `read(fd, buf: *mut Nothing, len)`: Linux-numbered, never parsed.
pub const SYSCALL_READ: u64 = 0;
pub const SYSCALL_PRIVATE_BASE: u64 = 1024;
pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 8;
pub const SYSCALL_PRIVATE_END: u64 = SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);
/// `widget_get(out: *mut Widget,
/// max) -> entries written` — fills widgets.
pub const SYSCALL_WIDGET_GET: u64 = SYSCALL_PRIVATE_BASE + 0;
/// Drives the gadget: `gadget_ctl(op, arg: u64)`, `op` from [`crate::gadget`].
pub const SYSCALL_GADGET_CTL: u64 = SYSCALL_PRIVATE_BASE + 1;
/// `knob_set(value: Knob) -> 0`.
pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;
}
}
pub mod widget {
/// A widget.
#[repr(C)]
pub struct Widget {
pub id: u32,
pub inner: Inner,
pub word: Word,
}
#[repr(C)]
pub union Word {
pub raw: u32,
pub bytes: [u8; 4],
}
#[repr(C)]
pub struct Inner {
pub a: u16,
pub b: [u8; INNER_LEN],
}
pub const INNER_LEN: usize = 4;
impl Widget {
pub const SIZE: usize = 8 + INNER_LEN;
pub fn helper(&self) -> u32 { self.id }
}
}
pub mod gadget {
pub const GADGET_OP_A: u32 = 1;
#[repr(u8)]
pub enum Mode {
Off = 0,
On = 1,
}
pub fn encode(m: Mode) -> u8 { m as u8 }
pub static TABLE: [u8; 2] = [1, 2];
}
pub mod flags {
pub const FLAG_X: u32 = 1 << 0;
pub const FLAG_Y: u32 = 1 << 1;
pub const OTHER: u32 = 9;
}
pub type Knob = u32;
pub mod linuxish {
#[repr(C)]
pub struct Stat {
pub size: u64,
}
}
RS
}

fx_thin() {
    cat > "$1" <<'RS'
pub mod base_interface {
pub const BASE_INTERFACE: u32 = 1;
pub const BASE_INTERFACE_MIN: u32 = 1;
}
pub mod syscall {
pub mod numbers {
pub const SYSCALL_PRIVATE_BASE: u64 = 1024;
pub const SYSCALL_PRIVATE_END: u64 = 1032;
/// `only_call()`.
pub const SYSCALL_ONLY_CALL: u64 = SYSCALL_PRIVATE_BASE + 0;
}
}
RS
}

# A fixture directory holding lib.rs: `<name>`, from base lib.rs with each
# `<exact line>` `<replacement>` pair applied (awk expands `\n`).
fx_variant() {
    local name="$1" src
    shift
    src="$GATE_FIXTURE_ROOT/base/lib.rs"
    mkdir -p "$GATE_FIXTURE_ROOT/$name"
    cp "$src" "$GATE_FIXTURE_ROOT/$name/lib.rs"
    while [ $# -ge 2 ]; do
        awk -v from="$1" -v to="$2" '$0 == from { print to; hit = 1; next } { print }
            END { if (!hit) { print "fx_variant: no line: " from > "/dev/stderr"; exit 1 } }' \
            "$GATE_FIXTURE_ROOT/$name/lib.rs" > "$GATE_FIXTURE_ROOT/$name/lib.rs.new"
        mv "$GATE_FIXTURE_ROOT/$name/lib.rs.new" "$GATE_FIXTURE_ROOT/$name/lib.rs"
        shift 2
    done
    fx_doc "$name"
}

fx_doc() {
    local dir="$GATE_FIXTURE_ROOT/$1" log
    if ! log="$(cd "$dir" && rustdoc "+$CHANNEL" --edition 2024 --crate-type lib \
        --crate-name slopos_abi -Zunstable-options --output-format json \
        --document-private-items --cap-lints allow -o out lib.rs 2>&1)"; then
        echo "check_base_interface --self-test: fixture $1 does not document:" >&2
        printf '%s\n' "$log" | sed 's/^/      /' >&2
        exit 1
    fi
}

fx_scan() {
    local fixture="$1" gate="$2" rc=0 want=0
    GATE_FINDINGS="$(run_parser "$GATE_FIXTURE_ROOT/$fixture/out/slopos_abi.json" \
        "$GATE_FIXTURE_ROOT/$fixture" "$gate" 3 10 check)" || rc=$?
    if [ "$rc" -gt 1 ] || ! printf '%s\n' "$GATE_FINDINGS" | grep -q '^info	'; then
        echo "check_base_interface --self-test: the parser could not read fixture $fixture (exit $rc)" >&2
        exit 1
    fi
    GATE_FINDINGS="$(printf '%s\n' "$GATE_FINDINGS" | grep -v '^info	' || true)"
    [ -z "$GATE_FINDINGS" ] || want=1
    if [ "$rc" -ne "$want" ]; then
        echo "check_base_interface --self-test: the parser exited $rc on fixture $fixture, whose findings call for $want" >&2
        exit 1
    fi
}

fx_record() {
    local fixture="$1" gate="$2"
    run_parser "$GATE_FIXTURE_ROOT/$fixture/out/slopos_abi.json" "$GATE_FIXTURE_ROOT/$fixture" \
        "$gate" 3 10 record >/dev/null 2>&1
}

selftest_fail() {
    echo "check_base_interface --self-test: $*" >&2
    GATE_SELF_TEST_FAIL=1
}

if [ "$MODE" = self-test ]; then
    command -v rustdoc >/dev/null 2>&1 || die_env "rustdoc is not installed"
    gate_selftest_begin check_base_interface

    mkdir -p "$GATE_FIXTURE_ROOT/base"
    fx_lib "$GATE_FIXTURE_ROOT/base/lib.rs"
    fx_doc base
    GATE="$GATE_FIXTURE_ROOT/gate.txt"
    printf '%s\n' "# fixture" "root slopos_abi::flags::FLAG_*" > "$GATE"

    if ! fx_record base "$GATE" || [ "$(grep -c '^interface ' "$GATE")" -ne 1 ] \
        || ! grep -q '^interface 1 [0-9a-f]\{64\}$' "$GATE"; then
        selftest_fail "--record on an empty ledger did not append exactly interface 1"
    else
        echo "  record: an empty ledger takes exactly one row, interface 1"
    fi
    if ! fx_record base "$GATE" || [ "$(grep -c '^interface ' "$GATE")" -ne 1 ]; then
        selftest_fail "--record of an already-recorded surface changed the ledger"
    fi
    run_parser "$GATE_FIXTURE_ROOT/base/out/slopos_abi.json" "$GATE_FIXTURE_ROOT/base" \
        "$GATE" 3 10 print > "$GATE_FIXTURE_ROOT/print.txt"
    if ! grep -qxF 'call 1024 SYSCALL_WIDGET_GET widget_get(out: *mut Widget, max) -> entries written' \
        "$GATE_FIXTURE_ROOT/print.txt"; then
        selftest_fail "--print does not show the multi-line signature collapsed onto its call line"
    else
        echo "  print: a signature spanning doc lines is one call line"
    fi

    fx_scan base "$GATE"
    gate_expect_silent '.' "the recorded surface, including a Linux call whose signature names nothing"

    fx_variant prose \
        '/// `knob_set(value: Knob) -> 0`.' '/// Sets the `knob` — `knob_set(value: Knob) -> 0` — and says more about it.' \
        '/// A widget.' '/// A widget, described at greater length.'
    fx_scan prose "$GATE"
    gate_expect_silent '.' "doc prose around the signatures and on a surface struct"

    fx_variant format \
        'pub fn encode(m: Mode) -> u8 { m as u8 }' 'pub fn encode(m: Mode)->u8 {\n    // why it is a cast\n    m as u8 /* a block comment */\n}' \
        'pub a: u16,' 'pub a: u16, // a comment' \
        'pub const FLAG_Y: u32 = 1 << 1;' 'pub const FLAG_Y: u32 =\n    1<<1;' \
        'pub const SIZE: usize = 8 + INNER_LEN;' 'pub const SIZE: usize = 8+INNER_LEN; // why'
    fx_scan format "$GATE"
    gate_expect_silent '.' "comments and formatting inside surface items and a rooted function"

    fx_variant reorder \
        'pub const FLAG_X: u32 = 1 << 0;' '@@' \
        'pub const FLAG_Y: u32 = 1 << 1;' 'pub const FLAG_X: u32 = 1 << 0;' \
        '@@' 'pub const FLAG_Y: u32 = 1 << 1;' \
        'pub const INNER_LEN: usize = 4;' '' \
        'pub mod widget {' 'pub mod widget {\npub const INNER_LEN: usize = 4;'
    fx_scan reorder "$GATE"
    gate_expect_silent '.' "items reordered within their modules"

    fx_variant outside \
        'pub size: u64,' 'pub size: u32,' \
        'pub const OTHER: u32 = 9;' 'pub const OTHER: u32 = 10;' \
        'pub fn helper(&self) -> u32 { self.id }' 'pub fn helper(&self) -> u32 { self.id + 1 }'
    fx_scan outside "$GATE"
    gate_expect_silent '.' "a change to items the surface does not reach (an unrooted struct, an unmatched constant, a non-rooted method)"
    { echo "root slopos_abi::widget::Widget::helper"; grep -v '^interface ' "$GATE"; } \
        > "$GATE_FIXTURE_ROOT/gate-method.txt"
    fx_record base "$GATE_FIXTURE_ROOT/gate-method.txt"
    fx_scan outside "$GATE_FIXTURE_ROOT/gate-method.txt"
    gate_expect bump 1 "the same method's body, once a root names it"

    fx_variant nested 'pub a: u16,' 'pub a: u32,'
    fx_scan nested "$GATE"
    gate_expect bump 1 "a nested field's type changed without a bump"

    fx_variant fieldorder \
        'pub id: u32,' '@@' \
        'pub inner: Inner,' 'pub id: u32,' \
        '@@' 'pub inner: Inner,'
    fx_scan fieldorder "$GATE"
    gate_expect bump 1 "two fields of a reached struct swapped"
    cp "$GATE" "$GATE_FIXTURE_ROOT/before.txt"
    if fx_record nested "$GATE" || ! cmp -s "$GATE" "$GATE_FIXTURE_ROOT/before.txt"; then
        selftest_fail "--record rewrote or accepted a changed surface under a recorded number"
    else
        echo "  record: refuses a changed surface under an already-recorded number"
    fi

    fx_variant newcall \
        'pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;' \
        'pub const SYSCALL_KNOB_SET: u64 = SYSCALL_PRIVATE_BASE + 2;\n/// `dial_set(level)`.\npub const SYSCALL_DIAL_SET: u64 = SYSCALL_PRIVATE_BASE + 3;'
    fx_scan newcall "$GATE"
    gate_expect bump 1 "a new private call without a bump"

    fx_variant sigarg '/// `knob_set(value: Knob) -> 0`.' '/// `knob_set(value: Knob, extra) -> 0`.'
    fx_scan sigarg "$GATE"
    gate_expect bump 1 "a signature gained an argument"

    fx_variant discr 'On = 1,' 'On = 2,'
    fx_scan discr "$GATE"
    gate_expect bump 1 "a discriminant changed in an enum rooted by a module link"

    fx_variant rootconst 'pub const FLAG_X: u32 = 1 << 0;' 'pub const FLAG_X: u32 = 1 << 2;'
    fx_scan rootconst "$GATE"
    gate_expect bump 1 "a glob-rooted constant's value changed"

    fx_variant fnbody 'pub fn encode(m: Mode) -> u8 { m as u8 }' 'pub fn encode(m: Mode) -> u8 { m as u8 ^ 1 }'
    fx_scan fnbody "$GATE"
    gate_expect bump 1 "a reached function's body changed"

    fx_variant static 'pub static TABLE: [u8; 2] = [1, 2];' 'pub static TABLE: [u8; 2] = [1, 3];'
    fx_scan static "$GATE"
    gate_expect bump 1 "a reached static's value changed"

    fx_variant alias 'pub type Knob = u32;' 'pub type Knob = u64;'
    fx_scan alias "$GATE"
    gate_expect bump 1 "a signature's alias retargeted"

    fx_variant repr '#[repr(u8)]' '#[repr(u16)]'
    fx_scan repr "$GATE"
    gate_expect bump 1 "an enum's repr changed"

    fx_variant union 'pub bytes: [u8; 4],' 'pub bytes: [u8; 2],'
    fx_scan union "$GATE"
    gate_expect bump 1 "a union field reached through a struct changed"

    fx_variant assoc 'pub const SIZE: usize = 8 + INNER_LEN;' 'pub const SIZE: usize = 9 + INNER_LEN;'
    fx_scan assoc "$GATE"
    gate_expect bump 1 "an inherent constant of a reached type changed"

    fx_variant bumped 'pub const BASE_INTERFACE: u32 = 1;' 'pub const BASE_INTERFACE: u32 = 2;'
    fx_scan bumped "$GATE"
    gate_expect unrecorded 1 "BASE_INTERFACE bumped without a ledger row"

    fx_variant nextiface \
        'pub a: u16,' 'pub a: u32,' \
        'pub const BASE_INTERFACE: u32 = 1;' 'pub const BASE_INTERFACE: u32 = 2;'
    cp "$GATE" "$GATE_FIXTURE_ROOT/gate2.txt"
    if ! fx_record nextiface "$GATE_FIXTURE_ROOT/gate2.txt" \
        || [ "$(grep -c '^interface ' "$GATE_FIXTURE_ROOT/gate2.txt")" -ne 2 ] \
        || ! diff <(cat "$GATE") <(sed '$d' "$GATE_FIXTURE_ROOT/gate2.txt") >/dev/null \
        || ! tail -n 1 "$GATE_FIXTURE_ROOT/gate2.txt" | grep -q '^interface 2 [0-9a-f]\{64\}$'; then
        selftest_fail "--record on a valid bump did not append exactly one row, interface 2"
    else
        echo "  record: a valid bump appends exactly one row, interface 2, and leaves row 1"
    fi
    fx_scan nextiface "$GATE_FIXTURE_ROOT/gate2.txt"
    gate_expect_silent '.' "the bumped and recorded surface"

    fx_variant nosig '/// `knob_set(value: Knob) -> 0`.' '/// Sets the knob.'
    fx_scan nosig "$GATE"
    gate_expect signature 1 "a private call with no signature"
    gate_expect bump 0 "not judged on a surface read in part"

    printf '%s\n' "root slopos_abi::nothing::*" >> "$GATE_FIXTURE_ROOT/gate-dead.txt"
    cat "$GATE" >> "$GATE_FIXTURE_ROOT/gate-dead.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-dead.txt"
    gate_expect dead 1 "a root glob matching nothing"
    { echo "root slopos_abi::gadget::Mode::On"; cat "$GATE"; } > "$GATE_FIXTURE_ROOT/gate-variant.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-variant.txt"
    gate_expect dead 1 "a root glob reaching only an enum variant, which renders nothing"
    { echo "root slopos_abi::base_interface::*"; cat "$GATE"; } > "$GATE_FIXTURE_ROOT/gate-excl.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-excl.txt"
    gate_expect dead 1 "a root glob reaching only the interface number itself, which is never digested"

    fx_variant unresolved \
        '/// `knob_set(value: Knob) -> 0`.' '/// `knob_set(level, value: Knobb) -> Modee`.' \
        'pub size: u64,' 'pub size: u64,\n}\npub struct Widget {\npub other: u8,'
    fx_scan unresolved "$GATE"
    gate_expect unresolved 3 "a later argument's type and a return naming nothing, and a type naming two items"

    fx_variant badmin 'pub const BASE_INTERFACE_MIN: u32 = 1;' 'pub const BASE_INTERFACE_MIN: u32 = 2;'
    fx_scan badmin "$GATE"
    gate_expect ledger 1 "BASE_INTERFACE_MIN above BASE_INTERFACE"

    { cat "$GATE"; echo "interface 3 $(printf '%064d' 0)"; echo "interface two"; } > "$GATE_FIXTURE_ROOT/gate-bad.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-bad.txt"
    gate_expect ledger 2 "a row out of sequence and past BASE_INTERFACE, and a malformed row"
    { cat "$GATE"; echo "interface 1 $(printf '%064d' 0)"; } > "$GATE_FIXTURE_ROOT/gate-dup.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-dup.txt"
    gate_expect ledger 1 "a number recorded twice"
    { cat "$GATE"; echo "interface 2 $(printf '%064d' 0)"; } > "$GATE_FIXTURE_ROOT/gate-ahead.txt"
    fx_scan base "$GATE_FIXTURE_ROOT/gate-ahead.txt"
    gate_expect ledger 1 "a row past BASE_INTERFACE"

    fx_variant skipnum 'pub const BASE_INTERFACE: u32 = 1;' 'pub const BASE_INTERFACE: u32 = 3;'
    fx_scan skipnum "$GATE"
    gate_expect ledger 1 "BASE_INTERFACE skipping the ledger's next number"
    gate_expect unrecorded 0 "not judged on a ledger that is itself wrong"
    fx_variant badbump \
        '/// `knob_set(value: Knob) -> 0`.' '/// Sets the knob.' \
        'pub const BASE_INTERFACE: u32 = 1;' 'pub const BASE_INTERFACE: u32 = 2;'
    cp "$GATE" "$GATE_FIXTURE_ROOT/before.txt"
    if fx_record skipnum "$GATE" || fx_record badbump "$GATE" \
        || ! cmp -s "$GATE" "$GATE_FIXTURE_ROOT/before.txt"; then
        selftest_fail "--record wrote a row for a skipped number or a surface with findings"
    else
        echo "  record: refuses a skipped number and a surface with findings, writing nothing"
    fi

    fx_variant nometa \
        'pub const SYSCALL_PRIVATE_END: u64 = SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);' '' \
        'pub const BASE_INTERFACE_MIN: u32 = 1;' ''
    fx_scan nometa "$GATE"
    gate_expect meta 2 "SYSCALL_PRIVATE_END and BASE_INTERFACE_MIN missing"

    mkdir -p "$GATE_FIXTURE_ROOT/thin"
    fx_thin "$GATE_FIXTURE_ROOT/thin/lib.rs"
    fx_doc thin
    printf '%s\n' "# empty" > "$GATE_FIXTURE_ROOT/gate-thin.txt"
    fx_scan thin "$GATE_FIXTURE_ROOT/gate-thin.txt"
    gate_expect floor 2 "a thin crate under both floors"

    sed 's/"format_version":[0-9]*/"format_version":1/' "$GATE_FIXTURE_ROOT/base/out/slopos_abi.json" \
        > "$GATE_FIXTURE_ROOT/old.json"
    rc=0
    run_parser "$GATE_FIXTURE_ROOT/old.json" "$GATE_FIXTURE_ROOT/base" "$GATE" 3 10 check \
        >/dev/null 2>&1 || rc=$?
    if [ "$rc" -ne 2 ]; then
        selftest_fail "an unknown rustdoc format_version exited $rc, not 2"
    else
        echo "  format: an unknown rustdoc format_version refuses with exit 2"
    fi

    python3 - "$GATE_FIXTURE_ROOT/base/out/slopos_abi.json" "$GATE_FIXTURE_ROOT/shape.json" <<'PY'
import json, sys
doc = json.load(open(sys.argv[1]))
for item in doc["index"].values():
    item.pop("attrs", None)
json.dump(doc, open(sys.argv[2], "w"))
PY
    rc=0
    run_parser "$GATE_FIXTURE_ROOT/shape.json" "$GATE_FIXTURE_ROOT/base" "$GATE" 3 10 check \
        >/dev/null 2>&1 || rc=$?
    if [ "$rc" -ne 2 ]; then
        selftest_fail "a JSON shape the parser does not expect exited $rc, not 2"
    else
        echo "  shape: a JSON shape the parser does not expect refuses with exit 2"
    fi

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# The real run.
# ---------------------------------------------------------------------------

[ -s "$GATE_FILE" ] || die_env "$GATE_FILE is missing or empty"
command -v cargo >/dev/null 2>&1 || die_env "cargo is not installed"

if ! log="$(cd "$REPO_ROOT" && cargo "+$CHANNEL" rustdoc --quiet --locked -p slopos-abi --lib \
    --target-dir "$TARGET_DIR" -- --cap-lints allow --document-private-items \
    -Zunstable-options --output-format json 2>&1)"; then
    printf '%s\n' "$log" >&2
    die_env "cargo rustdoc on slopos-abi failed"
fi
JSON="$TARGET_DIR/doc/slopos_abi.json"
[ -s "$JSON" ] || die_env "rustdoc wrote no $JSON"

case "$MODE" in
    print)
        run_parser "$JSON" "$REPO_ROOT" "$GATE_FILE" "$MIN_CALLS" "$MIN_LINES" print
        exit
        ;;
    record)
        rc=0
        out="$(run_parser "$JSON" "$REPO_ROOT" "$GATE_FILE" "$MIN_CALLS" "$MIN_LINES" record)" || rc=$?
        [ -z "$out" ] || printf '%s\n' "$out" | sed 's/^/    /' >&2
        exit "$rc"
        ;;
esac

rc=0
GATE_FINDINGS="$(run_parser "$JSON" "$REPO_ROOT" "$GATE_FILE" "$MIN_CALLS" "$MIN_LINES" check)" || rc=$?
case "$rc" in
    0 | 1) ;;
    *) exit 2 ;;
esac
INFO="$(printf '%s\n' "$GATE_FINDINGS" | grep '^info	' || true)"
GATE_FINDINGS="$(printf '%s\n' "$GATE_FINDINGS" | grep -v '^info	' || true)"
[ -n "$INFO" ] || die_env "the parser exited $rc without its summary line"

report_tag() {
    local hits
    hits="$(printf '%s\n' "$GATE_FINDINGS" | grep "^$1	" | cut -f2- || true)"
    [ -n "$hits" ] || return 0
    echo "check_base_interface: $2" >&2
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
}

if [ -n "$GATE_FINDINGS" ]; then
    report_tag bump "the private call surface changed under an issued number:"
    report_tag unrecorded "BASE_INTERFACE is not in the ledger:"
    report_tag ledger "scripts/gates/base-interface.txt's ledger is inconsistent:"
    report_tag signature "a private call does not write down its signature:"
    report_tag unresolved "a signature names a type the abi does not define once:"
    report_tag dead "a root glob roots nothing:"
    report_tag meta "the interface metadata constants could not be read:"
    report_tag floor "the surface is too small to be believed:"
    if printf '%s\n' "$GATE_FINDINGS" | grep -q '^bump	'; then
        cat >&2 <<'MSG'

  A base built against BASE_INTERFACE trusts every private call to keep the
  shape it had when that number was recorded. To change the surface: bump
  BASE_INTERFACE in abi/src/base_interface.rs; decide whether
  BASE_INTERFACE_MIN rises (only when the change reshapes something an
  older base calls — an addition leaves it, and so does a call retired to
  ENOSYS in a slot never reused); then run
  scripts/check_base_interface.sh --record. To see what moved, compare
  `scripts/check_base_interface.sh --print` before and after the change.
MSG
    elif printf '%s\n' "$GATE_FINDINGS" | grep -q '^unrecorded	'; then
        cat >&2 <<'MSG'

  If the bump is deliberate and BASE_INTERFACE_MIN has been decided (it rises
  only when the change reshapes something an older base calls; a call
  retired to ENOSYS leaves it), run scripts/check_base_interface.sh --record
  and commit the new row.
MSG
    fi
    exit 1
fi

calls="$(printf '%s\n' "$INFO" | cut -f2)"
lines="$(printf '%s\n' "$INFO" | cut -f3)"
digest="$(printf '%s\n' "$INFO" | cut -f4)"
echo "check_base_interface: OK — $calls private calls, $lines surface lines, digest ${digest:0:16}…"
echo "check_base_interface: matches the ledger's row for BASE_INTERFACE"
