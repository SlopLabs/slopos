#!/usr/bin/env bash
# Hold `libc.so`'s interface to only ever growing.
#
# `libc.so` is the stable interface for programs built outside the base, the
# role libSystem has on macOS and relibc on Redox. Such a program breaks when a
# function it calls is removed or renamed, or when a structure it shares with
# the library changes size or layout — and nothing in this tree notices,
# because everything in the tree is rebuilt against the new headers together.
#
#   exports   every defined dynamic symbol, held to scripts/gates/libc/
#             exports.txt as `<name> func`, `<name> object|tls <size>`, or
#             `<name> <type>` for any other symbol type. A data export's size
#             is ABI: a non-PIE executable copy-relocates it, so growth breaks
#             it as surely as removal.
#   layouts   the size, alignment and every field offset of each complete
#             struct/union the headers in slibc/include define, held to
#             scripts/gates/libc/layouts.txt. Every record, not only those a
#             prototype names: exports also take structures through `void *`
#             (`setsockopt`'s `ip_mreq` and `linger`, `ioctl`'s `winsize`).
#             Measured by clang's own record-layout dump; field types are left
#             out because their spelling varies across clang releases.
#
# Findings: `removed` (a recorded export is gone or changed kind/size),
# `added` (an export not yet recorded), `layout` (a recorded record changed),
# `gone` (a recorded record is no longer defined), `new` (a record not yet
# recorded), `floor` (under 500 exports or 60 records: a parse that read
# nothing, not a smaller library). An addition is recorded with `--record`,
# which only adds lines and refuses while anything recorded was removed or
# changed, so a removal can only land as a hand edit a reviewer sees.
#
# Usage: check_libc_exports.sh [--require] [--record] [LIBC_SO]
#        check_libc_exports.sh --self-test
#
# Without the library (default builddir/libc.so) the gate is skipped, and
# without the host clang `scripts/cxx_host_tools.sh` resolves the layout half
# is; `--require` makes either an error instead.

set -euo pipefail

SELF="check_libc_exports"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"

MIN_EXPORTS=500
MIN_RECORDS=60
DATA_DIR="$REPO_ROOT/scripts/gates/libc"
INCLUDE_DIR="$REPO_ROOT/slibc/include"
CANONICALISE="$SCRIPT_DIR/lib/libc_layouts.py"

EXPORTS_HEADER='# libc.so dynamic symbol table, held by scripts/check_libc_exports.sh.
#
# `<name> func`, `<name> object|tls <size>` for data (a data export'"'"'s size
# is ABI: a non-PIE executable copy-relocates it), or `<name> <type>` for any
# other symbol type. Lines are only ever added, by `scripts/check_libc_exports.sh
# --record`; removing or changing one breaks programs built outside the base.'

LAYOUTS_HEADER='# Layout of every struct/union slibc/include defines, held by
# scripts/check_libc_exports.sh.
#
# `<record> size <n> align <n>`, then `<record> <member.path> <offset>` per
# field in declaration order (bit-fields as `<byte>:<first>-<last>`). An
# anonymous record named by a typedef is `typedef <name>`. Lines are only ever
# added, by `scripts/check_libc_exports.sh --record`; changing one breaks
# programs built outside the base.'

usage() {
    echo "usage: $SELF.sh [--require] [--record] [LIBC_SO] | --self-test" >&2
    exit 2
}

envfail() {
    echo "$SELF: $* — refusing to report OK" >&2
    exit 2
}

READOBJ=""
resolve_readobj() {
    [ -n "$READOBJ" ] || READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)" ||
        envfail "no llvm-readobj"
}

# Sets CLANG, or clears it and says why in CLANG_NOTE. The resolver's note
# about untested majors is noise here: this gate's output is version-neutral.
CLANG_NOTE=""
resolve_clang() {
    local out err
    err="$(mktemp)"
    if out="$("$SCRIPT_DIR/cxx_host_tools.sh" 2>"$err")"; then
        eval "$out"
    else
        CLANG=""
        CLANG_NOTE="$(sed -n '1s/^cxx_host_tools: //p' "$err")"
        [ -n "$CLANG_NOTE" ] || CLANG_NOTE="cxx_host_tools.sh resolved no clang"
    fi
    rm -f "$err"
}

