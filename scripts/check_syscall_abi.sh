#!/usr/bin/env bash
# Hold every syscall number in abi/src/syscall/numbers.rs to Linux x86-64's
# own allocation.
#
# SlopOS numbers its syscalls as Linux x86-64 does, and the point of doing so
# is a single promise: a Linux number carries that call's Linux signature and
# semantics. Break it once — put a different operation at 59, or let 262 drift
# onto `fstat`'s shape — and every later consumer is mis-taught. A statically
# linked binary, a strace-shaped debugger, a libc port: each of them reads the
# number and believes it. Nothing else in the tree notices, because both sides
# of the mistake are this repository's own code agreeing with itself.
#
# So the authority is not in the tree. `scripts/gates/syscall/linux-x86_64.tbl`
# is the `common`/`64` rows of Linux's arch/x86/entry/syscalls/syscall_64.tbl,
# vendored as `<number> <name>` pairs, and this gate joins it against the
# constants by number. A constant named `SYSCALL_FOO` sitting on a number Linux
# gave to `bar` fails, naming both sides.
#
# Calls with no Linux analogue are not squeezed into the allocated space: they
# live at `SYSCALL_PRIVATE_BASE + k`, the discipline ARM takes with
# `__ARM_NR_BASE`.
#
# ---------------------------------------------------------------------------
# The checks
# ---------------------------------------------------------------------------
#
#   name      a `pub const SYSCALL_<NAME>: u64 = <N>;` with a bare `N` below
#             the private base must have `lowercase(<NAME>)` equal to the
#             name the vendored table gives number `N`.
#   range     the same constants must be `<= SYSCALL_LINUX_MAX`, and
#             `SYSCALL_LINUX_MAX` must equal the table's highest number, so
#             the dispatch table is exactly as wide as the allocated space.
#   private   `SYSCALL_PRIVATE_BASE + k` constants run contiguously from 0 —
#             a hole is a dispatch slot answering nothing — and every `k` is
#             inside `SYSCALL_PRIVATE_TABLE_SIZE`.
#   dup       no number and no constant name is defined twice.
#   collide   a private constant whose lowercased name is *also* a Linux
#             syscall name is a failure: it is either a call that belongs on
#             its Linux number, or one whose shape has diverged from Linux's
#             and needs saying so out loud. Saying so is an entry in
#             scripts/gates/syscall/private-allowlist.txt with a reason.
#   dead      an allowlist entry that suppresses nothing, so the file cannot
#             accumulate stale exemptions.
#   floor     a parse that fell over reads as a healthy tree otherwise: fewer
#             than 100 Linux-numbered constants, fewer than 20 private ones,
#             or a table under 300 rows fails outright.
#
# Deliberately accepted, and asserted in the self-test: a `#` comment anywhere
# in the table (including one that looks like a row), doc comments naming a
# constant, `const _: () = assert!(...)` lines, the `usize`/expression metadata
# constants, and an allowlisted private collision.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_syscall_abi "$@"

NUMBERS_RS="abi/src/syscall/numbers.rs"
LINUX_TBL="scripts/gates/syscall/linux-x86_64.tbl"
PRIVATE_ALLOWLIST="scripts/gates/syscall/private-allowlist.txt"

# Floors. Round numbers well under the real counts: they exist to catch a scan
# that parsed nothing, not to ratchet the ABI's size.
MIN_LINUX_CONSTS=100
MIN_PRIVATE_CONSTS=20
MIN_TABLE_ROWS=300

