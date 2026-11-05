package gates

// The lockdep gate holds every boot's lock-order validator to what it can
// promise on any run: it is ACTIVE at each required phase, it reported no
// violation, each pool (classes, edges, chains) stays under max-fill-pct of its
// size, and each clears a floor far below every observation, so a validator
// that stopped recording fails instead of satisfying every ceiling.
//
// Counts are not graded. Exact class caps and edge/chain caps and bands graded
// totals that moved between runs of identical code — which classes a phase
// registers and which orderings it observes depend on what the suite reached
// and how it was scheduled — and failed CI on docs-only commits (407d1d69,
// 38465620) whose parents passed. A lock-order defect fails as `violations=`;
// a lock array landing without a class key adds classes by the hundred and
// fails max-fill-pct.
//
// Aggregation: every invariant is per boot, so each log is graded on its own.

import (
	"fmt"
	"regexp"
	"slices"
	"strings"
)

func init() {
	register(Gate{
		Name:    "lockdep",
		Summary: "lock validator ACTIVE with no violation, pools under max-fill-pct, floors met, on every boot",
		Run:     runLockdep,
	})
}

var (
	lockdepMarker = regexp.MustCompile(`LOCKDEP\[[a-z-]+\]:`)
	// A state word may carry a parenthetical ("DISABLED (pool overflow)"),
	// so the run-up to classes= is "contains no equals sign".
	lockdepRE = regexp.MustCompile(`^LOCKDEP\[([a-z-]+)\]: ([A-Z]+)[^=]*classes=(\d+)/(\d+) \((\d+)%\) edges=(\d+)/(\d+) chains=(\d+)/(\d+).*violations=(\d+)`)
)

type lockdepPool struct {
	name      string
	val, size int64
}

type lockdepReport struct {
	phase, state, raw string
	pools             [3]lockdepPool
	violations        int64
}

func (r lockdepReport) classes() int64 { return r.pools[0].val }
func (r lockdepReport) edges() int64   { return r.pools[1].val }
func (r lockdepReport) chains() int64  { return r.pools[2].val }

func parseLockdep(c capture) ([]lockdepReport, error) {
	var out []lockdepReport
	for _, line := range c.lines {
		text, ok := reportAt(line, lockdepMarker)
		if !ok {
			continue
		}
		m := lockdepRE.FindStringSubmatch(text)
		if m == nil {
			return nil, fmt.Errorf("%s: could not parse a LOCKDEP line — the report format moved.\n  line: %s\n  want: LOCKDEP[<phase>]: <STATE> classes=N/C (P%%) edges=E/EC chains=H/HC ... violations=V\n  Update kdiag_dump_lock_graph and this gate together.", c.path, text)
		}
		out = append(out, lockdepReport{
			phase: m[1],
			state: m[2],
			raw:   text,
			pools: [3]lockdepPool{
				{"classes", num(m[3]), num(m[4])},
				{"edges", num(m[6]), num(m[7])},
				{"chains", num(m[8]), num(m[9])},
			},
			violations: num(m[10]),
		})
	}
	if len(out) == 0 {
		return nil, fmt.Errorf("%s: no LOCKDEP[...] line was emitted — nothing was measured. The kernel did not reach kdiag_dump_lock_graph (boot panic, missing ISO, or a renamed report line).", c.path)
	}
	return out, nil
}

type lockdepGate struct {
	maxFill, minClasses, minEdges, minChains int64
	required                                 []string
}

