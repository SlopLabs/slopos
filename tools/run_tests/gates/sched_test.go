package gates

import (
	"strings"
	"testing"
)

const schedGateText = `eligible-lag	boot	1
allow-ineligible	boot	0x1
require-dispatch	boot	0
eligible-lag	post-kernel-tests	0
allow-ineligible	post-kernel-tests	0x0
require-dispatch	post-kernel-tests	1
min-online	2
`

// Healthy: the BSP lags at boot and nowhere else.
var schedGood = lines(
	"SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe",
	"SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=1 online=1 eligible=1 switches=9 ticks=10 idle=9 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0xf",
	"SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=90 ticks=99 idle=3 pulled=1 pushed=2",
	"SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=1 switches=70 ticks=99 idle=9 pulled=2 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=1 switches=60 ticks=99 idle=8 pulled=0 pushed=1",
	"SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=1 switches=55 ticks=99 idle=7 pulled=0 pushed=0",
)

// The bug the gate exists for: three APs online, only the BSP eligible.
var schedRegressed = lines(
	"SCHEDCPU[boot]: cpus=4 online=0xf eligible=0x1",
	"SCHEDCPU[boot]: cpu=0 online=1 eligible=1 switches=3 ticks=10 idle=1 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=1 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=2 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0",
	"SCHEDCPU[boot]: cpu=3 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0x1",
	"SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=90 ticks=99 idle=3 pulled=0 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0",
	"SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0",
)

func schedFixture(t *testing.T) *fixture {
	f := newFixture(t)
	f.gateFile("sched", "tests", schedGateText)
	return f
}

func TestSchedSelfTestCases(t *testing.T) {
	f := schedFixture(t)
	good := f.log("good.log", schedGood)
	expect(t, "healthy log", f.run("sched", []string{good}), 0, "boot=3/4 post-kernel-tests=4/4")

	expect(t, "three ineligible CPUs", f.run("sched", []string{f.log("regressed.log", schedRegressed)}), 1, "are not eligible for placement")

	// The volume that broke the old gate in CI: the busiest CPU doing 20x the
	// least busy still passes, because only zero is a finding.
	lopsided := strings.NewReplacer(
		"cpu=0 online=1 eligible=1 switches=90 ", "cpu=0 online=1 eligible=1 switches=20056 ",
		"cpu=1 online=1 eligible=1 switches=70 ", "cpu=1 online=1 eligible=1 switches=874 ",
		"cpu=2 online=1 eligible=1 switches=60 ", "cpu=2 online=1 eligible=1 switches=1167 ",
		"cpu=3 online=1 eligible=1 switches=55 ", "cpu=3 online=1 eligible=1 switches=13761 ",
	).Replace(schedGood)
	expect(t, "lopsided but healthy", f.run("sched", []string{f.log("lopsided.log", lopsided)}), 0)

	idle := strings.Replace(schedGood, "cpu=1 online=1 eligible=1 switches=70 ", "cpu=1 online=1 eligible=1 switches=0 ", 1)
	expect(t, "an eligible CPU that never dispatched", f.run("sched", []string{f.log("idle.log", idle)}), 1, "cpu 1 dispatched nothing")

	f.gateFile("sched", "dead", "eligible-lag\tboot\t1\nallow-ineligible\tboot\t0x1\neligible-lag\tno-such-phase\t0\nmin-online\t2\n")
	expect(t, "dead gate entry", f.run("sched", []string{good}, "--variant", "dead"), 1, "gate names phase 'no-such-phase'")

	wrongCPU := f.log("wrong.log", lines(
		"SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xd",
		"SCHEDCPU[boot]: cpu=0 online=1 eligible=1 switches=3 ticks=10 idle=1 pulled=0 pushed=0",
		"SCHEDCPU[boot]: cpu=1 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0",
		"SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0",
		"SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0",
	))
	expect(t, "an AP standing in for the BSP's lag", f.run("sched", []string{wrongCPU}), 1, "unexpected=0x2")

	truncated := f.log("truncated.log", lines(
		"SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe",
		"SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0",
	))
	expect(t, "missing per-CPU rows", f.run("sched", []string{truncated}), 1, "no per-CPU row")

	wide := f.log("wide.log", "SCHEDCPU[boot]: cpus=64 online=0xffffffffffffffff eligible=0xffffffffffffffff\n")
	expect(t, "64-CPU mask", f.run("sched", []string{wide}, "--emit-allowlist"), 0, "min-online\t2")

	expect(t, "no SCHEDCPU lines", f.run("sched", []string{f.log("empty.log", "nothing to see here\n")}), 1, "no SCHEDCPU lines")

	f.emitInto("sched", "roundtrip", []string{good})
	expect(t, "emitted allowlist accepts its own input", f.run("sched", []string{good}, "--variant", "roundtrip"), 0)

	f.gateFile("sched", "typo", "eligible-lagg\tboot\t1\n")
	expect(t, "unknown gate key", f.run("sched", []string{good}, "--variant", "typo"), 2, "unknown gate key 'eligible-lagg'")

	f.removeGateFile("sched", "tests")
	expect(t, "no gate data", f.run("sched", []string{good}), 2, "no gate data")
}

func TestSchedLogs(t *testing.T) {
	f := schedFixture(t)
	a, b, c := f.log("a.log", schedGood), f.log("b.log", schedGood), f.log("c.log", schedGood)
	expect(t, "two logs", f.run("sched", []string{a, b}), 0, "a.log", "b.log")
	expect(t, "three logs", f.run("sched", []string{a, b, c}), 0)
	bad := f.log("bad.log", schedRegressed)
	expect(t, "one regressed boot fails the run and is named",
		f.run("sched", []string{a, bad, c}), 1, "bad.log: phase boot — 3 online CPU(s) are not eligible")

	// The emitter records the widest lag any boot showed; its file passes
	// every boot it was measured from.
	apLag := strings.Replace(strings.Replace(schedGood, "eligible=0xe", "eligible=0xd", 1),
		"SCHEDCPU[boot]: cpu=1 online=1 eligible=1", "SCHEDCPU[boot]: cpu=1 online=1 eligible=0", 1)
	apLag = strings.Replace(apLag, "SCHEDCPU[boot]: cpu=0 online=1 eligible=0", "SCHEDCPU[boot]: cpu=0 online=1 eligible=1", 1)
	d := f.log("d.log", apLag)
	out := f.emitInto("sched", "merged", []string{a, d})
	for _, want := range []string{"eligible-lag\tboot\t1\n", "allow-ineligible\tboot\t0x3\n", "require-dispatch\tpost-kernel-tests\t1\n"} {
		if !strings.Contains(out, want) {
			t.Errorf("emitted file missing %q:\n%s", want, out)
		}
	}
	expect(t, "merged emit round-trips", f.run("sched", []string{a, d}, "--variant", "merged"), 0)
}
