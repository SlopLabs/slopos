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
// Aggregation: the lines come from the tests that measure them, which run in
// one shard, so each FSPERF/FSCAP phase must come from exactly one log, and a
// row still fails when no log carries its phase.

import (
	"fmt"
	"regexp"
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

type fsRun struct {
	writeOrder, capOrder []string
	write                map[string]fsWrite
	capacity             map[string]fsCapacity
}

func parseFsperf(caps []capture) (*fsRun, error) {
	r := &fsRun{write: map[string]fsWrite{}, capacity: map[string]fsCapacity{}}
	var fail findings
	for _, c := range caps {
		write := map[string]fsWrite{}
		capacity := map[string]fsCapacity{}
		var writeOrder, capOrder []string
		for _, line := range c.lines {
			text, ok := reportAt(line, fsperfMarker)
			if !ok {
				continue
			}
			if m := fsperfRE.FindStringSubmatch(text); m != nil {
				if _, dup := write[m[1]]; !dup {
					writeOrder = append(writeOrder, m[1])
				}
				write[m[1]] = fsWrite{c.path, num(m[2]), num(m[3]), num(m[4]), num(m[5]), num(m[6]), num(m[7]), num(m[8]), num(m[9]), num(m[10])}
			} else if m := fscapRE.FindStringSubmatch(text); m != nil {
				if _, dup := capacity[m[1]]; !dup {
					capOrder = append(capOrder, m[1])
				}
				capacity[m[1]] = fsCapacity{c.path, num(m[2]), num(m[3]), num(m[4]), num(m[5]), num(m[6]), num(m[7]), num(m[8]), num(m[9]), num(m[10]), num(m[11]), num(m[12]), num(m[13])}
			}
		}
		for _, phase := range writeOrder {
			if prev, dup := r.write[phase]; dup {
				fail.add("FSPERF[%s] appears in both %s and %s — a measurement comes from exactly one boot; one test ran twice.", phase, prev.log, c.path)
				continue
			}
			r.write[phase] = write[phase]
			r.writeOrder = append(r.writeOrder, phase)
		}
		for _, phase := range capOrder {
			if prev, dup := r.capacity[phase]; dup {
				fail.add("FSCAP[%s] appears in both %s and %s — a measurement comes from exactly one boot; one test ran twice.", phase, prev.log, c.path)
				continue
			}
			r.capacity[phase] = capacity[phase]
			r.capOrder = append(r.capOrder, phase)
		}
	}
	if len(r.write) == 0 {
		fail.add("no FSPERF lines in %s — the kernel emits them from the fs cost report; a run with none never reached the measurement, it is not a cost-free one.", strings.Join(logPaths(caps), ", "))
	}
	return r, fail.err()
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
		var got int64
		switch key {
		case "max-txns-per-mib", "max-commits-per-mib", "max-devwrites-per-mib", "max-devblocks-per-mib", "max-barriers-per-mib", "min-fs-over-raw-pct", "min-bytes":
			w, ok := run.write[phase]
			if !ok {
				fail.add("%s names phase '%s', which reported no FSPERF line (dead entry)", key, phase)
				continue
			}
			switch key {
			case "max-txns-per-mib":
				got = perMiB(w.txns, w.bytes)
			case "max-commits-per-mib":
				got = perMiB(w.commits, w.bytes)
			case "max-devwrites-per-mib":
				got = perMiB(w.devwrites, w.bytes)
			case "max-devblocks-per-mib":
				got = perMiB(w.devblocks, w.bytes)
			case "max-barriers-per-mib":
				got = perMiB(w.barriers, w.bytes)
			case "min-fs-over-raw-pct":
				got = w.fsOverRawPct()
			case "min-bytes":
				got = w.bytes
			}
		case "max-mount-reads-per-gib", "max-lookup-reads", "min-volume-gib", "min-dirents", "min-files", "min-treebytes":
			c, ok := run.capacity[phase]
			if !ok {
				// A run without the capacity volume attached legitimately
				// reports no FSCAP line; --require-capacity is the run that has it.
				if cfg.RequireCapacity {
					fail.add("%s names phase '%s', which reported no FSCAP line", key, phase)
				} else if !skipped[phase] {
					skipped[phase] = true
					fmt.Fprintf(cfg.Out, "fsperf: no FSCAP[%s] in this run — capacity volume not attached, skipping its rows\n", phase)
				}
				continue
			}
			switch key {
			case "max-mount-reads-per-gib":
				got = c.mreads / c.gib()
			case "max-lookup-reads":
				got = c.lookupr
			case "min-volume-gib":
				got = c.gib()
			case "min-dirents":
				got = c.dirents
			case "min-files":
				got = c.files
			case "min-treebytes":
				got = c.treeb
			}
		default:
			return gateFileErrorf("%s:%d: unknown gate key '%s'", rel, l.no, key)
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
#
# A cap matching nothing is a dead entry and fails the gate.
`, cfg.Variant, emitCommand("fsperf", cfg.Variant))
	for _, phase := range run.writeOrder {
		w := run.write[phase]
		fmt.Fprintf(out, "\n# %s: %d bytes written, %d transactions,\n", phase, w.bytes, w.txns)
		fmt.Fprintf(out, "# %d device write requests carrying %d blocks,\n", w.devwrites, w.devblocks)
		fmt.Fprintf(out, "# %d barriers; fs %dKiB in %dns against\n", w.barriers, w.bytes/1024, w.ns)
		fmt.Fprintf(out, "# raw %dKiB in %dns.\n", w.rawb/1024, w.rawns)
		fmt.Fprintf(out, "max-txns-per-mib\t%s\t%d\n", phase, padUp(perMiB(w.txns, w.bytes)))
		fmt.Fprintf(out, "max-commits-per-mib\t%s\t%d\n", phase, padUp(perMiB(w.commits, w.bytes)))
		fmt.Fprintf(out, "max-devwrites-per-mib\t%s\t%d\n", phase, padUp(perMiB(w.devwrites, w.bytes)))
		fmt.Fprintf(out, "max-devblocks-per-mib\t%s\t%d\n", phase, padUp(perMiB(w.devblocks, w.bytes)))
		fmt.Fprintf(out, "max-barriers-per-mib\t%s\t%d\n", phase, padUp(perMiB(w.barriers, w.bytes)))
		fmt.Fprintf(out, "min-fs-over-raw-pct\t%s\t%d\n", phase, padDown(w.fsOverRawPct()))
		fmt.Fprintf(out, "min-bytes\t%s\t%d\n", phase, padDown(w.bytes))
	}
	for _, phase := range run.capOrder {
		c := run.capacity[phase]
		fmt.Fprintf(out, "\n# %s: %d blocks of %d (%d GiB) in\n", phase, c.blocks, c.bsize, c.gib())
		fmt.Fprintf(out, "# %d groups, mounted with %d cache entries in\n", c.groups, c.entries)
		fmt.Fprintf(out, "# %d device reads / %dns; %d names in one\n", c.mreads, c.mns, c.dirents)
		fmt.Fprintf(out, "# directory, a lookup of the last costing %d block reads;\n", c.lookupr)
		fmt.Fprintf(out, "# %d regular files totalling %d bytes walked back off it.\n", c.files, c.treeb)
		fmt.Fprintf(out, "max-mount-reads-per-gib\t%s\t%d\n", phase, padUp(c.mreads/c.gib()))
		fmt.Fprintf(out, "max-lookup-reads\t%s\t%d\n", phase, padUp(c.lookupr))
		fmt.Fprintf(out, "min-volume-gib\t%s\t%d\n", phase, c.gib())
		fmt.Fprintf(out, "min-dirents\t%s\t%d\n", phase, padDown(c.dirents))
		fmt.Fprintf(out, "min-files\t%s\t%d\n", phase, padDown(c.files))
		fmt.Fprintf(out, "min-treebytes\t%s\t%d\n", phase, padDown(c.treeb))
	}
}
