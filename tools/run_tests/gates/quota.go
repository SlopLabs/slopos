package gates

// The quota gate holds every resource account's peak to a measured cap, a
// denial to zero except where the suite refuses on purpose, and the charge
// path's cost to ratios taken against the same run's own reference.
//
// A denial is a finding, with one exception the gate file has to name: a kind
// whose refusal is a tested property. commitpages is the machine's commit
// ceiling, which the suite proves refuses at mmap and at fork; process and
// task are a person's share of the process and task tables, which
// sessions_test's fork bomb fills until it is refused. `expect-denials` holds
// such a kind to an exact count in both directions, and is refused for any
// other kind, so an accidental denial elsewhere stays a finding.
//
// The caps are measured, never chosen: the enforced default lives in the
// kernel and the gate ceiling here, so a boot-time observation never becomes
// a limit that cannot later be raised. `peak`, not `used`: a dump-time `used`
// samples whatever is live at that instant, and a peak of zero means the kind
// was never exercised, so a cap on it would bound nothing.
//
// Aggregation over shard logs: every boot must print each required phase;
// peaks are the max across logs; the test phases' denials are summed, so
// expect-denials still describes the whole suite. Boot-phase rows are the
// same boot in every shard and the later phases' rows count from boot, so a
// boot denial would be counted once per shard: in a run of several logs any
// boot denial fails as unattributable. The charge cost is measured by one
// test, so it is graded in whichever log carries the QUOTACOST lines.

import (
	"fmt"
	"maps"
	"regexp"
	"slices"
	"strings"
)

func init() {
	register(Gate{
		Name:    "quota",
		Summary: "account peaks under measured caps, no unexpected denial, charge cost ratios held",
		Run:     runQuota,
	})
}

// quotaRefusalTestedKinds are the kinds whose refusal the suite provokes on
// purpose; only these may carry an expect-denials line.
var quotaRefusalTestedKinds = []string{"commitpages", "process", "task"}

// quotaDirectiveFields is each space-separated directive's field count,
// directive name included.
var quotaDirectiveFields = map[string]int{
	"min-kinds": 2, "min-kinds-for": 3, "max-denials": 2, "expect-denials": 4,
	"min-charge-over-reference": 3, "min-reference-cycles": 2,
	"max-depth-cost-ratio": 3, "require-phase": 2,
}

// quotaBootPhase is the phase every shard replays identically.
const quotaBootPhase = "boot"

var (
	quotaMarker = regexp.MustCompile(`QUOTA\[[a-z-]+\]:`)
	quotaRE     = regexp.MustCompile(`^QUOTA\[([a-z-]+)\]: mode=([a-z]+) slot=(\d+) kind=([a-z]+) used=(\d+) peak=(\d+) limit=(-?\d+) denials=(\d+)`)
	quotaCostRE = regexp.MustCompile(`QUOTACOST: depth=(\d+) cycles_per_charge=(\d+)`)
	quotaRefRE  = regexp.MustCompile(`QUOTACOST: reference cycles_per_op=(\d+)`)
)

type quotaLog struct {
	path    string
	seen    map[string]bool
	mode    map[string]string
	peak    map[string]int64
	denials map[string]int64
	// cost is depth -> worst cycles per charge+refund round trip; ref is the
	// worst bare CAS round trip in the same run, the scale cost is read against.
	cost    map[int64]int64
	ref     int64
	hasCost bool
}

func quotaKey(phase, kind string) string { return phase + "/" + kind }