func readLockdepGate(cfg Config) (string, lockdepGate, error) {
	var g lockdepGate
	rel, lines, err := readGateFile(cfg, "lockdep")
	if err != nil {
		return rel, g, err
	}
	set := map[string]bool{}
	for _, l := range lines {
		fields := strings.Fields(l.text)
		key := fields[0]
		if key == "band" || strings.Contains(l.text, "\t") {
			return rel, g, gateFileErrorf("%s:%d: %q is a pool cap or band, which are no longer graded — counts move between runs of identical code. Delete it, or re-emit the file:\n    %s",
				rel, l.no, l.text, emitCommand("lockdep", cfg.Variant))
		}
		var dst *int64
		switch key {
		case "max-fill-pct":
			dst = &g.maxFill
		case "min-classes":
			dst = &g.minClasses
		case "min-edges":
			dst = &g.minEdges
		case "min-chains":
			dst = &g.minChains
		case "require-phase":
		default:
			return rel, g, gateFileErrorf("%s:%d: unknown directive '%s'", rel, l.no, key)
		}
		if len(fields) != 2 {
			return rel, g, gateFileErrorf("%s:%d: want '%s <value>', got %q", rel, l.no, key, l.text)
		}
		if key == "require-phase" {
			if !phaseNameRE.MatchString(fields[1]) {
				return rel, g, gateFileErrorf("%s:%d: bad phase name %q", rel, l.no, fields[1])
			}
			g.required = append(g.required, fields[1])
			continue
		}
		if set[key] {
			return rel, g, gateFileErrorf("%s:%d: '%s' given twice", rel, l.no, key)
		}
		set[key] = true
		n, err := parseCount(rel, l, fields[1])
		if err != nil {
			return rel, g, err
		}
		*dst = n
	}
	for _, key := range []string{"max-fill-pct", "min-classes", "min-edges", "min-chains"} {
		if !set[key] {
			return rel, g, gateFileErrorf("%s: no '%s' — every ceiling and floor is stated, or a validator that stopped recording reads as healthy", rel, key)
		}
	}
	if g.maxFill > 100 {
		return rel, g, gateFileErrorf("%s: max-fill-pct %d is over 100", rel, g.maxFill)
	}
	if len(g.required) == 0 {
		return rel, g, gateFileErrorf("%s: no require-phase, so it grades nothing", rel)
	}
	return rel, g, nil
}

func runLockdep(cfg Config) error {
	caps, err := loadCaptures(cfg.Logs)
	if err != nil {
		return err
	}
	reports := make([][]lockdepReport, len(caps))
	for i, c := range caps {
		if reports[i], err = parseLockdep(c); err != nil {
			return err
		}
	}
	if cfg.Emit {
		emitLockdep(cfg, reports)
		return nil
	}
	_, g, err := readLockdepGate(cfg)
	if err != nil {
		return err
	}

	var fail findings
	peak := map[string]*[3]lockdepPool{}
	for i, c := range caps {
		for _, phase := range g.required {
			seen := false
			for _, r := range reports[i] {
				if r.phase != phase {
					continue
				}
				seen = true
				gradeLockdep(&fail, c.path, g, r)
				p := peak[phase]
				if p == nil {
					p = &[3]lockdepPool{}
					*p = r.pools
					peak[phase] = p
				}
				for k, pool := range r.pools {
					if pool.val > p[k].val {
						p[k] = pool
					}
				}
			}
			if !seen {
				fail.add("%s: gate requires phase '%s' but the boot never printed it — a phase that stopped reporting looks exactly like a phase that passed.", c.path, phase)
			}
		}
	}
	if len(fail) > 0 {
		return fail.err()
	}
	for _, phase := range g.required {
		p := peak[phase]
		fmt.Fprintf(cfg.Out, "lockdep: OK — LOCKDEP[%s] ACTIVE, no violation, peak over %d log(s): classes=%d/%d edges=%d/%d chains=%d/%d (max-fill-pct %d)\n",
			phase, len(caps), p[0].val, p[0].size, p[1].val, p[1].size, p[2].val, p[2].size, g.maxFill)
	}
	return nil
}

