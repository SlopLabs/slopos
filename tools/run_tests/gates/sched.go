package gates

// The sched gate holds every boot to one invariant: a CPU that is online is
// eligible for task placement.
//
// Every placement helper filters candidates through is_schedulable_cpu, so a
// CPU whose runqueue is online but not enabled vanishes from every fork, exec
// and wakeup decision while still dispatching whatever work stealing hands it.
// The machine boots, the suite passes, and one core does all the work — as it
// does when a boot step resets the runqueues of APs already in their scheduler
// loops.
//
// The gate file names, per phase, how many online CPUs may lag (eligible-lag:
// the BSP enters its scheduler loop only after boot init), which CPUs that
// lag may name (allow-ineligible: a count alone accepts an AP dropping out as
// readily as the BSP), and whether every online CPU must have dispatched
// something (require-dispatch: zero versus non-zero, never a volume — how many
// switches a CPU makes is the workload's and the machine's business).
//
// Aggregation: eligibility is a property of one boot, so each log is graded on
// its own against the whole gate file.

import (
	"fmt"
	"math/bits"
	"regexp"
	"slices"
	"strconv"
	"strings"
)

func init() {
	register(Gate{
		Name:    "sched",
		Summary: "every online CPU is eligible for placement (bar the recorded lag), on every boot",
		Run:     runSched,
	})
}

var (
	schedMarker    = regexp.MustCompile(`SCHEDCPU\[`)
	schedSummaryRE = regexp.MustCompile(`^SCHEDCPU\[([a-z-]+)\]: cpus=(\d+) online=0x([0-9a-f]+) eligible=0x([0-9a-f]+)`)
	schedCPURE     = regexp.MustCompile(`^SCHEDCPU\[([a-z-]+)\]: cpu=(\d+) online=([01]) eligible=([01]) switches=(\d+) ticks=(\d+) idle=(\d+) pulled=(\d+) pushed=(\d+)`)
)

type schedCPU struct {
	online   bool
	switches int64
}

type schedPhase struct {
	cpus             int64
	online, eligible uint64
	rows             map[int64]schedCPU
}

func (p *schedPhase) lagMask() uint64 { return p.online &^ p.eligible }

type schedLog struct {
	path   string
	order  []string
	phases map[string]*schedPhase
}

func parseHexMask(s string) uint64 {
	v, err := strconv.ParseUint(s, 16, 64)
	if err != nil {
		return ^uint64(0)
	}
	return v
}

func parseSched(c capture) (*schedLog, error) {
	l := &schedLog{path: c.path, phases: map[string]*schedPhase{}}
	phase := func(name string) *schedPhase {
		p := l.phases[name]
		if p == nil {
			p = &schedPhase{rows: map[int64]schedCPU{}}
			l.phases[name] = p
		}
		return p
	}
	found := false
	for _, line := range c.lines {
		text, ok := reportAt(line, schedMarker)
		if !ok {
			continue
		}
		if m := schedSummaryRE.FindStringSubmatch(text); m != nil {
			found = true
			if !slices.Contains(l.order, m[1]) {
				l.order = append(l.order, m[1])
			}
			p := phase(m[1])
			p.cpus = num(m[2])
			p.online = parseHexMask(m[3])
			p.eligible = parseHexMask(m[4])
		} else if m := schedCPURE.FindStringSubmatch(text); m != nil {
			phase(m[1]).rows[num(m[2])] = schedCPU{online: m[3] == "1", switches: num(m[5])}
		}
	}
	if !found {
		return nil, fmt.Errorf("%s: no SCHEDCPU lines — the kernel emits them from sched_cpu_report(); a log with none is a boot that never reached a report site, not a healthy run.", c.path)
	}
	return l, nil
}

func bit(cpu int64) uint64 {
	if cpu < 0 || cpu >= 64 {
		return 0
	}
	return 1 << uint(cpu)
}

type schedGate struct {
	lag       map[string]int64
	allow     map[string]uint64
	dispatch  map[string]string
	minOnline int64
}

func readSchedGate(cfg Config) (schedGate, error) {
	g := schedGate{lag: map[string]int64{}, allow: map[string]uint64{}, dispatch: map[string]string{}, minOnline: 1}
	rel, lines, err := readGateFile(cfg, "sched")
	if err != nil {
		return g, err
	}
	for _, l := range lines {
		f := strings.Fields(l.text)
		for len(f) < 3 {
			f = append(f, "")
		}
		switch f[0] {
		case "eligible-lag":
			n, err := parseCount(rel, l, f[2])
			if err != nil {
				return g, err
			}
			g.lag[f[1]] = n
		case "allow-ineligible":
			m, err := strconv.ParseUint(f[2], 0, 64)
			if err != nil {
				return g, gateFileErrorf("%s:%d: want a CPU mask, got %q", rel, l.no, f[2])
			}
			g.allow[f[1]] = m
		case "require-dispatch":
			g.dispatch[f[1]] = f[2]
		case "min-online":
			n, err := parseCount(rel, l, f[1])
			if err != nil {
				return g, err
			}
			g.minOnline = n
		default:
			return g, gateFileErrorf("%s:%d: unknown gate key '%s'", rel, l.no, f[0])
		}
	}
	return g, nil
}