extract_exports() {
    local so="$1" out="$2"
    resolve_readobj
    "$READOBJ" --elf-output-style=GNU --dyn-syms "$so" >"$out.raw" ||
        envfail "llvm-readobj could not read $so"
    awk '
        # readelf prints a size of 100000 or more in hex.
        function num(s,    n, i) {
            if (s !~ /^0x/) return s + 0
            n = 0
            s = tolower(substr(s, 3))
            for (i = 1; i <= length(s); i++)
                n = n * 16 + index("0123456789abcdef", substr(s, i, 1)) - 1
            return n
        }
        $1 ~ /^[0-9]+:$/ && NF >= 8 && $7 != "UND" && $5 != "LOCAL" {
            t = $4
            if (t == "FUNC" || t == "IFUNC" || t == "GNU_IFUNC") print $8 " func"
            else if (t == "OBJECT" || t == "COMMON") printf "%s object %d\n", $8, num($3)
            else if (t == "TLS") printf "%s tls %d\n", $8, num($3)
            else print $8 " " tolower(t)
        }' "$out.raw" | LC_ALL=C sort >"$out"
    rm -f "$out.raw"
}

dump_layouts() {
    local include="$1" out="$2" tu="$2.c" header
    : >"$tu"
    while IFS= read -r header; do
        printf '#include <%s>\n' "${header#"$include"/}" >>"$tu"
    done <<EOF
$(find "$include" -name '*.h' -type f | LC_ALL=C sort)
EOF
    "$CLANG" --target=x86_64-unknown-slopos -nostdlibinc -isystem "$include" \
        -std=c11 -fsyntax-only -Xclang -fdump-record-layouts-complete \
        "$tu" >"$out.dump" 2>"$out.err" ||
        envfail "$CLANG could not compile the headers under $include:
$(sed 's/^/    /' "$out.err")"
    python3 "$CANONICALISE" "$include" <"$out.dump" >"$out" ||
        envfail "the layout dump of $include did not parse"
    rm -f "$tu" "$out.dump" "$out.err"
}

# Findings over a recorded list and a current one. Lines group by their first
# <keyfields> fields; a group present in both must match line for line.
compare() {
    local keyfields="$1" missing="$2" changed="$3" added="$4" recorded="$5" current="$6"
    awk -v kf="$keyfields" -v t_missing="$missing" -v t_changed="$changed" \
        -v t_added="$added" '
        /^#/ || /^[[:space:]]*$/ { next }
        {
            k = $1
            for (i = 2; i <= kf; i++) k = k " " $i
        }
        FILENAME == ARGV[1] {
            if (!(k in rec)) { ro[++rn] = k; rec[k] = "" }
            rec[k] = rec[k] $0 "\n"
            next
        }
        {
            if (!(k in cur)) { co[++cn] = k; cur[k] = "" }
            cur[k] = cur[k] $0 "\n"
        }
        END {
            for (i = 1; i <= rn; i++) {
                k = ro[i]
                if (!(k in cur)) {
                    printf "%s\t%s: recorded, no longer present\n", t_missing, k
                } else if (rec[k] != cur[k]) {
                    split("", a); split("", b)
                    na = split(rec[k], a, "\n")
                    nb = split(cur[k], b, "\n")
                    for (j = 1; j <= na || j <= nb; j++) if (a[j] != b[j]) break
                    printf "%s\t%s: recorded \"%s\", now \"%s\"\n", t_changed, k, a[j], b[j]
                }
            }
            for (i = 1; i <= cn; i++)
                if (!(co[i] in rec)) printf "%s\t%s: not recorded\n", t_added, co[i]
        }' "$recorded" "$current"
}

scan_exports() {
    local recorded="$1" current="$2" count
    compare 1 removed removed added "$recorded" "$current"
    count="$(grep -c . "$current" || true)"
    [ "$count" -ge "$MIN_EXPORTS" ] ||
        printf 'floor\t%s exports parsed, under the floor of %s\n' "$count" "$MIN_EXPORTS"
}

record_count() {
    awk '{ print $1 " " $2 }' "$1" | uniq | grep -c . || true
}

scan_layouts() {
    local recorded="$1" current="$2" count
    compare 2 gone layout new "$recorded" "$current"
    count="$(record_count "$current")"
    [ "$count" -ge "$MIN_RECORDS" ] ||
        printf 'floor\t%s records parsed, under the floor of %s\n' "$count" "$MIN_RECORDS"
}