# ---------------------------------------------------------------------------
# The scan. One awk pass: the table and the allowlist are read in BEGIN, the
# constants are collected from the operand and classified in END — the
# metadata constants (`SYSCALL_PRIVATE_BASE` and friends) must be known before
# any constant can be classified, and nothing guarantees they come first.
#
# Findings carry a `<tag>\t` prefix so the self-test can count each check
# independently; the reports strip it back off.
# ---------------------------------------------------------------------------
run_scan() {
    local numbers="$1" table="$2" allowlist="$3"
    awk -v tablefile="$table" -v allowfile="$allowlist" \
        -v min_linux="$MIN_LINUX_CONSTS" -v min_private="$MIN_PRIVATE_CONSTS" \
        -v min_rows="$MIN_TABLE_ROWS" '
    function field(line, idx,   n, f) {
        n = split(line, f, /[ \t]+/)
        if (f[1] == "") idx++
        if (idx > n) return ""
        return f[idx]
    }
    BEGIN {
        rows = 0
        tblmax = -1
        while ((getline line < tablefile) > 0) {
            sub(/#.*$/, "", line)
            if (line !~ /[^ \t]/) continue
            num = field(line, 1)
            nm = field(line, 2)
            if (num !~ /^[0-9]+$/ || nm == "") {
                printf "table\t%s: unparseable row: %s\n", tablefile, line
                continue
            }
            if ((num + 0) in tblname) {
                printf "table\t%s: number %s appears twice (%s, %s)\n", \
                    tablefile, num, tblname[num + 0], nm
                continue
            }
            tblname[num + 0] = nm
            tblnum[nm] = num + 0
            if (num + 0 > tblmax) tblmax = num + 0
            rows++
        }
        close(tablefile)

        while ((getline line < allowfile) > 0) {
            sub(/#.*$/, "", line)
            if (line !~ /[^ \t]/) continue
            nm = field(line, 1)
            if (field(line, 2) == "") {
                printf "dead\t%s: entry \"%s\" states no reason\n", allowfile, nm
                continue
            }
            if (nm in allow) {
                printf "dead\t%s: entry \"%s\" appears twice\n", allowfile, nm
                continue
            }
            allow[nm] = 1
        }
        close(allowfile)
    }

    /^pub const SYSCALL_[A-Z0-9_]+[ \t]*:/ {
        colon = index($0, ":")
        eq = index($0, "=")
        if (eq == 0 || eq < colon) next
        cname = substr($0, 11, colon - 11)
        gsub(/[ \t]/, "", cname)
        ctype = substr($0, colon + 1, eq - colon - 1)
        gsub(/[ \t]/, "", ctype)
        rhs = substr($0, eq + 1)
        sub(/;.*$/, "", rhs)
        gsub(/[ \t]/, "", rhs)

        if (cname == "SYSCALL_LINUX_MAX") { linuxmax = rhs + 0; seen_max = 1; next }
        if (cname == "SYSCALL_PRIVATE_BASE") { privbase = rhs + 0; seen_base = 1; next }
        if (cname == "SYSCALL_PRIVATE_TABLE_SIZE") { privsize = rhs + 0; seen_size = 1; next }
        if (cname == "SYSCALL_TABLE_SIZE" || cname == "SYSCALL_PRIVATE_END") next
        if (ctype != "u64") next

        nconst++
        cn[nconst] = cname
        cv[nconst] = rhs
        cl[nconst] = NR
    }

    END {
        if (!seen_max)
            printf "meta\t%s: SYSCALL_LINUX_MAX is not defined\n", FILENAME
        if (!seen_base)
            printf "meta\t%s: SYSCALL_PRIVATE_BASE is not defined\n", FILENAME
        if (!seen_size)
            printf "meta\t%s: SYSCALL_PRIVATE_TABLE_SIZE is not defined\n", FILENAME
        if (seen_max && linuxmax != tblmax)
            printf "range\t%s: SYSCALL_LINUX_MAX is %d, but Linux has allocated up to %d\n", \
                FILENAME, linuxmax, tblmax

        maxk = -1
        for (i = 1; seen_base && i <= nconst; i++) {
            cname = cn[i]
            rhs = cv[i]
            where = FILENAME ":" cl[i]
            short = tolower(substr(cname, 9))

            if (cname in seen_name) {
                printf "dupname\t%s: %s is already defined at line %d\n", \
                    where, cname, seen_name[cname]
                continue
            }
            seen_name[cname] = cl[i]

            if (rhs ~ /^[0-9]+$/) {
                num = rhs + 0
                if (num >= privbase) {
                    printf "shape\t%s: %s = %d is in the private range; write it as SYSCALL_PRIVATE_BASE + k\n", \
                        where, cname, num
                    continue
                }
                nlinux++
                if (!(num in tblname))
                    printf "name\t%s: %s claims number %d, which Linux x86-64 has not allocated\n", \
                        where, cname, num
                else if (tblname[num] != short)
                    printf "name\t%s: number %d belongs to Linux syscall %s, but %s puts %s there\n", \
                        where, num, tblname[num], cname, short
                if (seen_max && num > linuxmax)
                    printf "range\t%s: %s = %d exceeds SYSCALL_LINUX_MAX (%d)\n", \
                        where, cname, num, linuxmax
                eff = num
            } else if (rhs ~ /^SYSCALL_PRIVATE_BASE\+[0-9]+$/) {
                k = substr(rhs, index(rhs, "+") + 1) + 0
                nprivate++
                if (!(k in privk)) privk[k] = cname
                if (k > maxk) maxk = k
                if (seen_size && k >= privsize)
                    printf "private\t%s: %s is SYSCALL_PRIVATE_BASE + %d, past SYSCALL_PRIVATE_TABLE_SIZE (%d)\n", \
                        where, cname, k, privsize
                if (short in tblnum && !(short in allow))
                    printf "collide\t%s: %s is private, but %s is Linux x86-64 number %d\n", \
                        where, cname, short, tblnum[short]
                if (short in tblnum) allowused[short] = 1
                eff = privbase + k
            } else {
                printf "shape\t%s: %s = %s is neither a bare Linux number nor SYSCALL_PRIVATE_BASE + k\n", \
                    where, cname, rhs
                continue
            }

            if (eff in effowner)
                printf "dup\t%s: %s and %s are both number %d\n", \
                    where, cname, effowner[eff], eff
            else
                effowner[eff] = cname
        }

        for (k = 0; k <= maxk; k++)
            if (!(k in privk))
                printf "private\t%s: SYSCALL_PRIVATE_BASE + %d is unused while + %d is taken by %s\n", \
                    FILENAME, k, maxk, privk[maxk]

        for (nm in allow)
            if (!(nm in allowused))
                printf "dead\t%s: entry \"%s\" exempts nothing\n", allowfile, nm

        if (rows < min_rows)
            printf "floor\t%s: parsed %d rows, expected at least %d — the vendored table looks truncated\n", \
                tablefile, rows, min_rows
        if (nlinux < min_linux)
            printf "floor\t%s: found %d Linux-numbered constants, expected at least %d — the scan did not parse\n", \
                FILENAME, nlinux, min_linux
        if (nprivate < min_private)
            printf "floor\t%s: found %d private constants, expected at least %d — the scan did not parse\n", \
                FILENAME, nprivate, min_private
    }
    ' "$numbers"
}

require_file() {
    local path="$1"
    if [ ! -s "$path" ]; then
        echo "check_syscall_abi: $path is missing or empty — the join would compare" >&2
        echo "  nothing against nothing, so refusing to report OK. Check that this is" >&2
        echo "  the repository root." >&2
        exit 2
    fi
}

# ---------------------------------------------------------------------------
# Self-test
#
# Fixtures are generated, not copied: a self-test that read the real
# numbers.rs would start passing for the wrong reason the day that file
# changed. The synthetic Linux table is `<i> sc<i>`, so the expected name for
# every number is mechanical.
# ---------------------------------------------------------------------------
fixture_table() {
    local path="$1" rows="$2" i=0
    {
        echo "# Fixture table. A '#' line that looks exactly like a row must not"
        echo "# be parsed as one — if it were, the wrong name would land on 5."
        echo "# 5 not_sc5"
        echo ""
        while [ "$i" -lt "$rows" ]; do
            echo "$i sc$i"
            i=$((i + 1))
        done
    } > "$path"
}

fixture_numbers() {
    local path="$1" linuxmax="$2" nlinux="$3" npriv="$4" i=0
    {
        echo "//! Fixture header. A doc comment may name a constant and its value:"
        echo "//! \`pub const SYSCALL_SC1: u64 = 77;\` is prose, not a definition."
        echo "pub const SYSCALL_LINUX_MAX: u64 = $linuxmax;"
        echo "pub const SYSCALL_TABLE_SIZE: usize = (SYSCALL_LINUX_MAX as usize) + 1;"
        echo "pub const SYSCALL_PRIVATE_BASE: u64 = 1024;"
        echo "pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 48;"
        echo "pub const SYSCALL_PRIVATE_END: u64 ="
        echo "    SYSCALL_PRIVATE_BASE + (SYSCALL_PRIVATE_TABLE_SIZE as u64);"
        i=0
        while [ "$i" -lt "$nlinux" ]; do
            echo "/// \`sc$i(arg0)\`."
            echo "pub const SYSCALL_SC$i: u64 = $i;"
            i=$((i + 1))
        done
        i=0
        while [ "$i" -lt "$npriv" ]; do
            echo "pub const SYSCALL_PRIV$i: u64 = SYSCALL_PRIVATE_BASE + $i;"
            i=$((i + 1))
        done
        # An allowlisted private collision: `sc200` is a name the table gives
        # to a Linux number, carried privately on purpose.
        echo "pub const SYSCALL_SC200: u64 = SYSCALL_PRIVATE_BASE + $npriv;"
        echo "const _: () = assert!(SYSCALL_PRIVATE_BASE > SYSCALL_LINUX_MAX);"
        echo "const _: () = assert!("
        echo "    SYSCALL_SC200 < SYSCALL_PRIVATE_END,"
        echo "    \"private syscall range is full\","
        echo ");"
    } > "$path"
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_syscall_abi

    GOOD_TBL="$(gate_fixture good/linux.tbl)"
    GOOD_RS="$(gate_fixture good/numbers.rs)"
    GOOD_ALLOW="$(gate_fixture good/allow.txt)"
    fixture_table "$GOOD_TBL" 400
    fixture_numbers "$GOOD_RS" 399 120 25
    echo "sc200 private until the fixture's struct layout matches Linux's" > "$GOOD_ALLOW"

    GATE_FINDINGS="$(run_scan "$GOOD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect_silent '.' \
        "correctly numbered constants, an allowlisted private collision, a #-commented table row, doc comments naming a constant, and const _ asserts"

    # Wrong number for a name. `sc300` on 301 collides with nothing else, so
    # this fixture isolates the join.
    BAD_RS="$(gate_fixture name/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    echo "pub const SYSCALL_SC300: u64 = 301;" >> "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect name 1 "a constant sitting on another call's Linux number"

    # A number past the top of the allocated space.
    BAD_RS="$(gate_fixture range/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    echo "pub const SYSCALL_SC999: u64 = 999;" >> "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect range 1 "a number above SYSCALL_LINUX_MAX"

    # Two constants on one number, in the private range.
    BAD_RS="$(gate_fixture dup/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    echo "pub const SYSCALL_PRIV_AGAIN: u64 = SYSCALL_PRIVATE_BASE + 3;" >> "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect dup 1 "a duplicate number"

    # The same constant name twice.
    BAD_RS="$(gate_fixture dupname/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    echo "pub const SYSCALL_PRIV3: u64 = SYSCALL_PRIVATE_BASE + 40;" >> "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect dupname 1 "a duplicate constant name"

    # A private constant wearing a Linux syscall's name, unrecorded.
    BAD_RS="$(gate_fixture collide/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    echo "pub const SYSCALL_SC201: u64 = SYSCALL_PRIVATE_BASE + 26;" >> "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect collide 1 "a private constant named after a Linux syscall with no allowlist entry"

    # A hole in the private numbering.
    BAD_RS="$(gate_fixture hole/numbers.rs)"
    sed '/SYSCALL_PRIV5:/d' "$GOOD_RS" > "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect private 1 "an unused private slot below the highest one taken"

    # A private number past the end of the private table.
    BAD_RS="$(gate_fixture overflow/numbers.rs)"
    cp "$GOOD_RS" "$BAD_RS"
    sed -i.bak 's/^pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 48;/pub const SYSCALL_PRIVATE_TABLE_SIZE: usize = 20;/' "$BAD_RS"
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect private 6 "every private number at or past SYSCALL_PRIVATE_TABLE_SIZE"

    # An allowlist entry that exempts nothing, and one with no reason.
    BAD_ALLOW="$(gate_fixture dead/allow.txt)"
    {
        cat "$GOOD_ALLOW"
        echo "sc300 nothing private is called this"
        echo "sc250"
    } > "$BAD_ALLOW"
    GATE_FINDINGS="$(run_scan "$GOOD_RS" "$GOOD_TBL" "$BAD_ALLOW")"
    gate_expect dead 2 "a stale exemption and a reasonless one"

    # A truncated table: the join would still "work", against half of Linux.
    BAD_TBL="$(gate_fixture truncated/linux.tbl)"
    fixture_table "$BAD_TBL" 50
    GATE_FINDINGS="$(run_scan "$GOOD_RS" "$BAD_TBL" "$GOOD_ALLOW")"
    gate_expect floor 1 "a vendored table under the row floor"

    # A numbers.rs the scan barely parsed.
    BAD_RS="$(gate_fixture thin/numbers.rs)"
    fixture_numbers "$BAD_RS" 399 3 2
    GATE_FINDINGS="$(run_scan "$BAD_RS" "$GOOD_TBL" "$GOOD_ALLOW")"
    gate_expect floor 2 "both constant floors"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real run
# ---------------------------------------------------------------------------
cd "$REPO_ROOT"
require_file "$NUMBERS_RS"
require_file "$LINUX_TBL"
require_file "$PRIVATE_ALLOWLIST"

GATE_FINDINGS="$(run_scan "$NUMBERS_RS" "$LINUX_TBL" "$PRIVATE_ALLOWLIST")"

report_tag() {
    local hits
    hits="$(printf '%s\n' "$GATE_FINDINGS" | grep "^$1	" | cut -f2- || true)"
    [ -n "$hits" ] || return 0
    echo "check_syscall_abi: $2" >&2
    printf '%s\n' "$hits" | sed 's/^/    /' >&2
}

if [ -n "$GATE_FINDINGS" ]; then
    report_tag name "a Linux number carries the wrong call:"
    report_tag range "a number is outside the Linux-allocated space:"
    report_tag private "the private range is not a contiguous run inside its table:"
    report_tag dup "a syscall number is defined twice:"
    report_tag dupname "a syscall constant is defined twice:"
    report_tag collide "a private constant is named after a Linux syscall:"
    report_tag dead "the private allowlist has entries that exempt nothing:"
    report_tag shape "a syscall constant's value has an unrecognised form:"
    report_tag meta "the ABI metadata constants could not be read:"
    report_tag table "the vendored Linux table has malformed rows:"
    report_tag floor "the scan found too little to be believed:"
    cat >&2 <<'MSG'

  Numbers below SYSCALL_PRIVATE_BASE are Linux x86-64's, and a Linux number
  promises that call's Linux signature and semantics. Either move the call to
  the number Linux gave it, or give it a private number
  (SYSCALL_PRIVATE_BASE + k, contiguous from 0) and say so in
  scripts/gates/syscall/private-allowlist.txt if its name is one Linux also
  uses. The table itself comes from Linux's syscall_64.tbl; regenerate it with
  the recipe in its header rather than editing it to fit.
MSG
    exit 1
fi

echo "check_syscall_abi: OK — every Linux-numbered constant matches Linux x86-64's"
echo "check_syscall_abi: own allocation, and the private range is contiguous"
