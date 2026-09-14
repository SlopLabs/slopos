#!/usr/bin/env bash
# Filesystem cost ratchet.
#
# Reads the two report lines the suite emits and holds them to
# scripts/gates/fsperf/<variant>.txt:
#
#   FSPERF[<phase>]: bytes=N txns=N commits=N devwrites=N devblocks=N
#                    barriers=N ns=N rawbytes=N rawns=N
#   FSCAP[<phase>]: blocks=N blocksize=N groups=N cacheentries=N mountreads=N
#                   mountns=N dirents=N lookupreads=N bytes=N ns=N files=N
#                   treebytes=N
#
# What is graded and why each quantity is the shape it is:
#
#   Per-MiB *counts* are deterministic. How many transactions a write opens,
#   how many requests those transactions hand the block layer (a vectored
#   write is one, carrying its whole run), how many sectors those requests
#   move, and how many barriers they force are properties of the code, not of
#   the machine — the same ISO reports the same numbers on a laptop and on a
#   CI runner with no accelerator. Those carry caps.
#
#   Throughput is not. An absolute MiB/s floor fails on the unmodified tree on
#   any machine without /dev/kvm, which is the mistake the quota gate already
#   documents. The accelerator-invariant quantity is the ratio of the
#   filesystem's write rate to the *same run's* raw block-device write rate:
#   both move together with the emulator, so their quotient describes the
#   filesystem. That carries a floor.
#
#   Mount cost is graded in device reads per GiB of volume, not in wall time.
#   "A 16 GiB image mounts in bounded time" is a claim about how much I/O the
#   mount issues per unit of volume; reads are countable and deterministic,
#   seconds are neither.
#
#   Every floor (`min-bytes`, `min-volume-gib`, `min-dirents`, `min-files`,
#   `min-treebytes`) exists because a measurement that stopped happening looks
#   exactly like a measurement that got free. A test that silently wrote
#   nothing would otherwise report a perfect cost per MiB, and a volume that
#   stopped being populated would report a free tree walk.
#
#     scripts/check_fs_throughput.sh
#     scripts/check_fs_throughput.sh --log captured-raw.log
#     scripts/check_fs_throughput.sh --log capacity.log --require-capacity
#     scripts/check_fs_throughput.sh --emit-allowlist
#     scripts/check_fs_throughput.sh --self-test
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VARIANT=tests
LOG=""
EMIT=0
SELF_TEST=0
REQUIRE_CAPACITY=0
GATE_DATA_DIR="$REPO_ROOT/scripts/gates/fsperf"
while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --emit-allowlist) EMIT=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --require-capacity) REQUIRE_CAPACITY=1; shift ;;
        --gate-data-dir) GATE_DATA_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,44p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

MIB=1048576

# Matched once and in full: a per-field `sed` echoes its input unchanged on a
# miss, so a renamed field would yield a whole log line where an integer was
# expected.
PERF_RE='^FSPERF\[([a-z0-9-]+)\]: bytes=([0-9]+) txns=([0-9]+) commits=([0-9]+) devwrites=([0-9]+) devblocks=([0-9]+) barriers=([0-9]+) ns=([0-9]+) rawbytes=([0-9]+) rawns=([0-9]+)'
CAP_RE='^FSCAP\[([a-z0-9-]+)\]: blocks=([0-9]+) blocksize=([0-9]+) groups=([0-9]+) cacheentries=([0-9]+) mountreads=([0-9]+) mountns=([0-9]+) dirents=([0-9]+) lookupreads=([0-9]+) bytes=([0-9]+) ns=([0-9]+) files=([0-9]+) treebytes=([0-9]+)'

declare -A P_BYTES=() P_TXNS=() P_COMMITS=() P_DEVW=() P_DEVB=() P_BARRIERS=() P_NS=() P_RAWB=() P_RAWNS=()
declare -A C_BLOCKS=() C_BSIZE=() C_GROUPS=() C_ENTRIES=() C_MREADS=() C_MNS=() C_DIRENTS=() C_LOOKUPR=() C_BYTES=() C_NS=() C_FILES=() C_TREEB=()
PERF_PHASES=()
CAP_PHASES=()