# Rewrites <recorded> as its own comment header (or <header> for a new file)
# followed by <current>, which only adds when <findings> carries nothing but
# additions. Returns 1, writing nothing, otherwise.
record_list() {
    local recorded="$1" current="$2" header="$3" findings="$4" blocking
    blocking="$(printf '%s\n' "$findings" | grep -Ev '^(added|new)	|^$' || true)"
    if [ -n "$blocking" ]; then
        echo "$SELF: --record refuses: a recorded line was removed or changed" >&2
        printf '%s\n' "$blocking" | cut -f2- | sed 's/^/    /' >&2
        return 1
    fi
    local next="$recorded.next"
    if [ -f "$recorded" ]; then
        awk '/^#/ || /^[[:space:]]*$/ { print; next } { exit }' "$recorded" >"$next"
    else
        printf '%s\n\n' "$header" >"$next"
    fi
    cat "$current" >>"$next"
    mv "$next" "$recorded"
}

report() {
    local findings="$1" tag hits
    while IFS='|' read -r tag text; do
        hits="$(printf '%s\n' "$findings" | grep "^$tag	" | cut -f2- || true)"
        [ -n "$hits" ] || continue
        echo "$SELF: $text" >&2
        printf '%s\n' "$hits" | sed 's/^/    /' >&2
    done <<'EOF'
removed|an export was removed or changed kind/size — programs built against it break:
added|exports are not recorded yet (run with --record):
layout|a header structure changed size, alignment or a field offset:
gone|a header structure is no longer defined:
new|header structures are not recorded yet (run with --record):
floor|the scan found too little to be believed:
EOF
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------

# A header set whose records cover every form the canonicaliser handles:
# a nested record, an anonymous union member, a bit-field, an anonymous
# record named by an attributed typedef (siginfo_t's form; the attribute binds
# to the typedef, so the record keeps align 4). <pad> blank lines move every
# line number; <extra> adds one record.
fx_headers() {
    local dir="$1" pad="$2" extra="$3" i
    mkdir -p "$dir"
    {
        i=0
        while [ "$i" -lt "$pad" ]; do echo; i=$((i + 1)); done
        cat <<'EOF'
/* fixture */
#include <stddef.h>
struct inner {
    int a;
    int b;
};
struct outer {
    int x;
    struct inner in;
    union {
        int u1;
        long u2;
    };
    unsigned int bf : 3;
};
typedef struct {
    int m;
} __attribute__((aligned(8))) anon_t;
union val {
    int i;
    void *p;
};
/* struct mystery { int m; }; is prose, not a definition */
EOF
        [ "$extra" -eq 0 ] || echo 'struct extra { int e; };'
    } >"$dir/fx.h"
    i=1
    while [ "$i" -le "$MIN_RECORDS" ]; do
        printf 'struct fill_%02d { int f; };\n' "$i"
        i=$((i + 1))
    done >"$dir/fill.h"
}

fx_expected() {
    cat <<'EOF'
struct inner size 8 align 4
struct inner a 0
struct inner b 4
struct outer size 32 align 8
struct outer x 0
struct outer in 4
struct outer in.a 4
struct outer in.b 8
struct outer u1 16
struct outer u2 16
struct outer bf 24:0-2
typedef anon_t size 4 align 4
typedef anon_t m 0
union val size 8 align 8
union val i 0
union val p 0
EOF
}

# What clang prints for fx_headers, written out by hand in its dump format,
# plus the records the canonicaliser must skip. <variant>: base, size,
# offset, gone, new, or clang18 — base as clang 18 spells a nested anonymous
# record, without its parent's name.
fx_dump() {
    local dir="$1" pad="$2" variant="$3" i inner_size=8 bf=24:0-2
    local fx="$dir/fx.h"
    local anon="outer::(anonymous at $fx:$((10 + pad)):5)" anon_head
    [ "$variant" != size ] || inner_size=12
    [ "$variant" != offset ] || bf=28:0-2
    anon_head="$anon"
    if [ "$variant" = clang18 ]; then
        anon="(anonymous at $fx:$((10 + pad)):5)"
        anon_head="(unnamed at $fx:$((10 + pad)):5)"
    fi
    cat <<EOF

*** Dumping AST Record Layout
         0 | struct __va_list_tag
         0 |   unsigned int gp_offset
         4 |   unsigned int fp_offset
         8 |   void * overflow_arg_area
        16 |   void * reg_save_area
           | [sizeof=24, align=8]

*** Dumping AST Record Layout
         0 | struct (unnamed at /nonexistent/clang/include/__stddef_max_align_t.h:19:9)
         0 |   long long __clang_max_align_nonce1
        16 |   long double __clang_max_align_nonce2
           | [sizeof=32, align=16]

*** Dumping AST Record Layout
         0 | struct inner
         0 |   int a
         4 |   int b
           | [sizeof=$inner_size, align=4]

*** Dumping AST Record Layout
         0 | union $anon_head
         0 |   int u1
         0 |   long u2
           | [sizeof=8, align=8]

*** Dumping AST Record Layout
         0 | struct outer
         0 |   int x
         4 |   struct inner in
         4 |     int a
         8 |     int b
        16 |   union $anon 
        16 |     int u1
        16 |     long u2
$(printf '%10s' "$bf") |   unsigned int bf
           | [sizeof=32, align=8]

*** Dumping AST Record Layout
         0 | struct (unnamed at $fx:$((16 + pad)):9)
         0 |   int m
           | [sizeof=4, align=4]
EOF
    [ "$variant" = gone ] || cat <<'EOF'

*** Dumping AST Record Layout
         0 | union val
         0 |   int i
         0 |   void * p
           | [sizeof=8, align=8]
EOF
    [ "$variant" != new ] || cat <<'EOF'

*** Dumping AST Record Layout
         0 | struct extra
         0 |   int e
           | [sizeof=4, align=4]
EOF
    i=1
    while [ "$i" -le "$MIN_RECORDS" ]; do
        printf '\n*** Dumping AST Record Layout\n         0 | struct fill_%02d\n         0 |   int f\n           | [sizeof=4, align=4]\n' "$i"
        i=$((i + 1))
    done
}

fx_canonical() {
    python3 "$CANONICALISE" "$1" >"$2"
}

expect_same() {
    local label="$1" want="$2" got="$3"
    if cmp -s "$want" "$got"; then
        echo "  $label"
    else
        echo "$SELF --self-test: $label — differs:" >&2
        diff "$want" "$got" | sed 's/^/      /' >&2 || true
        GATE_SELF_TEST_FAIL=1
    fi
}

expect_exit() {
    local label="$1" want="$2" text="$3" out status
    shift 3
    set +e
    out="$("$@" 2>&1)"
    status=$?
    set -e
    if [ "$status" -ne "$want" ] || ! printf '%s\n' "$out" | grep -F -- "$text" >/dev/null; then
        echo "$SELF --self-test: $label — expected exit $want mentioning '$text', got $status:" >&2
        printf '%s\n' "$out" | sed 's/^/      /' >&2
        GATE_SELF_TEST_FAIL=1
        return
    fi
    echo "  $label"
}

self_test() {
    gate_selftest_begin "$SELF"
    local root="$GATE_FIXTURE_ROOT" i

    # --- exports: comparison over synthetic lists --------------------------
    local rec="$root/exports.txt" cur="$root/exports.cur"
    {
        printf '%s\n\n' "$EXPORTS_HEADER"
        {
            i=1
            while [ "$i" -le "$MIN_EXPORTS" ]; do
                printf 'sym%04d func\n' "$i"
                i=$((i + 1))
            done
            echo 'grow object 4'
            echo 'tlsvar tls 8'
        } | LC_ALL=C sort
    } >"$rec"
    grep -v '^#' "$rec" | grep . | LC_ALL=C sort -r >"$cur"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur")"
    gate_expect_silent '.' "an identical export list, reordered, under a comment header"

    grep -v '^sym0001 ' "$cur" >"$cur.1"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur.1")"
    gate_expect removed 1 "a recorded export missing"
    gate_expect added 0

    { cat "$cur"; echo 'newfn func'; } >"$cur.1"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur.1")"
    gate_expect added 1 "an export not yet recorded"
    gate_expect removed 0

    sed 's/^grow object 4$/grow object 8/' "$cur" >"$cur.1"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur.1")"
    gate_expect removed 1 "a data export whose size grew"

    sed 's/^sym0002 func$/sym0002 object 8/' "$cur" >"$cur.1"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur.1")"
    gate_expect removed 1 "a function export that became data"

    grep '^sym000' "$cur" >"$cur.1"
    GATE_FINDINGS="$(scan_exports "$rec" "$cur.1")"
    gate_expect floor 1 "an export list under the floor"

    # --- exports: --record adds, and refuses to drop or rewrite ------------
    local before
    { cat "$cur"; echo 'newfn func'; } | LC_ALL=C sort >"$cur.1"
    cp "$rec" "$rec.orig"
    if record_list "$rec" "$cur.1" "$EXPORTS_HEADER" "$(scan_exports "$rec" "$cur.1")" 2>/dev/null &&
        [ "$(diff "$rec.orig" "$rec" | grep '^[<>]')" = '> newfn func' ] &&
        [ "$(scan_exports "$rec" "$cur.1")" = "" ]; then
        echo "  --record: adds exactly the new export, header kept"
    else
        echo "$SELF --self-test: --record did not add exactly the new export" >&2
        GATE_SELF_TEST_FAIL=1
    fi
    before="$(cksum <"$rec")"
    for change in 's/^sym0003 func$//' 's/^grow object 4$/grow object 8/'; do
        sed "$change" "$cur.1" | grep . >"$cur.2"
        if record_list "$rec" "$cur.2" "$EXPORTS_HEADER" "$(scan_exports "$rec" "$cur.2")" 2>/dev/null ||
            [ "$(cksum <"$rec")" != "$before" ]; then
            echo "$SELF --self-test: --record wrote over a change ($change)" >&2
            GATE_SELF_TEST_FAIL=1
        else
            echo "  --record: refuses, writing nothing ($change)"
        fi
    done

    # --- exports: extraction from a real shared object ---------------------
    local llc lld so="$root/so"
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)" || envfail "no llc"
    lld="$("$SCRIPT_DIR/llvm_tool.sh" rust-lld)" || envfail "no rust-lld"
    mkdir -p "$so"
    cat >"$so/fx.ll" <<'EOF'
target triple = "x86_64-unknown-linux-gnu"
@exported_data = global [24 x i8] zeroinitializer
@exported_tls = thread_local global i64 0
@hidden_data = hidden global i32 0
declare void @undefined_fn()
define void @exported_fn() {
  call void @undefined_fn()
  ret void
}
define internal void @local_fn() {
  ret void
}
EOF
    "$llc" -mtriple=x86_64-unknown-linux-gnu -relocation-model=pic -filetype=obj \
        -o "$so/fx.o" "$so/fx.ll"
    "$lld" -flavor gnu -shared -o "$so/fx.so" "$so/fx.o"
    extract_exports "$so/fx.so" "$so/got"
    printf '%s\n' 'exported_data object 24' 'exported_fn func' 'exported_tls tls 8' >"$so/want"
    expect_same "extraction: a real .so yields its function, data and TLS exports, not its undefined, hidden or local symbols" \
        "$so/want" "$so/got"

    # --- layouts: canonicalisation of a synthetic dump ---------------------
    local inc="$root/inc" moved="$root/moved" grown="$root/grown" lay="$root/lay"
    mkdir -p "$lay"
    fx_headers "$inc" 0 0
    fx_headers "$moved" 3 0
    fx_headers "$grown" 0 1
    {
        fx_expected
        i=1
        while [ "$i" -le "$MIN_RECORDS" ]; do
            printf 'struct fill_%02d size 4 align 4\nstruct fill_%02d f 0\n' "$i" "$i"
            i=$((i + 1))
        done
    } | awk '{ k = $1 " " $2; print k "\t" NR "\t" $0 }' |
        LC_ALL=C sort -t "$(printf '\t')" -k1,1 -k2,2n | cut -f3- >"$lay/want"
    fx_dump "$inc" 0 base | fx_canonical "$inc" "$lay/base" ||
        GATE_SELF_TEST_FAIL=1
    expect_same "canonical form: nested path, anonymous union, bit-field, typedef name; builtin and resource-dir records skipped" \
        "$lay/want" "$lay/base"
    fx_dump "$inc" 0 clang18 | fx_canonical "$inc" "$lay/c18" ||
        GATE_SELF_TEST_FAIL=1
    expect_same "canonical form: clang 18's heading for a nested anonymous record" \
        "$lay/want" "$lay/c18"
    { fx_dump "$inc" 0 base; printf '\n*** Dumping AST Record Layout\n         0 | struct mystery\n         0 |   int m\n           | [sizeof=4, align=4]\n'; } >"$lay/mystery.dump"
    expect_exit "canonical form: a named record no header defines is an error, not a skip" 2 \
        "defined in no header" sh -c 'python3 "$1" "$2" <"$3"' sh "$CANONICALISE" "$inc" "$lay/mystery.dump"
    { printf '%s\n\n' "$LAYOUTS_HEADER"; cat "$lay/base"; } >"$lay/rec"

    fx_dump "$moved" 3 base | fx_canonical "$moved" "$lay/cur"
    GATE_FINDINGS="$(scan_layouts "$lay/rec" "$lay/cur")"
    gate_expect_silent '.' "headers whose line numbers moved without changing a layout"

    for variant in size offset; do
        fx_dump "$inc" 0 "$variant" | fx_canonical "$inc" "$lay/cur"
        GATE_FINDINGS="$(scan_layouts "$lay/rec" "$lay/cur")"
        gate_expect layout 1 "one changed record $variant"
    done
    fx_dump "$inc" 0 gone | fx_canonical "$inc" "$lay/cur"
    GATE_FINDINGS="$(scan_layouts "$lay/rec" "$lay/cur")"
    gate_expect gone 1 "a recorded record no longer defined"
    fx_dump "$grown" 0 new | fx_canonical "$grown" "$lay/new"
    GATE_FINDINGS="$(scan_layouts "$lay/rec" "$lay/new")"
    gate_expect new 1 "a header record not yet recorded"
    gate_expect layout 0
    grep -v '^struct fill_' "$lay/base" >"$lay/cur"
    GATE_FINDINGS="$(scan_layouts "$lay/rec" "$lay/cur")"
    gate_expect floor 1 "a layout list under the floor"

    cp "$lay/rec" "$lay/rec.orig"
    if record_list "$lay/rec" "$lay/new" "$LAYOUTS_HEADER" "$(scan_layouts "$lay/rec" "$lay/new")" &&
        [ "$(diff "$lay/rec.orig" "$lay/rec" | grep -c '^>')" -eq 2 ] &&
        [ "$(diff "$lay/rec.orig" "$lay/rec" | grep -c '^<')" -eq 0 ] &&
        [ "$(scan_layouts "$lay/rec" "$lay/new")" = "" ]; then
        echo "  --record: adds exactly the new record's lines"
    else
        echo "$SELF --self-test: --record did not add exactly the new record" >&2
        GATE_SELF_TEST_FAIL=1
    fi
    before="$(cksum <"$lay/rec")"
    for variant in offset gone; do
        fx_dump "$inc" 0 "$variant" | fx_canonical "$inc" "$lay/cur"
        if record_list "$lay/rec" "$lay/cur" "$LAYOUTS_HEADER" "$(scan_layouts "$lay/rec" "$lay/cur")" 2>/dev/null ||
            [ "$(cksum <"$lay/rec")" != "$before" ]; then
            echo "$SELF --self-test: --record wrote over a $variant change" >&2
            GATE_SELF_TEST_FAIL=1
        else
            echo "  --record: refuses, writing nothing (record $variant)"
        fi
    done

    # --- layouts: this clang's real dump, end to end -----------------------
    resolve_clang
    if [ -n "$CLANG" ]; then
        dump_layouts "$inc" "$lay/clang"
        expect_same "clang extraction: $("$CLANG" --version | sed -n 1p) parses to the expected canonical lines" \
            "$lay/want" "$lay/clang"
        dump_layouts "$moved" "$lay/clang.moved"
        expect_same "clang extraction: moved line numbers give the same canonical lines" \
            "$lay/want" "$lay/clang.moved"
    else
        echo "  clang extraction: skipped — $CLANG_NOTE"
    fi

    # --- command line ------------------------------------------------------
    expect_exit "cli: an unknown argument is a usage error" 2 "usage" "$0" --bogus
    expect_exit "cli: two libraries is a usage error" 2 "usage" "$0" a.so b.so
    expect_exit "cli: no library is a skip" 0 "skipped — no $root/absent.so" "$0" "$root/absent.so"
    expect_exit "cli: no library under --require is an error" 2 "no $root/absent.so" \
        "$0" --require "$root/absent.so"

    gate_selftest_end
}

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------

