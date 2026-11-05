package gates

import (
	"strings"
	"testing"
)

const fsperfGood = `noise before
FSPERF[tests]: bytes=4194304 txns=16 commits=16 devwrites=64 devblocks=1024 barriers=16 ns=200000000 rawbytes=4194304 rawns=100000000
noise after
`

const fsperfGate = `max-txns-per-mib	tests	4
max-commits-per-mib	tests	4
max-devwrites-per-mib	tests	16
max-devblocks-per-mib	tests	256
max-barriers-per-mib	tests	4
min-fs-over-raw-pct	tests	50
min-bytes	tests	1048576
`

const fsperfCapacityGate = `max-txns-per-mib	tests	4
min-bytes	tests	1048576
max-mount-reads-per-gib	capacity	64
min-volume-gib	capacity	16
min-files	capacity	4000
min-treebytes	capacity	800000000
`

const fscapLine = "FSCAP[capacity]: blocks=4194304 blocksize=4096 groups=128 cacheentries=4096 mountreads=512 mountns=900000000 dirents=20000 lookupreads=3 bytes=8388608 ns=400000000 files=5447 treebytes=1105596389\n"

func TestFsperfSelfTestCases(t *testing.T) {
	f := newFixture(t)
	good := f.log("good.log", fsperfGood)
	variant := func(name, from, to string) string { return f.log(name, strings.Replace(fsperfGood, from, to, 1)) }
	cases := []struct {
		label, gate, log string
		code             int
		extra            []string
	}{
		{"a log inside its caps", fsperfGate, good, 0, nil},
		{"a transaction per 4 KiB", fsperfGate, variant("chunky.log", "txns=16", "txns=1024"), 1, nil},
		{"a write moving twice its sectors", fsperfGate, variant("amplified.log", "devblocks=1024", "devblocks=2048"), 1, nil},
		{"a two-field gate row", fsperfGate + "max-txns-per-mib\ttests\n", good, 2, nil},
		{"a four-field gate row", fsperfGate + "max-txns-per-mib\ttests\t4\t4\n", good, 2, nil},
		{"a misspelled gate key", fsperfGate + "max-devblock-per-mib\ttests\t256\n", good, 2, nil},
		{"half the graded rate ratio", "min-fs-over-raw-pct\ttests\t75\nmin-bytes\ttests\t1048576\n", variant("slow.log", " ns=200000000", " ns=400000000"), 1, nil},
		{"a uniformly slower machine", fsperfGate, f.log("noaccel.log", strings.NewReplacer(" ns=200000000", " ns=600000000", "rawns=100000000", "rawns=300000000").Replace(fsperfGood)), 0, nil},
		{"a run that wrote 4 KiB", fsperfGate, f.log("empty.log", strings.NewReplacer("bytes=4194304 txns", "bytes=4096 txns", "txns=16", "txns=1").Replace(fsperfGood)), 1, nil},
		{"a dead gate entry", "max-txns-per-mib\ttests\t4\nmin-bytes\ttests\t1048576\nmax-txns-per-mib\tghost\t4\n", good, 1, nil},
		{"a log with no FSPERF line", fsperfGate, f.log("silent.log", "nothing to see\n"), 1, nil},
		{"an absent capacity volume on a non-required run", fsperfCapacityGate, good, 0, nil},
		{"--require-capacity without an FSCAP line", fsperfCapacityGate, good, 1, []string{"--require-capacity"}},
		{"a capacity line inside its caps", fsperfCapacityGate, f.log("cap.log", fsperfGood+fscapLine), 0, []string{"--require-capacity"}},
		{"a mount reading 4 MiB per GiB", fsperfCapacityGate, f.log("cap-slow.log", fsperfGood+strings.Replace(fscapLine, "mountreads=512", "mountreads=65536", 1)), 1, []string{"--require-capacity"}},
		{"a volume holding 2 files", fsperfCapacityGate, f.log("cap-bare.log", fsperfGood+strings.Replace(fscapLine, "files=5447", "files=2", 1)), 1, []string{"--require-capacity"}},
		{"a tree of 4 KiB", fsperfCapacityGate, f.log("cap-thin.log", fsperfGood+strings.Replace(fscapLine, "treebytes=1105596389", "treebytes=4096", 1)), 1, []string{"--require-capacity"}},
		{"a gate file grading nothing", "max-mount-reads-per-gib\tcapacity\t64\n", good, 1, nil},
	}
	for _, c := range cases {
		f.gateFile("fsperf", "tests", c.gate)
		expect(t, c.label, f.run("fsperf", []string{c.log}, c.extra...), c.code)
	}

	f.gateFile("fsperf", "tests", fsperfGate)
	expect(t, "the finding names the row", f.run("fsperf", []string{variant("chunky2.log", "txns=16", "txns=1024")}), 1, "tests max-txns-per-mib: 256 > 4")
	f.gateFile("fsperf", "tests", fsperfCapacityGate)
	expect(t, "a skipped capacity row says so", f.run("fsperf", []string{good}), 0, "no FSCAP[capacity] in this run")

	capLog := f.log("cap-rt.log", fsperfGood+fscapLine)
	out := f.emitInto("fsperf", "tests", []string{capLog})
	for _, want := range []string{"max-txns-per-mib\ttests\t6\n", "min-fs-over-raw-pct\ttests\t37\n", "min-volume-gib\tcapacity\t16\n", "max-mount-reads-per-gib\tcapacity\t41\n", "min-files\tcapacity\t4085\n"} {
		if !strings.Contains(out, want) {
			t.Errorf("emit missing %q:\n%s", want, out)
		}
	}
	expect(t, "emitted allowlist accepts its own measurement", f.run("fsperf", []string{capLog}, "--require-capacity"), 0)

	f.removeGateFile("fsperf", "tests")
	expect(t, "no gate data", f.run("fsperf", []string{good}), 2, "no gate data")
}

func TestFsperfShards(t *testing.T) {
	f := newFixture(t)
	f.gateFile("fsperf", "tests", fsperfCapacityGate)
	measured := f.log("measured.log", fsperfGood)
	other := f.log("other.log", "SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe\n")
	third := f.log("third.log", "boot\n")
	capShard := f.log("cap.log", "noise\n"+fscapLine)

	expect(t, "FSPERF from one shard of two", f.run("fsperf", []string{other, measured}), 0, "over 2 log(s)")
	expect(t, "FSPERF from one shard of three, FSCAP from another", f.run("fsperf", []string{other, measured, capShard}, "--require-capacity"), 0, "over 3 log(s)")
	expect(t, "no shard carries FSPERF", f.run("fsperf", []string{other, third}), 1, "no FSPERF lines in")
	expect(t, "a floor fails when no shard carries its phase", f.run("fsperf", []string{other, measured, third}, "--require-capacity"), 1, "which reported no FSCAP line")
	twice := f.log("twice.log", fsperfGood)
	expect(t, "a measurement in two shards", f.run("fsperf", []string{measured, twice}), 1, "FSPERF[tests] appears in both")
	capTwice := f.log("cap2.log", fscapLine)
	expect(t, "a capacity measurement in two shards", f.run("fsperf", []string{measured, capShard, capTwice}), 1, "FSCAP[capacity] appears in both")
}