parse_log() {
    local log="$1" line
    while IFS= read -r line; do
        if [[ $line =~ $PERF_RE ]]; then
            local phase="${BASH_REMATCH[1]}"
            [[ " ${PERF_PHASES[*]-} " == *" $phase "* ]] || PERF_PHASES+=("$phase")
            P_BYTES[$phase]="${BASH_REMATCH[2]}"
            P_TXNS[$phase]="${BASH_REMATCH[3]}"
            P_COMMITS[$phase]="${BASH_REMATCH[4]}"
            P_DEVW[$phase]="${BASH_REMATCH[5]}"
            P_DEVB[$phase]="${BASH_REMATCH[6]}"
            P_BARRIERS[$phase]="${BASH_REMATCH[7]}"
            P_NS[$phase]="${BASH_REMATCH[8]}"
            P_RAWB[$phase]="${BASH_REMATCH[9]}"
            P_RAWNS[$phase]="${BASH_REMATCH[10]}"
        elif [[ $line =~ $CAP_RE ]]; then
            local phase="${BASH_REMATCH[1]}"
            [[ " ${CAP_PHASES[*]-} " == *" $phase "* ]] || CAP_PHASES+=("$phase")
            C_BLOCKS[$phase]="${BASH_REMATCH[2]}"
            C_BSIZE[$phase]="${BASH_REMATCH[3]}"
            C_GROUPS[$phase]="${BASH_REMATCH[4]}"
            C_ENTRIES[$phase]="${BASH_REMATCH[5]}"
            C_MREADS[$phase]="${BASH_REMATCH[6]}"
            C_MNS[$phase]="${BASH_REMATCH[7]}"
            C_DIRENTS[$phase]="${BASH_REMATCH[8]}"
            C_LOOKUPR[$phase]="${BASH_REMATCH[9]}"
            C_BYTES[$phase]="${BASH_REMATCH[10]}"
            C_NS[$phase]="${BASH_REMATCH[11]}"
            C_FILES[$phase]="${BASH_REMATCH[12]}"
            C_TREEB[$phase]="${BASH_REMATCH[13]}"
        fi
    done < "$log"
    if [ "${#PERF_PHASES[@]}" -eq 0 ]; then
        echo "check_fs_throughput: no FSPERF lines in $log" >&2
        echo "  The kernel emits them from the fs cost report; a log with none is" >&2
        echo "  a run that never reached the measurement, not a cost-free one." >&2
        exit 1
    fi
}

boot_and_capture() {
    local out="$1"
    just _build-run-tests >/dev/null
    set -o pipefail
    "$REPO_ROOT/builddir/run_tests" --raw --no-color 2>&1 | tee "$out" >/dev/null || true
}

# Ceiling a quarter above the observation, floor a quarter below: the counts are
# deterministic for one ISO, but a legitimate change in test payload moves them,
# and a cap with no slack turns every such change into a gate edit.
pad_up() { echo $(( ($1 * 5 + 3) / 4 + 1 )); }
pad_down() { echo $(( $1 * 3 / 4 )); }

per_mib() {
    local count="$1" bytes="$2"
    [ "$bytes" -gt 0 ] || { echo 0; return; }
    echo $(( (count * MIB + bytes - 1) / bytes ))
}

# Rate quotient in percent, computed as a cross-product so neither side needs a
# division that would round a slow run into a fast one.
fs_over_raw_pct() {
    local bytes="$1" ns="$2" rawbytes="$3" rawns="$4"
    if [ "$ns" -eq 0 ] || [ "$rawbytes" -eq 0 ]; then echo 0; return; fi
    echo $(( (bytes * rawns * 100) / (ns * rawbytes) ))
}