REQUIRE=0
RECORD=0
LIBC=""
SELF_TEST=0
for arg in "$@"; do
    case "$arg" in
        --require) REQUIRE=1 ;;
        --record) RECORD=1 ;;
        --self-test) SELF_TEST=1 ;;
        -*) usage ;;
        *) [ -z "$LIBC" ] || usage; LIBC="$arg" ;;
    esac
done
if [ "$SELF_TEST" -eq 1 ]; then
    [ "$#" -eq 1 ] || usage
    self_test
fi
# Recording half a measurement would write a list that later reads as removal.
[ "$RECORD" -eq 0 ] || REQUIRE=1
LIBC="${LIBC:-$REPO_ROOT/builddir/libc.so}"

if [ ! -f "$LIBC" ]; then
    [ "$REQUIRE" -eq 0 ] || envfail "no $LIBC"
    echo "$SELF: skipped — no $LIBC"
    exit 0
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT INT TERM
EXPORTS="$DATA_DIR/exports.txt"
LAYOUTS="$DATA_DIR/layouts.txt"
if [ "$RECORD" -eq 0 ]; then
    [ -f "$EXPORTS" ] || envfail "no $EXPORTS"
    [ -f "$LAYOUTS" ] || envfail "no $LAYOUTS"
fi

# Only --record reaches here without the lists; it starts them empty.
[ -f "$EXPORTS" ] || EXPORTS_REC=/dev/null
[ -f "$LAYOUTS" ] || LAYOUTS_REC=/dev/null

