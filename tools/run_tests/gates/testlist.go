package gates

// The tests gate holds the suite to a tracked list of its test names.
//
// A sum of plan lines catches a refactor that deletes a hundred tests and
// nothing finer: one test deleted beside one added nets to zero, and a test
// registered twice adds one. Roughly 650 duplicate registrations went
// unnoticed that way. Names catch all three: a listed test that ran in no
// log, a test that ran but is not listed, and a name reported twice within
// one phase of one boot.
//
// Several logs are several whole runs: each log's names per phase must equal
// the list on their own, and duplicates are per log and phase, because a
// boot runs each of its tests once.

import (
	"fmt"
	"maps"
	"regexp"
	"slices"
	"strings"
)

func init() {
	register(Gate{
		Name:    "tests",
		Summary: "every top-level test is listed in scripts/gates/tests/<variant>.txt, ran, and ran once",
		Run:     runTestList,
	})
}

const ktapPrefix = "KTAP\t"

var (
	ktapPlanRE   = regexp.MustCompile(`^1\.\.(\d+)$`)
	ktapResultRE = regexp.MustCompile(`^(ok|not ok) (\d+) - (\S+)(?: # (.*))?$`)
	ansiRE       = regexp.MustCompile(`\x1b\[[0-9;?]*[A-Za-z]`)
	phaseNameRE  = regexp.MustCompile(`^[a-z][a-z0-9-]*$`)
)

// ktapPhaseNames follows the runner's convention: the kernel names its phases
// only in klog, so they are numbered off the wire, kernel first.
var ktapPhaseNames = []string{"kernel", "userland"}

type ktapPhase struct {
	name  string
	plan  bool
	tests []string
}

// ktapPhases reads one boot's top-level results, phase by phase. A phase
// opens at `KTAP\tTAP version 14`. Subtests are indented and never match.
func ktapPhases(c capture) []*ktapPhase {
	var phases []*ktapPhase
	open := func() {
		idx := len(phases)
		name := fmt.Sprintf("phase%d", idx+1)
		if idx < len(ktapPhaseNames) {
			name = ktapPhaseNames[idx]
		}
		phases = append(phases, &ktapPhase{name: name})
	}
	inLiteral := false
	for _, raw := range c.lines {
		line := ansiRE.ReplaceAllString(raw, "")
		// TTY tests write straight to the console, so a result line can
		// arrive as `helloKTAP\tok 994 - …`. Inside a captured-log literal a
		// `KTAP\t` substring is log content, not a result.
		if !inLiteral {
			if i := strings.Index(line, ktapPrefix); i > 0 {
				line = line[i:]
			}
		}
		body, ok := strings.CutPrefix(line, ktapPrefix)
		if !ok {
			inLiteral = false
			continue
		}
		if inLiteral {
			if body == "  ..." {
				inLiteral = false
			}
			continue
		}
		switch {
		case body == "  log: |":
			inLiteral = true
		case body == "TAP version 14":
			open()
		case ktapPlanRE.MatchString(body):
			if len(phases) == 0 {
				open()
			}
			phases[len(phases)-1].plan = true
		default:
			if m := ktapResultRE.FindStringSubmatch(body); m != nil {
				if len(phases) == 0 {
					open()
				}
				cur := phases[len(phases)-1]
				cur.tests = append(cur.tests, m[3])
			}
		}
	}
	return phases
}

// maxListed bounds how many names one finding category prints; the count is
// always exact.
const maxListed = 40

func listCapped(f *findings, header string, items []string, hint string) {
	if len(items) == 0 {
		return
	}
	f.add("%d %s", len(items), header)
	for i, it := range items {
		if i == maxListed {
			f.add("    … and %d more", len(items)-maxListed)
			break
		}
		f.add("    %s", it)
	}
	if hint != "" {
		f.add("  %s", hint)
	}
}

