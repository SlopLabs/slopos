package gates

// The fsperf gate holds the filesystem's write and capacity costs to
// scripts/gates/fsperf/<variant>.txt.
//
// Per-MiB counts — transactions a write opens, block-layer requests they hand
// down, sectors moved, barriers forced — are properties of the code, not of
// the machine, so they carry caps. Throughput is not: an absolute MiB/s floor
// fails on the unmodified tree on any machine without /dev/kvm. The
// accelerator-invariant quantity is the filesystem's write rate over the same
// run's raw device write rate, so that carries a floor. Mount cost is graded
// in device reads per GiB of volume, which are countable, not in seconds.
// Every floor (min-bytes, min-volume-gib, min-dirents, min-files,
// min-treebytes) exists because a measurement that stopped happening looks
// exactly like one that got free.
//
// Several logs are several whole runs: each row is graded against the worst
// value any log reported for its phase, and still fails when no log carries
// its phase.

import (
	"fmt"
	"regexp"
	"slices"
	"strings"
)

func init() {
	register(Gate{
		Name:    "fsperf",
		Summary: "filesystem per-MiB costs under caps, fs/raw write-rate ratio and volume floors met",
		Run:     runFsperf,
	})
}

const mib = 1 << 20

var (
	fsperfMarker = regexp.MustCompile(`FS(PERF|CAP)\[`)
	fsperfRE     = regexp.MustCompile(`^FSPERF\[([a-z0-9-]+)\]: bytes=(\d+) txns=(\d+) commits=(\d+) devwrites=(\d+) devblocks=(\d+) barriers=(\d+) ns=(\d+) rawbytes=(\d+) rawns=(\d+)`)
	fscapRE      = regexp.MustCompile(`^FSCAP\[([a-z0-9-]+)\]: blocks=(\d+) blocksize=(\d+) groups=(\d+) cacheentries=(\d+) mountreads=(\d+) mountns=(\d+) dirents=(\d+) lookupreads=(\d+) bytes=(\d+) ns=(\d+) files=(\d+) treebytes=(\d+)`)
)

type fsWrite struct {
	log                                                                   string
	bytes, txns, commits, devwrites, devblocks, barriers, ns, rawb, rawns int64
}

type fsCapacity struct {
	log                                                                                    string
	blocks, bsize, groups, entries, mreads, mns, dirents, lookupr, bytes, ns, files, treeb int64
}

// gib is the volume size in whole GiB, never below 1 so a per-GiB cost of a
// tiny volume is its whole cost.
func (c fsCapacity) gib() int64 {
	if c.bsize <= 0 || c.bsize > 1<<30 {
		return 1
	}
	return max(c.blocks/((1<<30)/c.bsize), 1)
}

// perMiB rounds up, so a count never reads cheaper than it was.
func perMiB(count, bytes int64) int64 {
	if bytes <= 0 {
		return 0
	}
	return (count*mib + bytes - 1) / bytes
}

// fsOverRawPct is the rate quotient in percent, as a cross-product so neither
// side needs a division that would round a slow run into a fast one.
func (w fsWrite) fsOverRawPct() int64 {
	if w.ns == 0 || w.rawb == 0 {
		return 0
	}
	return w.bytes * w.rawns * 100 / (w.ns * w.rawb)
}

// A quarter above the observation for a ceiling, a quarter below for a floor:
// the counts are deterministic for one ISO, but a legitimate change in test
// payload moves them, and a cap with no slack makes every such change a gate edit.
func padUp(v int64) int64   { return (v*5+3)/4 + 1 }
func padDown(v int64) int64 { return v * 3 / 4 }

// fsLog is one run's report lines, keyed by phase.
type fsLog struct {
	path     string
	write    map[string]fsWrite
	capacity map[string]fsCapacity
}

// fsRun is every log, each one whole run, with the phases in the order the
// logs first reported them.
type fsRun struct {
	logs                 []fsLog
	writeOrder, capOrder []string
}

var (
	fsWriteKeys = []string{"max-txns-per-mib", "max-commits-per-mib", "max-devwrites-per-mib", "max-devblocks-per-mib", "max-barriers-per-mib", "min-fs-over-raw-pct", "min-bytes"}
	fsCapKeys   = []string{"max-mount-reads-per-gib", "max-lookup-reads", "min-volume-gib", "min-dirents", "min-files", "min-treebytes"}
)