extract_exports "$LIBC" "$WORK/exports"
FINDINGS="$(scan_exports "${EXPORTS_REC:-$EXPORTS}" "$WORK/exports")"
summary="$(grep -c . "$WORK/exports") exports"

resolve_clang
LAYOUT_FINDINGS=""
if [ -n "$CLANG" ]; then
    dump_layouts "$INCLUDE_DIR" "$WORK/layouts"
    LAYOUT_FINDINGS="$(scan_layouts "${LAYOUTS_REC:-$LAYOUTS}" "$WORK/layouts")"
    summary="$summary, $(record_count "$WORK/layouts") header records"
elif [ "$REQUIRE" -eq 1 ]; then
    envfail "no clang for the layout half: $CLANG_NOTE"
else
    echo "$SELF: note: header layouts not checked — $CLANG_NOTE"
fi

if [ "$RECORD" -eq 1 ]; then
    mkdir -p "$DATA_DIR"
    # Both lists are judged before either is written.
    status=0
    blocking="$(printf '%s\n%s\n' "$FINDINGS" "$LAYOUT_FINDINGS" | grep -Ev '^(added|new)	|^$' || true)"
    if [ -n "$blocking" ]; then
        report "$blocking"
        echo "$SELF: --record refuses: it only adds. A removal or change is a hand edit to" >&2
        echo "  scripts/gates/libc/, for review to see." >&2
        exit 1
    fi
    record_list "$EXPORTS" "$WORK/exports" "$EXPORTS_HEADER" "$FINDINGS" || status=1
    record_list "$LAYOUTS" "$WORK/layouts" "$LAYOUTS_HEADER" "$LAYOUT_FINDINGS" || status=1
    echo "$SELF: recorded $(printf '%s\n' "$FINDINGS" | grep -c '^added	' || true) new exports," \
        "$(printf '%s\n' "$LAYOUT_FINDINGS" | grep -c '^new	' || true) new records ($summary)"
    exit "$status"
fi

ALL="$(printf '%s\n%s\n' "$FINDINGS" "$LAYOUT_FINDINGS" | grep . || true)"
if [ -n "$ALL" ]; then
    report "$ALL"
    cat >&2 <<'MSG'

  libc.so is the interface programs outside the base link against: it only
  grows. Restore what was removed or changed; record an addition with
  `scripts/check_libc_exports.sh --record`. A deliberate break is a hand edit
  to scripts/gates/libc/ that says why in the commit.
MSG
    exit 1
fi
echo "$SELF: OK — $summary held to scripts/gates/libc/"
