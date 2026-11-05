package gates

import (
	"fmt"
	"strings"
	"testing"
)

func lockdepLine(phase, state string, classes, pct, edges, chains, violations int) string {
	return fmt.Sprintf("LOCKDEP[%s]: %s classes=%d/508 (%d%%) edges=%d/1024 chains=%d/2048 held_max=3/16 held_drops=0 pop_miss=0/0 chain_hit=8 chain_miss=1 violations=%d reports=0 collisions=0 mode=Panic\r\n",
		phase, state, classes, pct, edges, chains, violations)
}

const lockdepFloors = "max-fill-pct 70\nmin-classes 32\nmin-edges 16\nmin-chains 32\n"

func lockdepFixture(t *testing.T, extra string) *fixture {
	f := newFixture(t)
	f.gateFile("lockdep", "tests", lockdepFloors+"require-phase boot\n"+extra)
	return f
}

func TestLockdepAccepts(t *testing.T) {
	f := lockdepFixture(t, "")
	clean := f.log("clean.log", lockdepLine("boot", "ACTIVE", 65, 12, 48, 112, 0))
	expect(t, "clean log accepted", f.run("lockdep", []string{clean}), 0, "OK — LOCKDEP[boot]")

	// The bands are gone: a scheduling-dependent count far outside what the
	// old file recorded is not a finding, in either direction.
	for _, c := range []struct {
		label         string
		edges, chains int
	}{{"many chains", 48, 400}, {"few edges above the floor", 20, 112}} {
		r := f.run("lockdep", []string{f.log("drift.log", lockdepLine("boot", "ACTIVE", 65, 12, c.edges, c.chains, 0))})
		expect(t, c.label, r, 0, "OK — LOCKDEP[boot]")
		expectNot(t, c.label, r, "DRIFT")
	}

	// 70% of 2048 is 1433.6: compared without truncation, 1433 passes.
	expect(t, "one under the ceiling passes",
		f.run("lockdep", []string{f.log("under.log", lockdepLine("boot", "ACTIVE", 65, 12, 48, 1433, 0))}), 0, "LOCKDEP[boot]")
}

// Runs of identical code registered different counts; the docs-only commits
// 407d1d69 and 38465620 failed on exactly that. Neither run may fail.
func TestLockdepCountsThatMoveBetweenRunsPass(t *testing.T) {
	f := lockdepFixture(t, "require-phase post-kernel-tests\n")
	a := f.log("a.log", lockdepLine("boot", "ACTIVE", 106, 20, 54, 148, 0)+lockdepLine("post-kernel-tests", "ACTIVE", 268, 52, 208, 466, 0))
	b := f.log("b.log", lockdepLine("boot", "ACTIVE", 107, 21, 59, 156, 0)+lockdepLine("post-kernel-tests", "ACTIVE", 270, 53, 173, 408, 0))
	expect(t, "run a", f.run("lockdep", []string{a}), 0)
	expect(t, "run b", f.run("lockdep", []string{b}), 0)
}

func TestLockdepRejects(t *testing.T) {
	cases := []struct {
		label, log, want string
	}{
		{"empty log", "", "nothing was measured"},
		{"fill ceiling", lockdepLine("boot", "ACTIVE", 65, 12, 48, 1600, 0), "over max-fill-pct"},
		{"stated fill ceiling is enforced", lockdepLine("boot", "ACTIVE", 65, 12, 48, 1434, 0), "over max-fill-pct"},
		{"zero edges", lockdepLine("boot", "ACTIVE", 65, 12, 0, 112, 0), "recorded only 0 edges"},
		{"chains under the floor", lockdepLine("boot", "ACTIVE", 65, 12, 48, 4, 0), "recorded only 4 chains"},
		{"inactive validator", lockdepLine("boot", "DISABLED (pool overflow)", 65, 12, 48, 112, 0), "not ACTIVE"},
		{"violation", lockdepLine("boot", "ACTIVE", 65, 12, 48, 112, 3), "violation(s)"},
		{"class floor", lockdepLine("boot", "ACTIVE", 4, 1, 48, 112, 0), "registered only 4 classes"},
		{"renamed field", "LOCKDEP[boot]: ACTIVE klasses=65/508 (12%) edges=48/1024 chains=112/2048 violations=0\r\n", "the report format moved"},
	}
	for _, c := range cases {
		f := lockdepFixture(t, "")
		expect(t, c.label, f.run("lockdep", []string{f.log("x.log", c.log)}), 1, c.want)
	}

	f := lockdepFixture(t, "")
	expect(t, "missing log", f.run("lockdep", []string{f.root + "/no-such.log"}), 1, "cannot read log")

	f = lockdepFixture(t, "require-phase post-userland-tests\n")
	clean := f.log("clean.log", lockdepLine("boot", "ACTIVE", 65, 12, 48, 112, 0))
	expect(t, "missing phase", f.run("lockdep", []string{clean}), 1, "never printed it")
}