func (w fsWrite) value(key string) int64 {
	switch key {
	case "max-txns-per-mib":
		return perMiB(w.txns, w.bytes)
	case "max-commits-per-mib":
		return perMiB(w.commits, w.bytes)
	case "max-devwrites-per-mib":
		return perMiB(w.devwrites, w.bytes)
	case "max-devblocks-per-mib":
		return perMiB(w.devblocks, w.bytes)
	case "max-barriers-per-mib":
		return perMiB(w.barriers, w.bytes)
	case "min-fs-over-raw-pct":
		return w.fsOverRawPct()
	}
	return w.bytes
}

func (c fsCapacity) value(key string) int64 {
	switch key {
	case "max-mount-reads-per-gib":
		return c.mreads / c.gib()
	case "max-lookup-reads":
		return c.lookupr
	case "min-volume-gib":
		return c.gib()
	case "min-dirents":
		return c.dirents
	case "min-files":
		return c.files
	}
	return c.treeb
}

// worse keeps whichever of two observations a cap or floor must admit.
func worse(key string, a, b int64) int64 {
	if strings.HasPrefix(key, "max-") {
		return max(a, b)
	}
	return min(a, b)
}

func parseFsperf(caps []capture) (*fsRun, error) {
	r := &fsRun{}
	seenWrite, seenCap := map[string]bool{}, map[string]bool{}
	for _, c := range caps {
		l := fsLog{path: c.path, write: map[string]fsWrite{}, capacity: map[string]fsCapacity{}}
		for _, line := range c.lines {
			text, ok := reportAt(line, fsperfMarker)
			if !ok {
				continue
			}
			if m := fsperfRE.FindStringSubmatch(text); m != nil {
				if !seenWrite[m[1]] {
					seenWrite[m[1]] = true
					r.writeOrder = append(r.writeOrder, m[1])
				}
				l.write[m[1]] = fsWrite{c.path, num(m[2]), num(m[3]), num(m[4]), num(m[5]), num(m[6]), num(m[7]), num(m[8]), num(m[9]), num(m[10])}
			} else if m := fscapRE.FindStringSubmatch(text); m != nil {
				if !seenCap[m[1]] {
					seenCap[m[1]] = true
					r.capOrder = append(r.capOrder, m[1])
				}
				l.capacity[m[1]] = fsCapacity{c.path, num(m[2]), num(m[3]), num(m[4]), num(m[5]), num(m[6]), num(m[7]), num(m[8]), num(m[9]), num(m[10]), num(m[11]), num(m[12]), num(m[13])}
			}
		}
		r.logs = append(r.logs, l)
	}
	if len(r.writeOrder) == 0 {
		var fail findings
		fail.add("no FSPERF lines in %s — the kernel emits them from the fs cost report; a run with none never reached the measurement, it is not a cost-free one.", strings.Join(logPaths(caps), ", "))
		return nil, fail.err()
	}
	return r, nil
}

// observed is the worst value of key over every log that reported phase.
func (r *fsRun) observed(key, phase string) (int64, bool) {
	var got int64
	found := false
	for _, l := range r.logs {
		var v int64
		if slices.Contains(fsWriteKeys, key) {
			w, ok := l.write[phase]
			if !ok {
				continue
			}
			v = w.value(key)
		} else {
			c, ok := l.capacity[phase]
			if !ok {
				continue
			}
			v = c.value(key)
		}
		if found {
			got = worse(key, got, v)
		} else {
			got, found = v, true
		}
	}
	return got, found
}

func logPaths(caps []capture) []string {
	out := make([]string, len(caps))
	for i, c := range caps {
		out[i] = c.path
	}
	return out
}