func parseQuota(c capture) (*quotaLog, error) {
	l := &quotaLog{
		path:    c.path,
		seen:    map[string]bool{},
		mode:    map[string]string{},
		peak:    map[string]int64{},
		denials: map[string]int64{},
		cost:    map[int64]int64{},
	}
	for _, line := range c.lines {
		if text, ok := reportAt(line, quotaMarker); ok {
			m := quotaRE.FindStringSubmatch(text)
			if m == nil {
				return nil, fmt.Errorf("%s: could not parse a QUOTA line — the report format moved.\n  line: %s\n  want: QUOTA[<phase>]: mode=M slot=S kind=K used=U peak=P limit=L denials=D\n  Update quota_report and this gate together.", c.path, text)
			}
			key := quotaKey(m[1], m[4])
			l.seen[m[1]] = true
			l.mode[m[1]] = m[2]
			if p, ok := l.peak[key]; !ok || num(m[6]) > p {
				l.peak[key] = num(m[6])
			}
			l.denials[key] += num(m[8])
		}
		if m := quotaCostRE.FindStringSubmatch(line); m != nil {
			l.hasCost = true
			depth, cycles := num(m[1]), num(m[2])
			if c, ok := l.cost[depth]; !ok || cycles > c {
				l.cost[depth] = cycles
			}
		}
		if m := quotaRefRE.FindStringSubmatch(line); m != nil {
			l.hasCost = true
			l.ref = max(l.ref, num(m[1]))
		}
	}
	if len(l.seen) == 0 {
		return nil, fmt.Errorf("%s: no QUOTA[...] line was emitted — nothing was measured. The kernel did not reach quota_report (boot panic, missing ISO, a renamed report line, or no charge was ever taken).", c.path)
	}
	return l, nil
}

// quotaRun is every log folded into one suite-wide account table.
type quotaRun struct {
	logs     []*quotaLog
	seenIn   map[string]int
	mode     map[string]string
	peak     map[string]int64
	denials  map[string]int64
	costLogs []*quotaLog
}

func foldQuota(logs []*quotaLog) (*quotaRun, findings) {
	r := &quotaRun{
		logs:    logs,
		seenIn:  map[string]int{},
		mode:    map[string]string{},
		peak:    map[string]int64{},
		denials: map[string]int64{},
	}
	var fail findings
	for _, l := range logs {
		for phase := range l.seen {
			r.seenIn[phase]++
			r.mode[phase] = l.mode[phase]
		}
		for key, p := range l.peak {
			if old, ok := r.peak[key]; !ok || p > old {
				r.peak[key] = p
			}
		}
		for _, key := range sortedKeys(l.denials) {
			d := l.denials[key]
			if len(logs) > 1 && d != 0 && strings.HasPrefix(key, quotaBootPhase+"/") {
				fail.add("%s: %s recorded %d denial(s) in a run of %d logs. Every shard replays the same boot, and the later phases' rows count from it, so a boot denial cannot be attributed to one boot or summed: grade this on one unsharded log, or make boot stop refusing.", l.path, key, d, len(logs))
			}
			r.denials[key] += d
		}
		if l.hasCost {
			r.costLogs = append(r.costLogs, l)
		}
	}
	return r, fail
}

type quotaGate struct {
	minKinds, maxDenials int64
	minKindsFor          map[string]int64
	required             []string
	caps                 map[string]int64
	expectDenials        map[string]int64
	costFloor            map[int64]int64
	ratioDepth, ratioCap int64
	minReference         int64
}

func readQuotaGate(cfg Config) (*quotaGate, error) {
	g := &quotaGate{
		minKindsFor:   map[string]int64{},
		caps:          map[string]int64{},
		expectDenials: map[string]int64{},
		costFloor:     map[int64]int64{},
	}
	rel, lines, err := readGateFile(cfg, "quota")
	if err != nil {
		return nil, err
	}
	for _, l := range lines {
		if strings.Contains(l.text, "\t") {
			f := strings.Split(l.text, "\t")
			if len(f) != 3 {
				return nil, gateFileErrorf("%s:%d: want '<phase> <TAB> <kind> <TAB> <cap>', got %q", rel, l.no, l.text)
			}
			n, err := parseCount(rel, l, f[2])
			if err != nil {
				return nil, err
			}
			g.caps[quotaKey(f[0], f[1])] = n
			continue
		}
		f := strings.Fields(l.text)
		want, known := quotaDirectiveFields[f[0]]
		if !known {
			return nil, gateFileErrorf("%s:%d: unknown directive '%s'", rel, l.no, f[0])
		}
		if len(f) != want {
			return nil, gateFileErrorf("%s:%d: '%s' takes %d field(s), got %q", rel, l.no, f[0], want-1, l.text)
		}
		var vals []int64
		for _, s := range f[1:] {
			if n, err := parseCount(rel, l, s); err == nil {
				vals = append(vals, n)
			} else {
				vals = append(vals, -1)
			}
		}
		last := vals[len(vals)-1]
		if f[0] != "require-phase" && last < 0 {
			return nil, gateFileErrorf("%s:%d: want a non-negative integer, got %q", rel, l.no, f[len(f)-1])
		}
		switch f[0] {
		case "min-kinds":
			g.minKinds = last
		case "min-kinds-for":
			g.minKindsFor[f[1]] = last
		case "max-denials":
			g.maxDenials = last
		case "expect-denials":
			if !slices.Contains(quotaRefusalTestedKinds, f[2]) {
				return nil, gateFileErrorf("%s:%d: expect-denials names '%s', whose refusal is not a tested property", rel, l.no, f[2])
			}
			g.expectDenials[quotaKey(f[1], f[2])] = last
		case "min-charge-over-reference":
			if vals[0] < 0 {
				return nil, gateFileErrorf("%s:%d: bad depth %q", rel, l.no, f[1])
			}
			g.costFloor[vals[0]] = last
		case "min-reference-cycles":
			g.minReference = last
		case "max-depth-cost-ratio":
			if vals[0] < 0 {
				return nil, gateFileErrorf("%s:%d: bad depth %q", rel, l.no, f[1])
			}
			g.ratioDepth, g.ratioCap = vals[0], last
		case "require-phase":
			g.required = append(g.required, f[1])
		}
	}
	return g, nil
}

