#!/usr/bin/env bash
set -euo pipefail

# Hold the C library to a licence every program may link it under.
#
# `libc.so`, `libc.a`, `crt0.o` and `libbuiltins.a` are built from the four
# packages in ROOTS and every crate they depend on except through
# dev-dependencies; build-dependencies count, since a build script's output is
# compiled in. A GPL-2.0-only program links the library, so one dependency on a
# GPL-3.0-or-later crate leaves git undistributable while everything still
# builds. Each crate `cargo metadata` resolves there must carry an SPDX
# expression MIT alone satisfies, the licence `slibc/NOTICE` gives such crates;
# a third-party one needs an entry there, and one of this tree must live under
# SOURCES. The walk must reach the crates in REACHED, and no file under SOURCES
# may include one from outside them.
#
# `core`, `alloc` and `compiler_builtins` come from the standard library's own
# workspace, which `cargo metadata` does not describe; `slibc/NOTICE` records
# them by hand. `slibc/build.rs` reads `toolchain/libc/`, the `libc` fork, which
# is `MIT OR Apache-2.0` like the library.
#
# Usage: check_libc_license.sh [--self-test]

SELF="check_libc_license"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

ROOTS=(slopos-slibc-cdylib slopos-slibc-staticlib slopos-crt0 slopos-slibc-builtins)
REACHED=(slopos-slibc slopos-slibc-core slopos-abi)
SOURCES=(slibc slibc-core abi)
MAX_TOKENS=256

MODE="${1:-}"
case "$MODE" in
    "" | --self-test) ;;
    *) echo "usage: $SELF.sh [--self-test]" >&2; exit 2 ;;
esac
for tool in jq python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$SELF: $tool is required" >&2; exit 2; }
done

# SPDX operators are all upper or all lower case; identifiers match in any.
operator() {
    case "${TOKENS[POS]:-}" in
        "$1" | "${1,,}") return 0 ;;
    esac
    return 1
}

spdx_id() {
    [[ "$1" =~ ^(DocumentRef-[A-Za-z0-9.-]+:)?[A-Za-z0-9][A-Za-z0-9.-]*\+?$ ]] &&
        [[ ! "$1" =~ ^(AND|OR|WITH|and|or|with)$ ]]
}

# Recursive descent over TOKENS from POS, leaving 1 or 0 in MET; a nonzero
# return is an expression SPDX does not describe.
parse_or() {
    local any
    parse_and || return 1
    any="$MET"
    while operator OR; do
        POS=$((POS + 1))
        parse_and || return 1
        [ "$MET" -eq 0 ] || any=1
    done
    MET="$any"
}

parse_and() {
    local all
    parse_term || return 1
    all="$MET"
    while operator AND; do
        POS=$((POS + 1))
        parse_term || return 1
        [ "$MET" -eq 1 ] || all=0
    done
    MET="$all"
}

parse_term() {
    local term="${TOKENS[POS]:-}"
    POS=$((POS + 1))
    if [ "$term" = "(" ]; then
        parse_or || return 1
        [ "${TOKENS[POS]:-}" = ")" ] || return 1
        POS=$((POS + 1))
        return 0
    fi
    spdx_id "$term" || return 1
    MET=0
    if operator WITH; then
        spdx_id "${TOKENS[POS + 1]:-}" || return 1
        POS=$((POS + 2))
    elif [ "${term^^}" = MIT ]; then
        MET=1
    fi
}

# 0 when MIT alone satisfies the expression, 1 when it does not, 2 when it is
# not an SPDX expression.
mit_satisfies() {
    local spaced="${1//[$'\t\n']/ }"
    spaced="${spaced//"("/ ( }"
    spaced="${spaced//")"/ ) }"
    read -r -a TOKENS <<<"$spaced"
    [ "${#TOKENS[@]}" -le "$MAX_TOKENS" ] || return 2
    POS=0
    parse_or || return 2
    [ "$POS" -eq "${#TOKENS[@]}" ] || return 2
    [ "$MET" -eq 1 ] || return 1
}