func gradeLockdep(fail *findings, log string, g lockdepGate, r lockdepReport) {
	if r.state != "ACTIVE" {
		fail.add("%s: LOCKDEP[%s] is %s, not ACTIVE — locks are unvalidated.", log, r.phase, r.state)
	}
	if r.violations != 0 {
		fail.add("%s: LOCKDEP[%s] reported %d violation(s) with the panic suppressed — a lock-order defect; the report above it in the log names the cycle.", log, r.phase, r.violations)
	}
	if r.classes() < g.minClasses {
		fail.add("%s: LOCKDEP[%s] registered only %d classes (min-classes %d) — nothing measured.", log, r.phase, r.classes(), g.minClasses)
	}
	if r.edges() < g.minEdges {
		fail.add("%s: LOCKDEP[%s] recorded only %d edges (min-edges %d) — nothing measured.", log, r.phase, r.edges(), g.minEdges)
	}
	if r.chains() < g.minChains {
		fail.add("%s: LOCKDEP[%s] recorded only %d chains (min-chains %d) — nothing measured.", log, r.phase, r.chains(), g.minChains)
	}
	for _, pool := range r.pools {
		if pool.size <= 0 {
			fail.add("%s: LOCKDEP[%s] reports a %s pool of size %d — the report format moved.", log, r.phase, pool.name, pool.size)
			continue
		}
		// Compared without the truncation the percentage carries, so the
		// ceiling the gate file states is the ceiling enforced.
		if pool.val*100 > g.maxFill*pool.size {
			fail.add("%s: LOCKDEP[%s] %s pool %d%% full (%d/%d), over max-fill-pct %d — a lock array without a class key, or a pool that needs to grow.",
				log, r.phase, pool.name, pool.val*100/pool.size, pool.val, pool.size, g.maxFill)
		}
	}
}

func emitLockdep(cfg Config, reports [][]lockdepReport) {
	// A phase is required only if every boot printed it, so the file the
	// emitter writes passes the logs it was measured from.
	const maxFill, minClasses, minEdges, minChains = 70, 32, 16, 32
	count := map[string]int{}
	var order []string
	for _, rs := range reports {
		inLog := map[string]bool{}
		for _, r := range rs {
			if !inLog[r.phase] {
				inLog[r.phase] = true
				count[r.phase]++
			}
			if !slices.Contains(order, r.phase) {
				order = append(order, r.phase)
			}
		}
	}
	var ceilings []string
	for _, pool := range reports[0][0].pools {
		ceilings = append(ceilings, fmt.Sprintf("#   %d of %d %s\n", maxFill*pool.size/100, pool.size, pool.name))
	}
	fmt.Fprintf(cfg.Out, `# lockdep gate data — variant: %s
#
#     %s
#
# Graded on every boot (every --log on its own): each required phase reports
# the lock validator ACTIVE with no violation; each pool (classes, edges,
# chains) stays under max-fill-pct of its size; and each phase clears the
# floors, so a validator that stopped recording fails instead of satisfying
# every ceiling.
#
# Counts are not graded. Exact class caps and edge/chain caps and bands
# graded totals that moved between runs of identical code (which classes a
# phase registers and which orderings it observes depend on what the suite
# reached and how it was scheduled) and failed CI on docs-only commits
# 407d1d69 and 38465620, whose parents passed. A lock-order defect fails as
# violations=; a lock array landing without a class key adds classes by the
# hundred and fails max-fill-pct.
#
# max-fill-pct mirrors MAX_POOL_FILL_PCT and min-classes MIN_CLASSES in
# ktesting/src/lockdep_tests.rs. The fill comparison is val*100 against
# max-fill-pct*pool_size, so with max-fill-pct %d the ceilings are:
%s# The floors are chosen far under every observation; the required phases are
# measured.

max-fill-pct %d
min-classes %d
min-edges %d
min-chains %d

`, cfg.Variant, emitCommand("lockdep", cfg.Variant), maxFill, strings.Join(ceilings, ""),
		maxFill, minClasses, minEdges, minChains)
	for _, phase := range order {
		if count[phase] == len(reports) {
			fmt.Fprintf(cfg.Out, "require-phase %s\n", phase)
		}
	}
}