emit_allowlist() {
    cat <<EOF
# check_fs_throughput gate data — variant: $VARIANT
#
#     scripts/check_fs_throughput.sh --variant $VARIANT --emit-allowlist
#
# Counts per MiB are deterministic for one ISO and carry caps. The
# filesystem-over-raw write-rate ratio is the only quantity here that is
# invariant under a change of accelerator, so it is the only rate graded.
# Floors exist so a measurement that stopped happening cannot read as free.
#
# A cap matching nothing is a dead entry and fails the gate.
EOF
    local phase
    for phase in "${PERF_PHASES[@]}"; do
        local bytes="${P_BYTES[$phase]}"
        echo
        echo "# ${phase}: ${bytes} bytes written, ${P_TXNS[$phase]} transactions,"
        echo "# ${P_DEVW[$phase]} device write requests carrying ${P_DEVB[$phase]} blocks,"
        echo "# ${P_BARRIERS[$phase]} barriers; fs $(( bytes / 1024 ))KiB in ${P_NS[$phase]}ns against"
        echo "# raw $(( P_RAWB[$phase] / 1024 ))KiB in ${P_RAWNS[$phase]}ns."
        printf 'max-txns-per-mib\t%s\t%s\n' "$phase" "$(pad_up "$(per_mib "${P_TXNS[$phase]}" "$bytes")")"
        printf 'max-commits-per-mib\t%s\t%s\n' "$phase" "$(pad_up "$(per_mib "${P_COMMITS[$phase]}" "$bytes")")"
        printf 'max-devwrites-per-mib\t%s\t%s\n' "$phase" "$(pad_up "$(per_mib "${P_DEVW[$phase]}" "$bytes")")"
        printf 'max-devblocks-per-mib\t%s\t%s\n' "$phase" "$(pad_up "$(per_mib "${P_DEVB[$phase]}" "$bytes")")"
        printf 'max-barriers-per-mib\t%s\t%s\n' "$phase" "$(pad_up "$(per_mib "${P_BARRIERS[$phase]}" "$bytes")")"
        printf 'min-fs-over-raw-pct\t%s\t%s\n' "$phase" \
            "$(pad_down "$(fs_over_raw_pct "$bytes" "${P_NS[$phase]}" "${P_RAWB[$phase]}" "${P_RAWNS[$phase]}")")"
        printf 'min-bytes\t%s\t%s\n' "$phase" "$(pad_down "$bytes")"
    done
    for phase in "${CAP_PHASES[@]-}"; do
        [ -n "$phase" ] || continue
        local gib=$(( C_BLOCKS[$phase] / (1024 * 1024 * 1024 / C_BSIZE[$phase]) ))
        [ "$gib" -gt 0 ] || gib=1
        echo
        echo "# ${phase}: ${C_BLOCKS[$phase]} blocks of ${C_BSIZE[$phase]} (${gib} GiB) in"
        echo "# ${C_GROUPS[$phase]} groups, mounted with ${C_ENTRIES[$phase]} cache entries in"
        echo "# ${C_MREADS[$phase]} device reads / ${C_MNS[$phase]}ns; ${C_DIRENTS[$phase]} names in one"
        echo "# directory, a lookup of the last costing ${C_LOOKUPR[$phase]} block reads;"
        echo "# ${C_FILES[$phase]} regular files totalling ${C_TREEB[$phase]} bytes walked back off it."
        printf 'max-mount-reads-per-gib\t%s\t%s\n' "$phase" "$(pad_up $(( C_MREADS[$phase] / gib )))"
        printf 'max-lookup-reads\t%s\t%s\n' "$phase" "$(pad_up "${C_LOOKUPR[$phase]}")"
        printf 'min-volume-gib\t%s\t%s\n' "$phase" "$gib"
        printf 'min-dirents\t%s\t%s\n' "$phase" "$(pad_down "${C_DIRENTS[$phase]}")"
        printf 'min-files\t%s\t%s\n' "$phase" "$(pad_down "${C_FILES[$phase]}")"
        printf 'min-treebytes\t%s\t%s\n' "$phase" "$(pad_down "${C_TREEB[$phase]}")"
    done
}

FAIL=0
fail() { echo "FAIL: $*" >&2; FAIL=1; }

# A gate file the parser cannot read is not a cost that regressed: it exits 2
# and skips the re-measure advice, which is what the sibling gates do with an
# unparseable directive.
gate_error() { echo "check_fs_throughput: $*" >&2; exit 2; }