func TestLockdepGateFileErrors(t *testing.T) {
	cases := []struct {
		label, gate, want string
	}{
		// What the old grammar graded is now refused, not ignored: a file
		// that still carries it describes a policy the gate no longer has.
		{"cap entry", lockdepFloors + "require-phase boot\nboot\tclasses\t65\n", "no longer graded"},
		{"dead cap entry", lockdepFloors + "require-phase boot\nghost\tedges\t1\n", "no longer graded"},
		{"band", lockdepFloors + "require-phase boot\nband boot edges 40 60\n", "no longer graded"},
		{"cap and band", lockdepFloors + "require-phase boot\nboot\tchains\t120\nband boot chains 100 120\n", "no longer graded"},
		{"unknown directive", lockdepFloors + "require-phase boot\nmax-fill-pcnt 70\n", "unknown directive 'max-fill-pcnt'"},
		{"missing floor", "max-fill-pct 70\nmin-classes 32\nmin-chains 32\nrequire-phase boot\n", "no 'min-edges'"},
		{"missing chain floor", "max-fill-pct 70\nmin-classes 32\nmin-edges 16\nrequire-phase boot\n", "no 'min-chains'"},
		{"no phase", lockdepFloors, "no require-phase"},
		{"twice", lockdepFloors + "min-edges 3\nrequire-phase boot\n", "given twice"},
		{"spare field", lockdepFloors + "require-phase boot extra\n", "want 'require-phase <value>'"},
		{"not a number", "max-fill-pct seventy\nmin-classes 32\nmin-edges 16\nmin-chains 32\nrequire-phase boot\n", "non-negative integer"},
	}
	for _, c := range cases {
		f := newFixture(t)
		f.gateFile("lockdep", "tests", c.gate)
		clean := f.log("clean.log", lockdepLine("boot", "ACTIVE", 65, 12, 48, 112, 0))
		expect(t, c.label, f.run("lockdep", []string{clean}), 2, c.want)
	}
	f := newFixture(t)
	clean := f.log("clean.log", lockdepLine("boot", "ACTIVE", 65, 12, 48, 112, 0))
	expect(t, "no gate data", f.run("lockdep", []string{clean}), 2, "no gate data")
}

func TestLockdepShards(t *testing.T) {
	f := lockdepFixture(t, "require-phase post-kernel-tests\n")
	shard := func(name string, kernelChains, violations int) string {
		return f.log(name, lockdepLine("boot", "ACTIVE", 106, 20, 54, 148, 0)+lockdepLine("post-kernel-tests", "ACTIVE", 268, 52, 200, kernelChains, violations))
	}
	a, b, c := shard("a.log", 400, 0), shard("b.log", 466, 0), shard("c.log", 430, 0)
	r := f.run("lockdep", []string{a, b})
	expect(t, "two shards", r, 0, "peak over 2 log(s)", "chains=466/2048")
	r = f.run("lockdep", []string{a, b, c})
	expect(t, "three shards", r, 0, "peak over 3 log(s)", "chains=466/2048")

	bad := shard("bad.log", 400, 1)
	expect(t, "a violation in one shard fails the run and names the shard",
		f.run("lockdep", []string{a, bad, c}), 1, "bad.log: LOCKDEP[post-kernel-tests] reported 1 violation(s)")

	short := f.log("short.log", lockdepLine("boot", "ACTIVE", 106, 20, 54, 148, 0))
	expect(t, "a shard missing a phase fails",
		f.run("lockdep", []string{a, short}), 1, "short.log: gate requires phase 'post-kernel-tests'")
}

func TestLockdepEmitRoundTrips(t *testing.T) {
	f := newFixture(t)
	a := f.log("a.log", lockdepLine("boot", "ACTIVE", 65, 12, 43, 110, 0)+lockdepLine("post-kernel-tests", "ACTIVE", 65, 12, 148, 340, 0))
	b := f.log("b.log", lockdepLine("boot", "ACTIVE", 65, 12, 43, 110, 0)+lockdepLine("post-kernel-tests", "ACTIVE", 65, 12, 160, 352, 0)+lockdepLine("post-userland-tests", "ACTIVE", 70, 13, 170, 360, 0))
	out := f.emitInto("lockdep", "tests", []string{a, b})
	for _, want := range []string{"require-phase boot\n", "require-phase post-kernel-tests\n", "max-fill-pct 70\n", "min-chains 32\n", "355 of 508 classes", "1433 of 2048 chains"} {
		if !strings.Contains(out, want) {
			t.Errorf("emitted file missing %q:\n%s", want, out)
		}
	}
	// Only a phase every boot printed is required, so the file passes the
	// logs it was measured from.
	if strings.Contains(out, "require-phase post-userland-tests") {
		t.Errorf("emitted a phase only one log printed:\n%s", out)
	}
	if strings.Contains(out, "band ") || strings.Contains(out, "\t") {
		t.Errorf("emitted a count cap or band:\n%s", out)
	}
	expect(t, "emitted file accepts its own logs", f.run("lockdep", []string{a, b}), 0)
}