func runSched(cfg Config) error {
	caps, err := loadCaptures(cfg.Logs)
	if err != nil {
		return err
	}
	logs := make([]*schedLog, len(caps))
	for i, c := range caps {
		if logs[i], err = parseSched(c); err != nil {
			return err
		}
	}
	if cfg.Emit {
		emitSched(cfg, logs)
		return nil
	}
	g, err := readSchedGate(cfg)
	if err != nil {
		return err
	}
	var fail findings
	for _, l := range logs {
		gradeSched(&fail, g, l)
	}
	if len(fail) > 0 {
		return fail.err()
	}
	for _, l := range logs {
		var summary []string
		for _, name := range l.order {
			p := l.phases[name]
			summary = append(summary, fmt.Sprintf("%s=%d/%d", name, bits.OnesCount64(p.eligible), bits.OnesCount64(p.online)))
		}
		fmt.Fprintf(cfg.Out, "sched: OK — variant=%s %s, eligible/online per phase: %s\n", cfg.Variant, l.path, strings.Join(summary, " "))
	}
	return nil
}

func gradeSched(fail *findings, g schedGate, l *schedLog) {
	for _, phase := range sortedKeys(g.lag) {
		if !slices.Contains(l.order, phase) {
			fail.add("%s: gate names phase '%s', which the run never reported — a dead entry is an exemption for something that no longer happens.", l.path, phase)
		}
	}
	for _, name := range l.order {
		p := l.phases[name]
		nOnline := int64(bits.OnesCount64(p.online))
		lagMask := p.lagMask()
		lag := int64(bits.OnesCount64(lagMask))
		want := g.lag[name]
		if nOnline < g.minOnline {
			fail.add("%s: phase %s has %d online CPU(s), floor is %d — a run that brought up fewer CPUs cannot demonstrate the invariant.", l.path, name, nOnline, g.minOnline)
		}
		allow := g.allow[name]
		unexpected := lagMask &^ allow
		if lag > want || unexpected != 0 {
			fail.add("%s: phase %s — %d online CPU(s) are not eligible for placement (allowed %d)\n  online=0x%x eligible=0x%x ineligible=0x%x permitted=0x%x unexpected=0x%x\n  Every task placement filters on this set, so those CPUs receive work only by work stealing. Find what left their runqueue disabled; do not raise the number.",
				l.path, name, lag, want, p.online, p.eligible, lagMask, allow, unexpected)
		}
		// Before the dispatch check: a missing row is not an idle CPU, and
		// defaulting it to offline would let a truncated report grade clean.
		var missing uint64
		for cpu := int64(0); cpu < p.cpus; cpu++ {
			if (p.online|p.eligible)&bit(cpu) == 0 {
				continue
			}
			row, ok := p.rows[cpu]
			if !ok {
				missing |= bit(cpu)
				continue
			}
			if !row.online || g.dispatch[name] != "1" {
				continue
			}
			if row.switches == 0 {
				fail.add("%s: phase %s cpu %d dispatched nothing — an online CPU that never dispatches is a CPU nothing was placed on.", l.path, name, cpu)
			}
		}
		if missing != 0 {
			fail.add("%s: phase %s reported no per-CPU row for 0x%x\n  online=0x%x eligible=0x%x — an incomplete report cannot be graded.", l.path, name, missing, p.online, p.eligible)
		}
	}
}

// emitSched records, per phase, the widest lag any boot showed: the largest
// count, the union of the lagging CPUs, and require-dispatch only where every
// boot had every online CPU dispatch.
func emitSched(cfg Config, logs []*schedLog) {
	type agg struct {
		lag      int
		mask     uint64
		dispatch bool
	}
	var order []string
	phases := map[string]*agg{}
	for _, l := range logs {
		for _, name := range l.order {
			p := l.phases[name]
			a := phases[name]
			if a == nil {
				a = &agg{dispatch: true}
				phases[name] = a
				order = append(order, name)
			}
			a.lag = max(a.lag, bits.OnesCount64(p.lagMask()))
			a.mask |= p.lagMask()
			seen := false
			for cpu := int64(0); cpu < p.cpus; cpu++ {
				row, ok := p.rows[cpu]
				if !ok || !row.online {
					continue
				}
				seen = true
				if row.switches == 0 {
					a.dispatch = false
				}
			}
			if !seen {
				a.dispatch = false
			}
		}
	}
	fmt.Fprintf(cfg.Out, `# Measured by: %s
#
# eligible-lag     <phase> <n>  online CPUs allowed to be ineligible
# allow-ineligible <phase> <m>  which CPUs that lag may name
# require-dispatch <phase> <b>  every online CPU must dispatch something
# min-online       <n>          floor on the online CPU count
#
# A lag above 0 outside the boot phase is a bug, not a number to raise:
# it means a CPU is dispatching work that no placement decision can see.
# require-dispatch is zero-versus-non-zero, never a volume: how many
# switches a CPU makes varies with the machine, so a floor above zero
# fails on a runner that is merely faster or slower.

`, emitCommand("sched", cfg.Variant))
	for _, name := range order {
		a := phases[name]
		dispatch := 0
		if a.dispatch {
			dispatch = 1
		}
		fmt.Fprintf(cfg.Out, "eligible-lag\t%s\t%d\n", name, a.lag)
		fmt.Fprintf(cfg.Out, "allow-ineligible\t%s\t0x%x\n", name, a.mask)
		fmt.Fprintf(cfg.Out, "require-dispatch\t%s\t%d\n", name, dispatch)
	}
	// Chosen, not measured: two CPUs is the least that can demonstrate the
	// invariant at all.
	fmt.Fprint(cfg.Out, "\nmin-online\t2\n")
}
