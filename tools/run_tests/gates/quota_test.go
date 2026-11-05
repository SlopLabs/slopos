package gates

import (
	"fmt"
	"strings"
	"testing"
)

func quotaLine(phase, mode string, slot int, kind string, used, peak, limit, denials int) string {
	return fmt.Sprintf("QUOTA[%s]: mode=%s slot=%d kind=%s used=%d peak=%d limit=%d denials=%d\r\n",
		phase, mode, slot, kind, used, peak, limit, denials)
}

const pu = "post-userland-tests"

var quotaClean = quotaLine(pu, "warn", 0, "fdslot", 3, 257, -1, 0) +
	quotaLine(pu, "warn", 7, "fdslot", 0, 18, -1, 0) +
	quotaLine(pu, "warn", 0, "objectrow", 1, 257, -1, 0)

const quotaGoodGate = "min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\npost-userland-tests\tobjectrow\t257\n"

func cost(depth, cycles int) string {
	return fmt.Sprintf("QUOTACOST: depth=%d cycles_per_charge=%d\r\n", depth, cycles)
}
func ref(cycles int) string { return fmt.Sprintf("QUOTACOST: reference cycles_per_op=%d\r\n", cycles) }

// TestQuotaSelfTestCases ports every case of the script's self-test, numbered
// as it numbered them.
func TestQuotaSelfTestCases(t *testing.T) {
	f := newFixture(t)
	clean := f.log("clean.log", quotaClean)
	denied := f.log("denied.log", quotaLine(pu, "warn", 0, "fdslot", 3, 257, -1, 0)+
		quotaLine(pu, "warn", 7, "fdslot", 0, 18, 64, 4)+
		quotaLine(pu, "warn", 0, "objectrow", 1, 257, -1, 0))
	refusals := func(name string, n int) string {
		return f.log(name, quotaClean+quotaLine(pu, "warn", 0, "commitpages", 3, 257, 300, n))
	}
	refused := refusals("refused.log", 2)
	expectGate := quotaGoodGate + "expect-denials post-userland-tests commitpages 2\npost-userland-tests\tcommitpages\t257\n"

	cases := []struct {
		label, gate, log string
		code             int
		want             string
	}{
		{"1 clean log accepted", quotaGoodGate, clean, 0, "OK — post-userland-tests/fdslot"},
		{"2 peak == cap accepted", quotaGoodGate, clean, 0, "OK —"},
		{"3 empty log rejected", quotaGoodGate, f.log("empty.log", ""), 1, "nothing was measured"},
		{"4 missing phase rejected", "min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\nrequire-phase boot\npost-userland-tests\tfdslot\t257\npost-userland-tests\tobjectrow\t257\n", clean, 1, "never printed it"},
		{"5 dead entry rejected", quotaGoodGate + "poste\tghost\t1\n", clean, 1, "dead entry"},
		{"6 over-cap rejected", "min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t256\npost-userland-tests\tobjectrow\t257\n", clean, 1, "over the recorded cap"},
		{"7 zero peak rejected", quotaGoodGate, f.log("zero.log", quotaLine(pu, "warn", 0, "fdslot", 0, 0, -1, 0)+quotaLine(pu, "warn", 0, "objectrow", 1, 257, -1, 0)), 1, "peaked at 0"},
		{"8 denial rejected", quotaGoodGate, denied, 1, "denial(s), over max-denials"},
		{"9 min-kinds floor rejected", "min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\n", f.log("onekind.log", quotaLine(pu, "warn", 0, "fdslot", 3, 257, -1, 0)), 1, "min 2"},
		{"10 renamed field rejected", quotaGoodGate, f.log("moved.log", "QUOTA[post-userland-tests]: slot=0 kind=fdslot high_water=257\r\n"), 1, "the report format moved"},
		{"11 unknown directive rejected", "min-kinds 2\nmax-denialz 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\n", clean, 2, "unknown directive"},
		{"12 collapsed charge cost rejected", quotaGoodGate + "min-charge-over-reference 1 300\n", f.log("collapsed.log", quotaClean+cost(1, 150)+ref(100)), 1, "under the floor 300"},
		{"13 missing charge cost rejected", quotaGoodGate + "min-charge-over-reference 4 300\n", f.log("collapsed2.log", quotaClean+cost(1, 150)+ref(100)), 1, "printed none"},
		{"14 honest charge accepted", quotaGoodGate + "min-charge-over-reference 1 300\n", f.log("fast.log", quotaClean+cost(1, 1500)+ref(100)), 0, "charge cost at depth 1"},
		{"14a a slower host does not move the verdict", quotaGoodGate + "min-charge-over-reference 1 300\n", f.log("slow-host.log", quotaClean+cost(1, 15000)+ref(1000)), 0, "charge cost at depth 1"},
		{"14b missing reference rejected", quotaGoodGate + "min-charge-over-reference 1 300\n", f.log("noref.log", quotaClean+cost(1, 1500)), 1, "no QUOTACOST reference line"},
		{"14c non-amortising walk rejected", quotaGoodGate + "min-charge-over-reference 1 300\nmin-charge-over-reference 7 300\nmax-depth-cost-ratio 7 500\n", f.log("nonamortised.log", quotaClean+cost(1, 1000)+cost(7, 7000)+ref(100)), 1, "over the recorded cap 500"},
		{"14d amortising walk accepted", quotaGoodGate + "min-charge-over-reference 1 300\nmin-charge-over-reference 7 300\nmax-depth-cost-ratio 7 500\n", f.log("amortised.log", quotaClean+cost(1, 1000)+cost(7, 4000)+ref(100)), 0, "costs 400x100 of depth 1"},
		{"14e a slower host does not excuse a non-amortising walk", quotaGoodGate + "min-charge-over-reference 1 300\nmin-charge-over-reference 7 300\nmax-depth-cost-ratio 7 500\n", f.log("nonamortised-slow.log", quotaClean+cost(1, 10000)+cost(7, 70000)+ref(1000)), 1, "over the recorded cap 500"},
		{"14f collapsed reference rejected", quotaGoodGate + "min-charge-over-reference 1 300\nmin-reference-cycles 20\n", f.log("noref-cycles.log", quotaClean+cost(1, 15)+ref(1)), 1, "under the floor 20"},
		{"14g honest reference accepted", quotaGoodGate + "min-charge-over-reference 1 300\nmin-reference-cycles 20\n", f.log("honest-ref.log", quotaClean+cost(1, 1500)+ref(100)), 0, "charge cost at depth 1"},
		{"14h dead min-kinds-for rejected", quotaGoodGate + "min-kinds-for ghost 3\n", clean, 1, "dead entry"},
		{"16 expected refusals accepted", expectGate, refused, 0, "OK —"},
		{"16a a refusal that stopped happening rejected", expectGate, refusals("underrefused.log", 1), 1, "want exactly 2"},
		{"16b an undeclared refusal rejected", expectGate, refusals("overrefused.log", 3), 1, "want exactly 2"},
		{"16c expect-denials on an untested kind refused", quotaGoodGate + "expect-denials post-userland-tests fdslot 4\n", denied, 2, "not a tested property"},
		{"16c' an expected refusal does not excuse another kind's denial", expectGate, f.log("refused-and-denied.log", strings.Join([]string{
			quotaLine(pu, "warn", 0, "fdslot", 3, 257, -1, 0), quotaLine(pu, "warn", 7, "fdslot", 0, 18, 64, 4),
			quotaLine(pu, "warn", 0, "objectrow", 1, 257, -1, 0), quotaLine(pu, "warn", 0, "commitpages", 3, 257, 300, 2),
		}, "")), 1, "denial(s), over max-denials"},
		{"16d dead expect-denials rejected", quotaGoodGate + "expect-denials post-userland-tests commitpages 1\n", clean, 1, "dead entry"},
		{"malformed cap row", quotaGoodGate + "post-userland-tests\tfdslot\n", clean, 2, "want '<phase> <TAB> <kind> <TAB> <cap>'"},
	}
	for _, c := range cases {
		f.gateFile("quota", "tests", c.gate)
		expect(t, c.label, f.run("quota", []string{c.log}), c.code, c.want)
	}

	// 14i. The emitter's own output passes the check path on the same log,
	// including the depth cap it emits a quarter above the observation.
	roundtrip := f.log("roundtrip.log", quotaClean+cost(1, 1500)+cost(7, 6000)+ref(100))
	out := f.emitInto("quota", "tests", []string{roundtrip})
	for _, want := range []string{"min-charge-over-reference 1 750\n", "min-charge-over-reference 7 3000\n", "max-depth-cost-ratio 7 500\n", "min-reference-cycles 20\n"} {
		if !strings.Contains(out, want) {
			t.Errorf("emit missing %q:\n%s", want, out)
		}
	}
	expect(t, "14i emit round-trips", f.run("quota", []string{roundtrip}), 0, "OK —", "depth 7 costs 400x100 of depth 1 (cap 500)")
	// The quarter is the margin: a run 25% dearer at depth is still in.
	expect(t, "14i the emitted depth cap carries its margin",
		f.run("quota", []string{f.log("dearer.log", quotaClean+cost(1, 1500)+cost(7, 7500)+ref(100))}), 0)
	expect(t, "14i and nothing past it",
		f.run("quota", []string{f.log("dearest.log", quotaClean+cost(1, 1500)+cost(7, 7560)+ref(100))}), 1, "over the recorded cap 500")

	// 16e. The emitter writes the expect-denials line from the log.
	out = f.emitInto("quota", "tests", []string{refused})
	if !strings.Contains(out, "\nexpect-denials post-userland-tests commitpages 2\n") {
		t.Errorf("emit missing the expect-denials line:\n%s", out)
	}
	expect(t, "16e emit expect-denials round-trips", f.run("quota", []string{refused}), 0, "OK —")

	// An untested kind's denial is never emitted as expected.
	out = f.emitInto("quota", "tests", []string{denied})
	if strings.Contains(out, "expect-denials") || !strings.Contains(out, "# not emitting post-userland-tests/fdslot's 4 denial(s)") {
		t.Errorf("emit handled an untested kind's denial wrongly:\n%s", out)
	}

	// 15. No gate data at all for the variant.
	f.removeGateFile("quota", "tests")
	expect(t, "15 missing gate data rejected", f.run("quota", []string{clean}), 2, "no gate data")
}