findings() {
    local meta="$1" notice="$2" names dirs closure name version license member outside rc
    names="$(printf '%s\n' "${ROOTS[@]}" | jq -R . | jq -s .)" || return 1
    dirs="$(printf '%s\n' "${SOURCES[@]}" | jq -R . | jq -s .)" || return 1
    jq -r --argjson roots "$names" '
        . as $m | $roots[] | select(. as $r | [$m.packages[] | select(.name == $r)] | length == 0)
        | "missing \(.)"
    ' "$meta" || return 1
    closure="$(jq -r --argjson roots "$names" --argjson dirs "$dirs" '
        .workspace_members as $members
        | .workspace_root as $ws
        | [ $dirs[] | "\($ws)/\(.)/" ] as $inside
        | (.packages | map({ key: .id, value: . }) | from_entries) as $pkg
        | ( [ .resolve.nodes[]
              | { key: .id,
                  value: [ .deps[] | select(any(.dep_kinds[]; .kind != "dev")) | .pkg ] } ]
            | from_entries ) as $adj
        | { seen: {},
            frontier: [ .packages[] | select(.name as $n | $roots | index($n)) | .id ] }
        | until( (.frontier | length) == 0;
            .frontier[0] as $cur
            | .frontier |= .[1:]
            | if .seen[$cur] then . else .seen[$cur] = true | .frontier += ($adj[$cur] // []) end )
        | .seen | keys[]
        | . as $id | $pkg[$id]
        | ($members | index($id) != null) as $member
        | ( [ .manifest_path, .targets[]?.src_path ]
            | any(. as $path | $inside | any(. as $dir | $path | startswith($dir)) | not) ) as $outside
        | [ .name, .version, $member, ($member and $outside), (.license // "") ] | @tsv
    ' "$meta")" || return 1

    for name in "${REACHED[@]}"; do
        grep -q "^$name	" <<<"$closure" || echo "unreached $name"
    done
    while IFS=$'\t' read -r name version member outside license; do
        [ -n "$name" ] || continue
        [ "$outside" = false ] || echo "outside $name $version"
        if [ -z "$license" ]; then
            echo "unlicensed $name $version"
        else
            rc=0
            mit_satisfies "$license" || rc=$?
            case "$rc" in
                1) echo "refused $name $version $license" ;;
                2) echo "malformed $name $version $license" ;;
            esac
        fi
        if [ "$member" = false ] && ! grep -q "^- $name: " "$notice"; then
            echo "unnoticed $name $version"
        fi
    done <<<"$closure"
    echo "checked $(grep -c . <<<"$closure")"
}

# Every `include!`, `include_str!`, `include_bytes!` and attribute `path =`
# under SOURCES, read past comments and literals, must be a literal naming a
# file there, a `.rs` one where it is code, or one file in `OUT_DIR`; a symlink
# there is refused. Spellings are read, not expansions: a macro that assembles
# an `include!` or a `path` attribute from its arguments goes unseen.
read -r -d '' INCLUDE_SCAN <<'PYTHON' || true
import os
import re
import sys

root = os.path.realpath(sys.argv[1])
sources = [os.path.join(root, name) for name in sys.argv[2:]]
RAW = re.compile(r'[bc]?r(#*)"')
STR = re.compile(r'[bc]?"((?:[^"\\]|\\.)*)"', re.S)
CHAR = re.compile(r"b?'(?:[^'\\\n]|\\.[^'\n]*)'")
NUMBER = re.compile(r"[0-9][0-9A-Za-z_]*")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
OUT_FILE = re.compile(r"/[A-Za-z0-9_-][A-Za-z0-9_.-]*")
CLOSE = {"(": ")", "[": "]", "{": "}"}
MACROS = {"include", "include_str", "include_bytes"}
OUT_DIR = [("ident", "concat"), ("punct", "!"), ("punct", "("), ("ident", "env"), ("punct", "!"),
           ("punct", "("), ("lit", "OUT_DIR"), ("punct", ")"), ("punct", ",")]
OUT_DIR_CLOSE = ([("punct", ")")], [("punct", ","), ("punct", ")")])


def tokens(src):
    i, line, n = 0, 1, len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            line += c == "\n"
            i += 1
        elif src.startswith("//", i):
            end = src.find("\n", i)
            i = n if end < 0 else end
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    line += src[i] == "\n"
                    i += 1
        elif m := RAW.match(src, i):
            end = src.find('"' + m.group(1), m.end())
            end = n if end < 0 else end
            yield "lit", src[m.end():end], line
            line += src.count("\n", i, end)
            i = end + 1 + len(m.group(1))
        elif m := STR.match(src, i):
            yield "lit", None if "\\" in m.group(1) else m.group(1), line
            line += m.group(0).count("\n")
            i = m.end()
        elif m := CHAR.match(src, i) or NUMBER.match(src, i):
            i = m.end()
        elif m := IDENT.match(src, i):
            yield "ident", m.group(0), line
            i = m.end()
        else:
            yield "punct", c, line
            i += 1


def group(toks, i):
    if i >= len(toks) or toks[i][0] != "punct" or toks[i][1] not in CLOSE:
        return None
    want = []
    for j in range(i, len(toks)):
        kind, text, _ = toks[j]
        if kind == "punct" and text in CLOSE:
            want.append(CLOSE[text])
        elif kind == "punct" and text in ")]}":
            if not want or want.pop() != text:
                return None
            if not want:
                return toks[i + 1:j]
    return None


def shape(toks):
    return [(kind, text) for kind, text, _ in toks]


def out_dir_form(args):
    n = len(OUT_DIR)
    return (len(args) > n and shape(args[:n]) == OUT_DIR and args[n][0] == "lit"
            and OUT_FILE.fullmatch(args[n][1] or "") is not None
            and shape(args[n + 1:]) in OUT_DIR_CLOSE)


def scan(path, rel):
    with open(path, encoding="utf-8", errors="replace") as f:
        toks = list(tokens(f.read()))
    for i, (kind, text, line) in enumerate(toks):
        where = f"{rel}:{line}"
        if kind == "ident" and text in MACROS and i + 2 < len(toks) and toks[i + 1][1] == "!" \
                and toks[i + 2][1] in CLOSE:
            args = group(toks, i + 2)
            if args is not None and len(args) == 1 and args[0][0] == "lit" and args[0][1] is not None:
                yield target(path, where, args[0][1], text == "include")
            elif not out_dir_form(args or []):
                yield f"opaque {where} {text}!"
        elif kind == "punct" and text == "#":
            j = i + 1 + (i + 1 < len(toks) and toks[i + 1][1] == "!")
            attr = group(toks, j) if j < len(toks) and toks[j][1] == "[" else None
            for k, (kind2, text2, _) in enumerate(attr or []):
                if kind2 == "ident" and text2 == "path" and k + 1 < len(attr) and attr[k + 1][1] == "=":
                    value = attr[k + 2] if k + 2 < len(attr) else None
                    if value and value[0] == "lit" and value[1] is not None:
                        yield target(path, where, value[1], True)
                    else:
                        yield f"opaque {where} path"


def target(path, where, literal, code):
    resolved = os.path.realpath(os.path.join(os.path.dirname(path), literal))
    shown = os.path.relpath(resolved, root) if resolved.startswith(root + os.sep) else resolved
    if not any(resolved.startswith(source + os.sep) for source in sources):
        return f"included {where} {shown}"
    if code and not resolved.endswith(".rs"):
        return f"unread {where} {shown}"
    return None


findings = []
for source in sources:
    if not os.path.isdir(source) or os.path.islink(source):
        sys.exit(f"{os.path.relpath(source, root)} is not a directory")
    for directory, dirs, files in os.walk(source):
        for name in sorted(dirs + files):
            path = os.path.join(directory, name)
            rel = os.path.relpath(path, root)
            if os.path.islink(path):
                findings.append(f"linked {rel}")
            elif name.endswith(".rs") and os.path.isfile(path):
                findings.extend(f for f in scan(path, rel) if f)
print("\n".join(sorted(findings)))
PYTHON

included() {
    python3 -c "$INCLUDE_SCAN" "$1" "${SOURCES[@]}"
}

self_test() {
    local fail=0
    SELF_TEST_DIR="$(mktemp -d)"
    trap 'rm -rf "$SELF_TEST_DIR"' EXIT
    local fixture="$SELF_TEST_DIR/metadata.json" edited="$SELF_TEST_DIR/edited.json"
    local notice="$SELF_TEST_DIR/NOTICE" tree="$SELF_TEST_DIR/tree"
    echo "$SELF: self-test against built-in fixtures"

    printf -- '- libm: Copyright\n- unwinding: Gary Guo\n' >"$notice"
    jq -n '
        def pkg($id; $name; $dir; $license):
            { id: $id, name: $name, version: "1.0.0", license: $license,
              manifest_path: "\($dir)/Cargo.toml", targets: [ { src_path: "\($dir)/src/lib.rs" } ] };
        def dep($pkg; $kind): { pkg: $pkg, dep_kinds: [ { kind: $kind } ] };
        { workspace_root: "/ws",
          packages: [
              pkg("c"; "slopos-slibc-cdylib"; "/ws/slibc/cdylib"; "MIT OR Apache-2.0"),
              pkg("s"; "slopos-slibc-staticlib"; "/ws/slibc/staticlib"; "MIT OR Apache-2.0"),
              pkg("0"; "slopos-crt0"; "/ws/slibc/crt0"; "MIT OR Apache-2.0"),
              pkg("b"; "slopos-slibc-builtins"; "/ws/slibc/builtins"; "MIT OR Apache-2.0"),
              pkg("l"; "slopos-slibc"; "/ws/slibc"; "MIT OR Apache-2.0"),
              pkg("r"; "slopos-slibc-core"; "/ws/slibc-core"; "MIT OR Apache-2.0"),
              pkg("a"; "slopos-abi"; "/ws/abi"; "MIT OR Apache-2.0"),
              pkg("m"; "libm"; "/registry/libm-1.0.0"; "MIT"),
              pkg("u"; "unwinding"; "/ws/vendor/unwinding"; "MIT OR Apache-2.0"),
              pkg("g"; "helper"; "/ws/helper"; "GPL-3.0-or-later"),
              pkg("k"; "kernel"; "/ws/kernel"; "GPL-3.0-or-later") ],
          workspace_members: [ "c", "s", "0", "b", "l", "r", "a", "g", "k" ],
          resolve: { nodes: [
              { id: "c", deps: [ dep("l"; null) ] },
              { id: "s", deps: [ dep("l"; null) ] },
              { id: "0", deps: [] },
              { id: "b", deps: [] },
              { id: "l", deps: [ dep("r"; null), dep("a"; null), dep("m"; null),
                                 dep("u"; null), dep("g"; "dev") ] },
              { id: "r", deps: [] },
              { id: "a", deps: [] },
              { id: "m", deps: [] },
              { id: "u", deps: [] },
              { id: "g", deps: [] },
              { id: "k", deps: [ dep("a"; null), dep("g"; null) ] } ] } }
    ' >"$fixture"

    report() {
        local label="$1" want="$2" got="$3"
        if [ "$got" = "$want" ]; then
            echo "  ok: $label"
        else
            echo "  FAIL: $label" >&2
            echo "    want: $want" >&2
            echo "    got:  $got" >&2
            fail=1
        fi
    }
    expect() {
        local got
        jq "$3" "$fixture" >"$edited"
        got="$(findings "$edited" "$notice" 2>/dev/null)" || got="failed"
        report "$1" "$2" "$got"
    }
    license() {
        printf '(.packages[] | select(.name == "%s") | .license) = %s' "$1" "$2"
    }
    depend() {
        printf '(.resolve.nodes[] | select(.id == "l") | .deps) += [ { pkg: "g", dep_kinds: [ { kind: %s } ] } ]' "$1"
    }
    local nl=$'\n'

    expect "a closure MIT satisfies; a GPL dev-dependency and the kernel's own ignored" "checked 9" "."
    expect "a GPL crate as a normal dependency" \
        "outside helper 1.0.0${nl}refused helper 1.0.0 GPL-3.0-or-later${nl}checked 10" \
        "$(depend null)"
    expect "a GPL crate as a build-dependency" \
        "outside helper 1.0.0${nl}refused helper 1.0.0 GPL-3.0-or-later${nl}checked 10" \
        "$(depend '"build"')"
    expect "an Apache-2.0-only crate" "refused libm 1.0.0 Apache-2.0${nl}checked 9" "$(license libm '"Apache-2.0"')"
    expect "GPL-2.0-only, compatible with git yet copyleft" "refused libm 1.0.0 GPL-2.0-only${nl}checked 9" \
        "$(license libm '"GPL-2.0-only"')"
    expect "ISC, permissive but not what slibc/NOTICE carries" "refused libm 1.0.0 ISC${nl}checked 9" \
        "$(license libm '"ISC"')"
    expect "an AND that needs Apache-2.0" "refused unwinding 1.0.0 MIT AND Apache-2.0${nl}checked 9" \
        "$(license unwinding '"MIT AND Apache-2.0"')"
    expect "MIT with an exception" "refused unwinding 1.0.0 MIT WITH LLVM-exception${nl}checked 9" \
        "$(license unwinding '"MIT WITH LLVM-exception"')"
    expect "the alternatives in the other order" "checked 9" "$(license unwinding '"Apache-2.0 OR MIT"')"
    expect "nested groups" "checked 9" "$(license unwinding '"(MIT OR Apache-2.0) AND (Zlib OR (MIT))"')"
    expect "lowercase operators and identifier" "checked 9" "$(license unwinding '"mit or apache-2.0"')"
    expect "a LicenseRef alternative" "checked 9" "$(license unwinding '"LicenseRef-x OR MIT"')"
    expect "a group no alternative of which MIT satisfies" \
        "refused unwinding 1.0.0 (Apache-2.0 OR GPL-2.0-only) AND MIT${nl}checked 9" \
        "$(license unwinding '"(Apache-2.0 OR GPL-2.0-only) AND MIT"')"
    expect "an unbalanced parenthesis" "malformed unwinding 1.0.0 (MIT${nl}checked 9" "$(license unwinding '"(MIT"')"
    expect "an empty alternative" "malformed unwinding 1.0.0 MIT OR ${nl}checked 9" "$(license unwinding '"MIT OR "')"
    expect "a mixed-case operator" "malformed unwinding 1.0.0 MIT Or Apache-2.0${nl}checked 9" \
        "$(license unwinding '"MIT Or Apache-2.0"')"
    expect "cargo's retired slash syntax" "malformed unwinding 1.0.0 MIT/Apache-2.0${nl}checked 9" \
        "$(license unwinding '"MIT/Apache-2.0"')"
    expect "a crate with only a license-file" "unlicensed unwinding 1.0.0${nl}checked 9" "$(license unwinding null)"
    expect "a third-party crate slibc/NOTICE does not name" "unnoticed unwinder 1.0.0${nl}checked 9" \
        '(.packages[] | select(.name == "unwinding") | .name) = "unwinder"'
    expect "a crate of this tree outside the three directories, though MIT" "outside helper 1.0.0${nl}checked 10" \
        "$(depend null) | $(license helper '"MIT"')"
    expect "a crate root outside the three directories" "outside slopos-slibc 1.0.0${nl}checked 9" \
        '(.packages[] | select(.name == "slopos-slibc") | .targets[0].src_path) = "/ws/userland/src/lib.rs"'
    expect "a renamed root" "missing slopos-crt0${nl}checked 8" \
        '(.packages[] | select(.name == "slopos-crt0") | .name) = "slopos-start"'
    expect "a walk that stops at the roots" \
        "unreached slopos-slibc-core${nl}unreached slopos-abi${nl}checked 5" \
        '(.resolve.nodes[] | select(.id == "l") | .deps) = []'
    expect "metadata with no resolve graph" "failed" 'del(.resolve)'

    mkdir -p "$tree/slibc/src" "$tree/slibc-core/tests" "$tree/abi/src"
    cat >"$tree/slibc/src/lib.rs" <<'EOF'
#[path = "inner.rs"]
mod inner;
include!(concat!(env!("OUT_DIR"), "/pins.rs"));
// include!("../../userland/comment.rs");
/* #[path = "../../userland/block.rs"] */
const S: &str = "include!(\"../../userland/string.rs\")";
const Q: char = '"';
fn f<'a>(x: &'a str) -> &'a str { x }
fn g(include: bool) -> bool { include != true }
EOF
    printf '#[path = "../../slibc/src/inner.rs"]\nmod inner;\n' >"$tree/slibc-core/tests/t.rs"
    scan() {
        local got
        got="$(included "$1" 2>/dev/null)" || got="failed"
        LC_ALL=C sort <<<"$got"
    }
    report "includes that stay inside, and look-alikes in comments, strings and lifetimes" "" "$(scan "$tree")"
    cat >"$tree/slibc/src/out.rs" <<'EOF'
include!(
    "../../userland/c.rs"
);
include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../userland/m.rs"));
#[cfg_attr(unix, path = "../../userland/a.rs")]
mod a;
include! { "../../userland/b.rs" }
const D: &[u8] = include_bytes![r"../../userland/d.rs"];
#[path = "/etc/passwd"]
mod p;
#[doc = include_str!("../../README.md")]
struct R;
const C: &CStr = cr"\"; include!("../../userland/e.rs"); // ";
ignore!(1r"\" // "); include!("../../userland/f.rs");
include!(concat!(env!("OUT_DIR"), "/../../../userland/g.rs"));
#[path = "inner.txt"]
mod txt;
EOF
    ln -s ../../userland/h.rs "$tree/abi/src/h.rs"
    report "every way to name a file outside, and a symlink" "$(LC_ALL=C sort <<EOF
included slibc/src/out.rs:1 userland/c.rs
opaque slibc/src/out.rs:4 include!
included slibc/src/out.rs:5 userland/a.rs
included slibc/src/out.rs:7 userland/b.rs
included slibc/src/out.rs:8 userland/d.rs
included slibc/src/out.rs:9 /etc/passwd
included slibc/src/out.rs:11 README.md
included slibc/src/out.rs:13 userland/e.rs
included slibc/src/out.rs:14 userland/f.rs
opaque slibc/src/out.rs:15 include!
unread slibc/src/out.rs:16 slibc/src/inner.txt
linked abi/src/h.rs
EOF
)" "$(scan "$tree")"
    report "a tree without the C library's directories" "failed" "$(scan "$SELF_TEST_DIR/nowhere")"

    if [ "$fail" -ne 0 ]; then
        echo "$SELF: self-test FAILED" >&2
        exit 1
    fi
    echo "$SELF: self-test OK"
}

if [ "$MODE" = "--self-test" ]; then
    self_test
    exit 0
fi

META="$(mktemp)"
trap 'rm -f "$META"' EXIT
(cd "$REPO_ROOT" && cargo metadata --format-version 1 --locked --all-features) >"$META" ||
    { echo "$SELF: cargo metadata failed" >&2; exit 1; }
out="$(findings "$META" "$REPO_ROOT/slibc/NOTICE")" ||
    { echo "$SELF: cannot read cargo metadata's dependency graph" >&2; exit 1; }
inc="$(included "$REPO_ROOT")" || { echo "$SELF: cannot search ${SOURCES[*]} for includes" >&2; exit 1; }
bad="$(printf '%s\n%s\n' "$(grep -v '^checked ' <<<"$out" || true)" "$inc" | sed '/^$/d')"
if [ -n "$bad" ]; then
    echo "$SELF: the C library takes in what it may not:" >&2
    while read -r kind rest; do
        case "$kind" in
            missing) echo "  $rest: no such package; ROOTS names what the library is built from" >&2 ;;
            unreached) echo "  $rest: not reached from ROOTS; the walk or REACHED is stale" >&2 ;;
            unnoticed) echo "  $rest: no '- ${rest%% *}: ' entry in slibc/NOTICE" >&2 ;;
            included) echo "  ${rest% *} includes ${rest##* }, outside ${SOURCES[*]}" >&2 ;;
            opaque) echo "  ${rest% *}: ${rest##* } takes no path literal to check" >&2 ;;
            unread) echo "  ${rest% *} names ${rest##* }, code this scan does not read" >&2 ;;
            outside) echo "  $rest: a workspace crate with a manifest or a target outside ${SOURCES[*]}" >&2 ;;
            linked) echo "  $rest: a symlink under ${SOURCES[*]}" >&2 ;;
            *) echo "  $kind $rest: MIT alone must satisfy it, or its licence joins slibc/NOTICE and mit_satisfies" >&2 ;;
        esac
    done <<<"$bad"
    exit 1
fi
echo "$SELF: OK — $(sed -n 's/^checked //p' <<<"$out") packages behind libc.so, libc.a, crt0.o and libbuiltins.a, each under a licence MIT satisfies; nothing included from outside ${SOURCES[*]}"