func runQuota(cfg Config) error {
	caps, err := loadCaptures(cfg.Logs)
	if err != nil {
		return err
	}
	logs := make([]*quotaLog, len(caps))
	for i, c := range caps {
		if logs[i], err = parseQuota(c); err != nil {
			return err
		}
	}
	run, fail := foldQuota(logs)
	if len(fail) > 0 {
		return fail.err()
	}
	if cfg.Emit {
		emitQuota(cfg, run)
		return nil
	}
	g, err := readQuotaGate(cfg)
	if err != nil {
		return err
	}

	for _, phase := range g.required {
		missing := false
		for _, l := range logs {
			if !l.seen[phase] {
				missing = true
				fail.add("%s: gate requires phase '%s' but the boot never printed it — a phase that stopped reporting looks exactly like a phase that passed.", l.path, phase)
			}
		}
		if missing {
			continue
		}
		kinds := int64(0)
		for key := range run.peak {
			if strings.HasPrefix(key, phase+"/") {
				kinds++
			}
		}
		// Boot legitimately reports fewer kinds than the test phases: the
		// only charges before userland exists are the idle tasks' stacks.
		floor := g.minKinds
		if f, ok := g.minKindsFor[phase]; ok {
			floor = f
		}
		if kinds < floor {
			fail.add("QUOTA[%s] reported only %d kind(s) (min %d) — nothing measured.", phase, kinds, floor)
		}
	}

	unmatchedCaps := maps.Clone(g.caps)
	expect := maps.Clone(g.expectDenials)
	for _, key := range sortedKeys(run.peak) {
		phase, _, _ := strings.Cut(key, "/")
		// Only phases the gate asks about are held to a cap; a boot that
		// reports an extra phase is not a failure.
		if !slices.Contains(g.required, phase) {
			continue
		}
		peak, denials := run.peak[key], run.denials[key]
		if peak == 0 {
			fail.add("%s peaked at 0 — the kind was never exercised, so a cap on it bounds nothing.", key)
		}
		if want, ok := expect[key]; ok {
			if denials != want {
				fail.add("%s recorded %d denial(s), want exactly %d. Fewer: a deliberate refusal stopped happening. More: something refused that no test meant to.", key, denials, want)
			}
			delete(expect, key)
		} else if denials > g.maxDenials {
			fail.add("%s recorded %d denial(s), over max-denials %d. Under quota=warn these were granted; under quota=enforce they would refuse.", key, denials, g.maxDenials)
		}
		if limit, ok := unmatchedCaps[key]; ok {
			if peak > limit {
				fail.add("%s peaked at %d, over the recorded cap %d. Re-measure with --emit-allowlist and say what started consuming more.", key, peak, limit)
			}
			delete(unmatchedCaps, key)
		}
	}

	costLogs := run.costLogs
	if len(costLogs) == 0 {
		// Graded as a log that measured nothing, so every cost directive
		// reports the measurement it is missing.
		costLogs = []*quotaLog{{path: "every log", cost: map[int64]int64{}}}
	}
	for _, l := range costLogs {
		gradeQuotaCost(&fail, g, l)
	}

	// An entry matching nothing is dead: it stops describing the kernel and
	// keeps passing after the kind it names stopped being charged.
	for _, key := range sortedKeys(unmatchedCaps) {
		fail.add("gate entry '%s' matched no observed phase/kind — dead entry, delete it.", key)
	}
	for _, key := range sortedKeys(expect) {
		fail.add("gate entry 'expect-denials %s' matched no observed phase/kind — dead entry, delete it.", key)
	}
	for _, phase := range sortedKeys(g.minKindsFor) {
		if run.seenIn[phase] == 0 {
			fail.add("gate entry 'min-kinds-for %s' matched no observed phase — dead entry, delete it.", phase)
		}
	}
	if len(fail) > 0 {
		return fail.err()
	}

	for _, phase := range g.required {
		for _, key := range sortedKeys(run.peak) {
			if strings.HasPrefix(key, phase+"/") {
				fmt.Fprintf(cfg.Out, "quota: OK — %s peak=%d denials=%d (mode=%s)\n", key, run.peak[key], run.denials[key], run.mode[phase])
			}
		}
	}
	for _, l := range run.costLogs {
		for _, depth := range slices.Sorted(maps.Keys(g.costFloor)) {
			fmt.Fprintf(cfg.Out, "quota: OK — %s: charge cost at depth %d = %dx100 of the %d-cycle reference (floor %d)\n",
				l.path, depth, l.cost[depth]*100/l.ref, l.ref, g.costFloor[depth])
		}
		if g.ratioDepth > 0 {
			fmt.Fprintf(cfg.Out, "quota: OK — %s: depth %d costs %dx100 of depth 1 (cap %d)\n",
				l.path, g.ratioDepth, l.cost[g.ratioDepth]*100/l.cost[1], g.ratioCap)
		}
	}
	return nil
}