check_against() {
    local gate="$1" line key phase want got lineno=0
    declare -A MATCHED=() SKIPPED_CAPACITY=()
    while IFS= read -r line; do
        lineno=$((lineno + 1))
        line="${line%%#*}"
        # shellcheck disable=SC2086
        set -- $line
        [ "$#" -ne 0 ] || continue
        # A row with a field missing or spare grades nothing while the file
        # says it does, and the "graded nothing" guard below cannot see it as
        # long as one other row matched.
        [ "$#" -eq 3 ] || gate_error "$gate:$lineno: want '<key> <phase> <value>', got $# fields: $line"
        key="$1"; phase="$2"; want="$3"
        case "$key" in
            max-txns-per-mib|max-commits-per-mib|max-devwrites-per-mib|max-devblocks-per-mib|max-barriers-per-mib|min-fs-over-raw-pct|min-bytes)
                if [ -z "${P_BYTES[$phase]:-}" ]; then
                    fail "$key names phase '$phase', which reported no FSPERF line (dead entry)"
                    continue
                fi
                MATCHED[$key/$phase]=1
                case "$key" in
                    max-txns-per-mib)     got="$(per_mib "${P_TXNS[$phase]}" "${P_BYTES[$phase]}")" ;;
                    max-commits-per-mib)  got="$(per_mib "${P_COMMITS[$phase]}" "${P_BYTES[$phase]}")" ;;
                    max-devwrites-per-mib) got="$(per_mib "${P_DEVW[$phase]}" "${P_BYTES[$phase]}")" ;;
                    max-devblocks-per-mib) got="$(per_mib "${P_DEVB[$phase]}" "${P_BYTES[$phase]}")" ;;
                    max-barriers-per-mib) got="$(per_mib "${P_BARRIERS[$phase]}" "${P_BYTES[$phase]}")" ;;
                    min-fs-over-raw-pct)  got="$(fs_over_raw_pct "${P_BYTES[$phase]}" "${P_NS[$phase]}" "${P_RAWB[$phase]}" "${P_RAWNS[$phase]}")" ;;
                    min-bytes)            got="${P_BYTES[$phase]}" ;;
                esac
                ;;
            max-mount-reads-per-gib|max-lookup-reads|min-volume-gib|min-dirents|min-files|min-treebytes)
                if [ -z "${C_BLOCKS[$phase]:-}" ]; then
                    if [ "$REQUIRE_CAPACITY" = "1" ]; then
                        fail "$key names phase '$phase', which reported no FSCAP line"
                    else
                        if [ -z "${SKIPPED_CAPACITY[$phase]:-}" ]; then
                            SKIPPED_CAPACITY[$phase]=1
                            echo "check_fs_throughput: no FSCAP[$phase] in this run — capacity volume not attached, skipping its rows"
                        fi
                    fi
                    continue
                fi
                MATCHED[$key/$phase]=1
                local gib=$(( C_BLOCKS[$phase] / (1024 * 1024 * 1024 / C_BSIZE[$phase]) ))
                [ "$gib" -gt 0 ] || gib=1
                case "$key" in
                    max-mount-reads-per-gib) got=$(( C_MREADS[$phase] / gib )) ;;
                    max-lookup-reads)        got="${C_LOOKUPR[$phase]}" ;;
                    min-volume-gib)          got="$gib" ;;
                    min-dirents)             got="${C_DIRENTS[$phase]}" ;;
                    min-files)               got="${C_FILES[$phase]}" ;;
                    min-treebytes)           got="${C_TREEB[$phase]}" ;;
                esac
                ;;
            *) gate_error "$gate:$lineno: unknown gate key '$key'" ;;
        esac
        case "$key" in
            max-*) [ "$got" -le "$want" ] || fail "$phase $key: $got > $want" ;;
            min-*) [ "$got" -ge "$want" ] || fail "$phase $key: $got < $want" ;;
        esac
        printf '  %-24s %-10s %6s (cap %s)\n' "$key" "$phase" "$got" "$want"
    done < "$gate"
    if [ "${#MATCHED[@]}" -eq 0 ]; then
        fail "gate file $gate graded nothing"
    fi
}