// quotaShards builds n shard logs of one run: each boots (boot row), and the
// suite's refusals are spread across the shards that ran the refusing tests.
func quotaShards(f *fixture, prefix string, kernelDenials, userDenials []int, costIn int) []string {
	var out []string
	for i := range kernelDenials {
		text := quotaLine("boot", "enforce", 0, "kernelmeta", 1701, 1701, -1, 0) +
			quotaLine("post-kernel-tests", "enforce", 0, "commitpages", 0, 2000+i, 178935, kernelDenials[i]) +
			quotaLine("post-kernel-tests", "enforce", 0, "fdslot", 0, 256, -1, 0) +
			quotaLine(pu, "enforce", 0, "commitpages", 0, 170000+i, 178935, kernelDenials[i]+userDenials[i]) +
			quotaLine(pu, "enforce", 0, "fdslot", 0, 800+i, -1, 0)
		if i == costIn {
			text += cost(1, 3893) + cost(7, 10792) + ref(131)
		}
		out = append(out, f.log(fmt.Sprintf("%s-%d.log", prefix, i), text))
	}
	return out
}

const quotaShardGate = `min-kinds 2
min-kinds-for boot 1
max-denials 0
expect-denials post-kernel-tests commitpages 4
expect-denials post-userland-tests commitpages 7
require-phase boot
require-phase post-kernel-tests
require-phase post-userland-tests
boot	kernelmeta	1802
post-kernel-tests	commitpages	2564
post-kernel-tests	fdslot	256
post-userland-tests	commitpages	193853
post-userland-tests	fdslot	862
min-charge-over-reference 1 982
min-charge-over-reference 7 3759
max-depth-cost-ratio 7 478
min-reference-cycles 20
`