func runTestList(cfg Config) error {
	caps, err := loadCaptures(cfg.Logs)
	if err != nil {
		return err
	}
	var fail findings
	var seen []map[string]bool
	var phaseOrder []string
	var duplicates []string
	for _, c := range caps {
		phases := ktapPhases(c)
		ran := map[string]bool{}
		seen = append(seen, ran)
		planned := false
		for _, ph := range phases {
			planned = planned || ph.plan
			if !slices.Contains(phaseOrder, ph.name) {
				phaseOrder = append(phaseOrder, ph.name)
			}
			count := map[string]int{}
			for _, t := range ph.tests {
				count[t]++
				ran[ph.name+"\t"+t] = true
			}
			for _, t := range sortedKeys(count) {
				if count[t] > 1 {
					duplicates = append(duplicates, fmt.Sprintf("%s %s ×%d in %s", ph.name, t, count[t], c.path))
				}
			}
		}
		if !planned {
			fail.add("%s: no KTAP plan line was emitted — nothing was measured. The kernel did not reach the harness (boot panic, missing ISO, or a stale run_tests); read the log.", c.path)
		}
	}
	if len(fail) > 0 {
		return fail.err()
	}

	if cfg.Emit {
		for i, c := range caps[1:] {
			if !maps.Equal(seen[0], seen[i+1]) {
				return fmt.Errorf("%s and %s ran different tests — every log is a whole run, so the list can only be recorded from runs that agree", caps[0].path, c.path)
			}
		}
		emitTestList(cfg, seen[0])
		return nil
	}

	rel, lines, err := readGateFile(cfg, "tests")
	if err != nil {
		return err
	}
	listed := map[string]bool{}
	prev := ""
	for _, l := range lines {
		phase, name, ok := strings.Cut(l.text, "\t")
		if !ok || !phaseNameRE.MatchString(phase) || name == "" || strings.ContainsAny(name, " \t") {
			return gateFileErrorf("%s:%d: want '<phase> <TAB> <module::name>', got %q", rel, l.no, l.text)
		}
		if l.text <= prev {
			return gateFileErrorf("%s:%d: %q is out of order or repeated — the list is sorted and unique; re-emit it", rel, l.no, l.text)
		}
		prev = l.text
		listed[l.text] = true
	}
	if len(listed) == 0 {
		return gateFileErrorf("%s lists no tests, so it grades nothing", rel)
	}

	where := func(key string, c capture) string {
		item := strings.Replace(key, "\t", " ", 1)
		if len(caps) > 1 {
			item += " in " + c.path
		}
		return item
	}
	var missing, unlisted []string
	for i, c := range caps {
		for _, key := range sortedKeys(listed) {
			if !seen[i][key] {
				missing = append(missing, where(key, c))
			}
		}
		for _, key := range sortedKeys(seen[i]) {
			if !listed[key] {
				unlisted = append(unlisted, where(key, c))
			}
		}
	}
	listCapped(&fail, "listed test(s) did not run — a deleted or renamed test:", missing,
		"If the removal is intended, delete the line(s) from "+rel+" in the same commit.")
	listCapped(&fail, "test(s) ran but are not listed in "+rel+":", unlisted,
		"Record them: "+emitCommand("tests", cfg.Variant))
	listCapped(&fail, "test name(s) reported more than once within one phase of one boot — a test registered twice; drop the extra registration:", duplicates, "")
	if len(fail) > 0 {
		return fail.err()
	}

	perPhase := map[string]int{}
	for key := range listed {
		phase, _, _ := strings.Cut(key, "\t")
		perPhase[phase]++
	}
	var parts []string
	for _, ph := range phaseOrder {
		parts = append(parts, fmt.Sprintf("%d %s", perPhase[ph], ph))
	}
	fmt.Fprintf(cfg.Out, "tests: OK — %s tests ran in each of %d log(s), each once per boot and each listed in %s\n",
		strings.Join(parts, " + "), len(caps), rel)
	return nil
}

func emitTestList(cfg Config, seen map[string]bool) {
	fmt.Fprintf(cfg.Out, `# tests gate data — variant: %s
#
#     %s
#
# Every top-level test the suite runs: one '<phase> <TAB> <module::name>'
# line, sorted, phase 'kernel' or 'userland'. The gate fails on a listed test
# that ran in no log (a test deleted by a refactor), on a test that ran but is
# not listed (record it by re-emitting), and on a name reported twice within
# one phase of one boot (one test registered twice). A '# SKIP' result counts
# as present; subtests indented under a utest do not count.
#
# Explicit tests run only when named, so they never appear in the CI capture
# and are not listed here.

`, cfg.Variant, emitCommand("tests", cfg.Variant))
	for _, key := range sortedKeys(seen) {
		fmt.Fprintln(cfg.Out, key)
	}
}

func sortedKeys[V any](m map[string]V) []string {
	return slices.Sorted(maps.Keys(m))
}