self_test() {
    local tmp; tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    local rc

    # A log the gate must accept: exactly the caps it is given.
    cat > "$tmp/good.log" <<'EOF'
noise before
FSPERF[tests]: bytes=4194304 txns=16 commits=16 devwrites=64 devblocks=1024 barriers=16 ns=200000000 rawbytes=4194304 rawns=100000000
noise after
EOF
    cat > "$tmp/gate.txt" <<'EOF'
max-txns-per-mib	tests	4
max-commits-per-mib	tests	4
max-devwrites-per-mib	tests	16
max-devblocks-per-mib	tests	256
max-barriers-per-mib	tests	4
min-fs-over-raw-pct	tests	50
min-bytes	tests	1048576
EOF
    mkdir -p "$tmp/gates"
    cp "$tmp/gate.txt" "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: the gate rejected a log inside its caps" >&2; return 1; }

    # One transaction per 4 KiB — the shape this ratchet exists to catch.
    sed 's/txns=16/txns=1024/' "$tmp/good.log" > "$tmp/chunky.log"
    rc=0
    "$0" --log "$tmp/chunky.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a transaction per 4 KiB passed" >&2; return 1; }

    # Twice the sectors for the same bytes at an unchanged request count: data
    # written to the log instead of home is invisible to the request cap.
    sed 's/devblocks=1024/devblocks=2048/' "$tmp/good.log" > "$tmp/amplified.log"
    rc=0
    "$0" --log "$tmp/amplified.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a write moving twice its sectors passed" >&2; return 1; }

    # A row with a field missing or spare must be a gate-file error (2), not a
    # silent skip: the rows around it match, so nothing else would notice.
    { cat "$tmp/gate.txt"; printf 'max-txns-per-mib\ttests\n'; } > "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 2 ] || { echo "self-test: a two-field gate row was not a gate-file error (rc $rc)" >&2; return 1; }
    { cat "$tmp/gate.txt"; printf 'max-txns-per-mib\ttests\t4\t4\n'; } > "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 2 ] || { echo "self-test: a four-field gate row was not a gate-file error (rc $rc)" >&2; return 1; }

    # A misspelled key is the same class of error, and must not read as a cost
    # to re-measure either.
    { cat "$tmp/gate.txt"; printf 'max-devblock-per-mib\ttests\t256\n'; } > "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 2 ] || { echo "self-test: a misspelled gate key was not a gate-file error (rc $rc)" >&2; return 1; }

    # Half the raw device's rate where the gate wants three quarters.
    sed 's/ns=200000000/ns=400000000/' "$tmp/good.log" > "$tmp/slow.log"
    cat > "$tmp/gates/tests.txt" <<'EOF'
min-fs-over-raw-pct	tests	75
min-bytes	tests	1048576
EOF
    rc=0
    "$0" --log "$tmp/slow.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a run at half the graded rate ratio passed" >&2; return 1; }

    # The same absolute rate on a machine three times slower at everything:
    # the quotient is unchanged, so the gate must stay silent. This is the
    # property an absolute MiB/s floor does not have.
    sed -e 's/ns=200000000/ns=600000000/' -e 's/rawns=100000000/rawns=300000000/' \
        "$tmp/good.log" > "$tmp/noaccel.log"
    cp "$tmp/gate.txt" "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/noaccel.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: a uniformly slower machine failed the rate ratio" >&2; return 1; }

    # A run that wrote almost nothing has a perfect cost per MiB and must fail.
    sed -e 's/bytes=4194304/bytes=4096/' -e 's/txns=16/txns=1/' "$tmp/good.log" > "$tmp/empty.log"
    rc=0
    "$0" --log "$tmp/empty.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a run that wrote 4 KiB passed the byte floor" >&2; return 1; }

    # A gate row naming a phase the run never reported is a dead entry.
    cat > "$tmp/gates/tests.txt" <<'EOF'
max-txns-per-mib	tests	4
min-bytes	tests	1048576
max-txns-per-mib	ghost	4
EOF
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a dead gate entry passed" >&2; return 1; }

    # A log with no report at all must fail rather than read as cost-free.
    echo "nothing to see" > "$tmp/silent.log"
    cp "$tmp/gate.txt" "$tmp/gates/tests.txt"
    rc=0
    "$0" --log "$tmp/silent.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a log with no FSPERF line passed" >&2; return 1; }

    # Capacity rows are skipped when the volume is absent and required when asked.
    cat > "$tmp/gates/tests.txt" <<'EOF'