func gradeQuotaCost(fail *findings, g *quotaGate, l *quotaLog) {
	// A floor, not a cap: a bare CAS is relatively far dearer under TCG than
	// natively, so the reference can only prove the measurement happened and
	// that the charge path still does atomic work.
	if len(g.costFloor) > 0 && l.ref <= 0 {
		fail.add("%s: no QUOTACOST reference line, so no cost ratio can be formed. A cost nobody measures is how an 8 %% throughput regression passes.", l.path)
	}
	// The floors are ratios over the reference, so a collapsed reference
	// satisfies all of them; no x86 does a lock cmpxchg round trip this cheap.
	if g.minReference > 0 && l.ref > 0 && l.ref < g.minReference {
		fail.add("%s: the QUOTACOST reference measured %d cycles, under the floor %d. A CAS round trip cannot be that cheap; the reference measurement collapsed, and every ratio taken against it is meaningless.", l.path, l.ref, g.minReference)
	}
	for _, depth := range slices.Sorted(maps.Keys(g.costFloor)) {
		cost, ok := l.cost[depth]
		if !ok {
			fail.add("%s: gate expects a QUOTACOST line for depth %d and the boot printed none. A cost nobody measures is how an 8 %% throughput regression passes.", l.path, depth)
			continue
		}
		if l.ref <= 0 {
			continue
		}
		if ratio := cost * 100 / l.ref; ratio < g.costFloor[depth] {
			fail.add("%s: a charge at depth %d cost %d cycles against a %d-cycle reference — %dx100, under the floor %d. The charge path cannot be cheaper than the atomics it performs; the measurement collapsed.",
				l.path, depth, cost, l.ref, ratio, g.costFloor[depth])
		}
	}
	if g.ratioDepth > 0 {
		shallow, okShallow := l.cost[1]
		deep, okDeep := l.cost[g.ratioDepth]
		if !okShallow || !okDeep || shallow <= 0 {
			var depths []string
			for _, d := range slices.Sorted(maps.Keys(l.cost)) {
				depths = append(depths, fmt.Sprint(d))
			}
			fail.add("%s: gate expects QUOTACOST at depth 1 and %d and the boot printed only: [%s]", l.path, g.ratioDepth, strings.Join(depths, " "))
		} else if ratio := deep * 100 / shallow; ratio > g.ratioCap {
			fail.add("%s: depth %d costs %dx100 of depth 1, over the recorded cap %d. The walk stopped amortising: each level is meant to add a bounded constant.", l.path, g.ratioDepth, ratio, g.ratioCap)
		}
	}
}