func TestQuotaShards(t *testing.T) {
	f := newFixture(t)
	f.gateFile("quota", "tests", quotaShardGate)

	single := quotaShards(f, "one", []int{4}, []int{3}, 0)
	expect(t, "one log", f.run("quota", single), 0, "post-userland-tests/commitpages peak=170000 denials=7")

	two := quotaShards(f, "two", []int{3, 1}, []int{0, 3}, 1)
	expect(t, "two shards: denials summed, peaks maxed", f.run("quota", two), 0,
		"post-kernel-tests/commitpages peak=2001 denials=4", "post-userland-tests/commitpages peak=170001 denials=7", "two-1.log: charge cost at depth 1")

	three := quotaShards(f, "three", []int{0, 4, 0}, []int{2, 0, 1}, 2)
	expect(t, "three shards", f.run("quota", three), 0, "post-userland-tests/commitpages peak=170002 denials=7")

	short := quotaShards(f, "short", []int{3, 0}, []int{0, 3}, 0)
	expect(t, "a refusal missing from the shard set fails", f.run("quota", short), 1, "post-kernel-tests/commitpages recorded 3 denial(s), want exactly 4")

	over := quotaShards(f, "over", []int{4, 0}, []int{3, 0}, 0)
	over = append(over, f.log("over-hot.log", quotaLine("boot", "enforce", 0, "kernelmeta", 1, 1803, -1, 0)+
		quotaLine("post-kernel-tests", "enforce", 0, "fdslot", 0, 1, -1, 0)+quotaLine(pu, "enforce", 0, "fdslot", 0, 1, -1, 0)))
	expect(t, "one shard's peak over its cap fails", f.run("quota", over), 1, "boot/kernelmeta peaked at 1803, over the recorded cap 1802")

	noUser := quotaShards(f, "nouser", []int{4, 0}, []int{3, 0}, 0)
	noUser = append(noUser, f.log("nouser-x.log", quotaLine("boot", "enforce", 0, "kernelmeta", 1, 1701, -1, 0)))
	expect(t, "every boot prints every required phase", f.run("quota", noUser), 1, "nouser-x.log: gate requires phase 'post-kernel-tests'")

	noCost := quotaShards(f, "nocost", []int{4, 0}, []int{3, 0}, -1)
	expect(t, "a cost floor fails when no shard carries the measurement", f.run("quota", noCost), 1, "every log: no QUOTACOST reference line")

	// Every shard replays the same boot and the later rows count from it, so
	// a boot denial cannot be summed: it fails loudly in a sharded run.
	bootDenied := quotaShards(f, "bootdenied", []int{4, 0}, []int{3, 0}, 0)
	bootDenied[1] = f.log("bootdenied-1.log", quotaLine("boot", "enforce", 0, "commitpages", 1, 5, 9, 1)+
		quotaLine("boot", "enforce", 0, "kernelmeta", 1701, 1701, -1, 0)+
		quotaLine("post-kernel-tests", "enforce", 0, "fdslot", 0, 256, -1, 0)+quotaLine(pu, "enforce", 0, "fdslot", 0, 800, -1, 0))
	expect(t, "a boot denial in a sharded run is refused", f.run("quota", bootDenied), 1, "boot/commitpages recorded 1 denial(s) in a run of 2 logs")
	f.gateFile("quota", "single", "min-kinds 1\nmax-denials 0\nexpect-denials boot commitpages 1\nrequire-phase boot\nboot\tcommitpages\t5\nboot\tkernelmeta\t1802\n")
	expect(t, "...while one log grades it as before", f.run("quota", bootDenied[1:], "--variant", "single"), 0, "boot/commitpages peak=5 denials=1")

	// The emitter over shards writes the suite-wide sums and passes them.
	f.emitInto("quota", "merged", three)
	expect(t, "emit over shards round-trips", f.run("quota", three, "--variant", "merged"), 0, "denials=7")
}