max-txns-per-mib	tests	4
min-bytes	tests	1048576
max-mount-reads-per-gib	capacity	64
min-volume-gib	capacity	16
min-files	capacity	4000
min-treebytes	capacity	800000000
EOF
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: an absent capacity volume failed a non-required run" >&2; return 1; }
    rc=0
    "$0" --log "$tmp/good.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: --require-capacity passed without an FSCAP line" >&2; return 1; }

    # A real capacity line inside its caps passes; one whose mount cost scales
    # with the volume does not.
    cat > "$tmp/cap.log" <<'EOF'
FSPERF[tests]: bytes=4194304 txns=16 commits=16 devwrites=64 devblocks=1024 barriers=16 ns=200000000 rawbytes=4194304 rawns=100000000
FSCAP[capacity]: blocks=4194304 blocksize=4096 groups=128 cacheentries=4096 mountreads=512 mountns=900000000 dirents=20000 lookupreads=3 bytes=8388608 ns=400000000 files=5447 treebytes=1105596389
EOF
    rc=0
    "$0" --log "$tmp/cap.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: a capacity line inside its caps was rejected" >&2; return 1; }
    sed 's/mountreads=512/mountreads=65536/' "$tmp/cap.log" > "$tmp/cap-slow.log"
    rc=0
    "$0" --log "$tmp/cap-slow.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a mount reading 4 MiB per GiB passed" >&2; return 1; }

    # A volume that stopped being populated: the walk is cheap because there
    # is nothing on the medium, which is the reading `min-files` refuses.
    sed 's/files=5447/files=2/' "$tmp/cap.log" > "$tmp/cap-bare.log"
    rc=0
    "$0" --log "$tmp/cap-bare.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a volume holding 2 files passed the residency floor" >&2; return 1; }
    sed 's/treebytes=1105596389/treebytes=4096/' "$tmp/cap.log" > "$tmp/cap-thin.log"
    rc=0
    "$0" --log "$tmp/cap-thin.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -ne 0 ] || { echo "self-test: a tree of 4 KiB passed the residency byte floor" >&2; return 1; }

    # --emit-allowlist output must round-trip through the check path, or
    # "re-measure with --emit-allowlist" is not a remedy that works.
    "$0" --log "$tmp/cap.log" --emit-allowlist > "$tmp/gates/tests.txt" 2>/dev/null
    rc=0
    "$0" --log "$tmp/cap.log" --gate-data-dir "$tmp/gates" --require-capacity >/dev/null 2>&1 || rc=$?
    [ "$rc" -eq 0 ] || { echo "self-test: emitted allowlist does not accept its own measurement" >&2; return 1; }

    echo "check_fs_throughput: self-test passed"
}

if [ "$SELF_TEST" = "1" ]; then
    self_test
    exit 0
fi

CAPTURE=""
if [ -z "$LOG" ]; then
    CAPTURE="$REPO_ROOT/builddir/fsperf-capture.log"
    mkdir -p "$(dirname "$CAPTURE")"
    boot_and_capture "$CAPTURE"
    LOG="$CAPTURE"
fi
parse_log "$LOG"

if [ "$EMIT" = "1" ]; then
    emit_allowlist
    exit 0
fi

GATE="$GATE_DATA_DIR/$VARIANT.txt"
if [ ! -f "$GATE" ]; then
    echo "check_fs_throughput: no gate data at $GATE" >&2
    echo "  Measure it: scripts/check_fs_throughput.sh --variant $VARIANT --emit-allowlist" >&2
    exit 1
fi
echo "check_fs_throughput: variant=$VARIANT log=$LOG"
check_against "$GATE"
if [ "$FAIL" != "0" ]; then
    echo "check_fs_throughput: a cost above its cap is a measurement to re-take," >&2
    echo "  not a number to raise. Re-measure with --emit-allowlist in the same" >&2
    echo "  commit and say what started costing more." >&2
    exit 1
fi
echo "check_fs_throughput: OK"