func emitQuota(cfg Config, run *quotaRun) {
	out := cfg.Out
	fmt.Fprintf(out, `# quota gate data — variant: %s
#
#     %s
#
# One line per <phase> <TAB> <kind> <TAB> <cap>, where the cap is the
# highest peak any single account row reached for that kind. Measured,
# never chosen: raise a cap only with a fresh --emit-allowlist in the
# same commit, and say in the message what started consuming more.
#
# A cap matching nothing is a dead entry and fails, because a kind that
# stopped being reported looks exactly like a kind that stayed cheap.

# The peak of a kind nobody exercised is zero, and a cap derived from
# it would bound nothing. Every listed kind must have been reached.
min-kinds 2
`, cfg.Variant, emitCommand("quota", cfg.Variant))
	// Only for a phase a run reported: the check path sweeps min-kinds-for
	// for dead entries.
	if run.seenIn[quotaBootPhase] > 0 {
		fmt.Fprintf(out, "min-kinds-for %s 1\n", quotaBootPhase)
	}
	fmt.Fprint(out, `
# A denial is an over-limit charge. Under quota=warn it is granted and
# counted, so a non-zero total means the enforced tier would have
# refused something — which is a finding, not a pass.
max-denials 0

# The refusals the suite provokes on purpose, held to their exact count:
# fewer means a test stopped refusing, more means something else was.
`)
	for _, key := range sortedKeys(run.denials) {
		d := run.denials[key]
		if d == 0 {
			continue
		}
		phase, kind, _ := strings.Cut(key, "/")
		if slices.Contains(quotaRefusalTestedKinds, kind) {
			fmt.Fprintf(out, "expect-denials %s %s %d\n", phase, kind, d)
		} else {
			fmt.Fprintf(out, "# not emitting %s's %d denial(s): its refusal is not a tested property\n", key, d)
		}
	}
	fmt.Fprintln(out)
	for _, phase := range sortedKeys(run.seenIn) {
		if run.seenIn[phase] == len(run.logs) {
			fmt.Fprintf(out, "require-phase %s\n", phase)
		}
	}
	fmt.Fprintln(out)
	for _, key := range sortedKeys(run.peak) {
		phase, kind, _ := strings.Cut(key, "/")
		fmt.Fprintf(out, "%s\t%s\t%d\n", phase, kind, run.peak[key])
	}
	fmt.Fprint(out, `
# The charge path's cost, as ratios rather than cycle counts. A cycle
# count here measures the accelerator: the same tree reports ~1500
# cycles per charge under KVM and ~20000 under TCG, and both are
# correct. Every number below is x100, so 420 means 4.20.
#
# The reference floors are emitted at half the observed value, because
# a bare CAS is relatively dearer under TCG than natively and this
# ratio does move with the accelerator. They prove the measurement
# happened; the depth ratio below is the check.
`)
	floors := map[int64]int64{}
	var ratioDepth, ratioCap int64
	for _, l := range run.costLogs {
		ref := max(l.ref, 1)
		for depth, cost := range l.cost {
			floor := cost * 100 / ref / 2
			if old, ok := floors[depth]; !ok || floor < old {
				floors[depth] = floor
			}
		}
		shallow := l.cost[1]
		deepDepth := int64(0)
		for depth := range l.cost {
			deepDepth = max(deepDepth, depth)
		}
		if shallow > 0 && deepDepth > 1 {
			// A quarter above the observation: emitted exactly, the file
			// would fail the very next run, so re-measuring would not be a
			// remedy that survives being used.
			ratio := l.cost[deepDepth] * 125 / shallow
			if deepDepth > ratioDepth || (deepDepth == ratioDepth && ratio > ratioCap) {
				ratioDepth, ratioCap = deepDepth, ratio
			}
		}
	}
	for _, depth := range slices.Sorted(maps.Keys(floors)) {
		fmt.Fprintf(out, "min-charge-over-reference %d %d\n", depth, floors[depth])
	}
	if ratioDepth > 0 {
		fmt.Fprintf(out, "max-depth-cost-ratio %d %d\n", ratioDepth, ratioCap)
	}
	fmt.Fprint(out, "\n# A physical bound on the reference, not a measurement.\nmin-reference-cycles 20\n")
}