func runFsperf(cfg Config) error {
	caps, err := loadCaptures(cfg.Logs)
	if err != nil {
		return err
	}
	run, err := parseFsperf(caps)
	if err != nil {
		return err
	}
	if cfg.Emit {
		emitFsperf(cfg, run)
		return nil
	}
	rel, lines, err := readGateFile(cfg, "fsperf")
	if err != nil {
		return err
	}
	type row struct {
		key, phase string
		want, got  int64
	}
	var rows []row
	var fail findings
	matched := 0
	skipped := map[string]bool{}
	for _, l := range lines {
		f := strings.Fields(l.text)
		// A row with a field missing or spare grades nothing while the file
		// says it does.
		if len(f) != 3 {
			return gateFileErrorf("%s:%d: want '<key> <phase> <value>', got %d fields: %s", rel, l.no, len(f), l.text)
		}
		key, phase := f[0], f[1]
		isWrite := slices.Contains(fsWriteKeys, key)
		if !isWrite && !slices.Contains(fsCapKeys, key) {
			return gateFileErrorf("%s:%d: unknown gate key '%s'", rel, l.no, key)
		}
		got, ok := run.observed(key, phase)
		if !ok {
			switch {
			case isWrite:
				fail.add("%s names phase '%s', which reported no FSPERF line (dead entry)", key, phase)
			case cfg.RequireCapacity:
				fail.add("%s names phase '%s', which reported no FSCAP line", key, phase)
			case !skipped[phase]:
				// A run without the capacity volume attached legitimately
				// reports no FSCAP line; --require-capacity is the run that has it.
				skipped[phase] = true
				fmt.Fprintf(cfg.Out, "fsperf: no FSCAP[%s] in this run — capacity volume not attached, skipping its rows\n", phase)
			}
			continue
		}
		want, err := parseCount(rel, l, f[2])
		if err != nil {
			return err
		}
		matched++
		if strings.HasPrefix(key, "max-") && got > want {
			fail.add("%s %s: %d > %d", phase, key, got, want)
		}
		if strings.HasPrefix(key, "min-") && got < want {
			fail.add("%s %s: %d < %d", phase, key, got, want)
		}
		rows = append(rows, row{key, phase, want, got})
	}
	if matched == 0 {
		fail.add("gate file %s graded nothing", rel)
	}
	if len(fail) > 0 {
		fail.add("A cost above its cap is a measurement to re-take, not a number to raise. Re-measure with --emit-allowlist in the same commit and say what started costing more.")
		return fail.err()
	}
	for _, r := range rows {
		fmt.Fprintf(cfg.Out, "fsperf:   %-24s %-18s %10d (cap %d)\n", r.key, r.phase, r.got, r.want)
	}
	fmt.Fprintf(cfg.Out, "fsperf: OK — variant=%s, %d row(s) graded over %d log(s)\n", cfg.Variant, matched, len(caps))
	return nil
}

// fsPad is the emitted bound for an observation: padded away from it, except
// the volume size, which is a property of the attached image.
func fsPad(key string, v int64) int64 {
	switch {
	case key == "min-volume-gib":
		return v
	case strings.HasPrefix(key, "max-"):
		return padUp(v)
	}
	return padDown(v)
}

func emitFsperf(cfg Config, run *fsRun) {
	out := cfg.Out
	fmt.Fprintf(out, `# fsperf gate data — variant: %s
#
#     %s
#
# Counts per MiB are deterministic for one ISO and carry caps. The
# filesystem-over-raw write-rate ratio is the only quantity here that is
# invariant under a change of accelerator, so it is the only rate graded.
# Floors exist so a measurement that stopped happening cannot read as free.
# Over several runs each cap takes the highest and each floor the lowest
# value any run reported.
#
# A cap matching nothing is a dead entry and fails the gate.
`, cfg.Variant, emitCommand("fsperf", cfg.Variant))
	for _, phase := range run.writeOrder {
		for _, l := range run.logs {
			w, ok := l.write[phase]
			if !ok {
				continue
			}
			fmt.Fprintf(out, "\n# %s (%s): %d bytes written, %d transactions,\n", phase, l.path, w.bytes, w.txns)
			fmt.Fprintf(out, "# %d device write requests carrying %d blocks,\n", w.devwrites, w.devblocks)
			fmt.Fprintf(out, "# %d barriers; fs %dKiB in %dns against\n", w.barriers, w.bytes/1024, w.ns)
			fmt.Fprintf(out, "# raw %dKiB in %dns.\n", w.rawb/1024, w.rawns)
			break
		}
		for _, key := range fsWriteKeys {
			v, _ := run.observed(key, phase)
			fmt.Fprintf(out, "%s\t%s\t%d\n", key, phase, fsPad(key, v))
		}
	}
	for _, phase := range run.capOrder {
		for _, l := range run.logs {
			c, ok := l.capacity[phase]
			if !ok {
				continue
			}
			fmt.Fprintf(out, "\n# %s (%s): %d blocks of %d (%d GiB) in\n", phase, l.path, c.blocks, c.bsize, c.gib())
			fmt.Fprintf(out, "# %d groups, mounted with %d cache entries in\n", c.groups, c.entries)
			fmt.Fprintf(out, "# %d device reads / %dns; %d names in one\n", c.mreads, c.mns, c.dirents)
			fmt.Fprintf(out, "# directory, a lookup of the last costing %d block reads;\n", c.lookupr)
			fmt.Fprintf(out, "# %d regular files totalling %d bytes walked back off it.\n", c.files, c.treeb)
			break
		}
		for _, key := range fsCapKeys {
			v, _ := run.observed(key, phase)
			fmt.Fprintf(out, "%s\t%s\t%d\n", key, phase, fsPad(key, v))
		}
	}
}
