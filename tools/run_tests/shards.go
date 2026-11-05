package main

// A sharded run: `--shards N` boots the suite N times at once, each boot
// running the slice `tests.shard=K/N` selects, and renders the boots as one
// run. Every boot has its own ISO, its own sparse copy of the tests fs image,
// its own scratch disks and its own raw log, so nothing one boot writes can
// reach another.

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"sort"
	"strconv"
	"sync"
	"syscall"
	"time"
)

type shardedRun struct {
	args         *Args
	root         string
	cmdline      string
	fsImage      string
	captureDir   string
	shardsDir    string
	lastFailList string
	hasSelection bool
}

// shardBoot is one boot of a sharded run and what it left behind.
type shardBoot struct {
	index, count int
	// dir holds the boot's ISO, fs image copy and scratch disks.
	dir     string
	iso     string
	fsImage string
	logPath string

	recorder *RunRecorder
	driver   DriverResult
	verdict  RunVerdict
	err      error
}

func (b *shardBoot) label() string {
	return fmt.Sprintf("%d/%d", b.index, b.count)
}

// shardIndices lists the 1-based shards a run boots: `only` alone when set.
func shardIndices(count, only int) []int {
	if only > 0 {
		return []int{only}
	}
	out := make([]int, count)
	for i := range out {
		out[i] = i + 1
	}
	return out
}

func runSharded(r *shardedRun) int {
	a := r.args
	var boots []*shardBoot
	for _, k := range shardIndices(a.Shards, a.OnlyShard) {
		dir := filepath.Join(r.shardsDir, strconv.Itoa(k))
		boots = append(boots, &shardBoot{
			index:   k,
			count:   a.Shards,
			dir:     dir,
			iso:     filepath.Join(dir, "slop-tests.iso"),
			fsImage: filepath.Join(dir, filepath.Base(r.fsImage)),
			logPath: filepath.Join(r.captureDir, fmt.Sprintf("shard-%d.log", k)),
		})
	}

	if a.DryRun {
		fmt.Printf("TEST_CMDLINE=%q\n", r.cmdline)
		for _, b := range boots {
			fmt.Printf("would run: SCRATCH_DIR=%s scripts/qemu_run.sh test %s %s > %s  (tests.shard=%s)\n",
				b.dir, b.iso, b.fsImage, b.logPath, b.label())
		}
		return 0
	}

	if !a.NoBuild {
		if err := runJustIsoTestsShards(r.root, r.cmdline, a.Shards, a.OnlyShard); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
	}
	if _, err := os.Stat(r.fsImage); err != nil {
		fmt.Fprintf(os.Stderr, "run_tests: fs image not found at %s\n", r.fsImage)
		return 64
	}
	if err := os.MkdirAll(r.captureDir, 0o755); err != nil {
		fmt.Fprintln(os.Stderr, "run_tests:", err)
		return 64
	}
	for _, b := range boots {
		if _, err := os.Stat(b.iso); err != nil {
			fmt.Fprintf(os.Stderr, "run_tests: shard %s ISO not found at %s\n", b.label(), b.iso)
			return 64
		}
		if err := sparseCopy(r.fsImage, b.fsImage); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
	}

	var sink *JsonlSink
	if a.JsonPath != "" {
		s, err := NewJsonlSink(a.JsonPath)
		if err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
			return 64
		}
		sink = s
		defer sink.Close()
	}

	stdoutFd := os.Stdout.Fd()
	colour := UseColour(a.ColorMode, a.NoColor, stdoutFd)
	cols := TerminalCols(stdoutFd)
	verbosity := "summary"
	switch {
	case a.Verbose:
		verbosity = "verbose"
	case a.Quiet:
		verbosity = "quiet"
	}
	out := &lockedWriter{w: os.Stdout}
	for _, b := range boots {
		fmt.Fprintf(out, "shard %s: log %s\n", b.label(), b.logPath)
	}

	ctx, cancelSignal := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer cancelSignal()

	jobs := a.Jobs
	if jobs == 0 || jobs > len(boots) {
		jobs = len(boots)
	}
	slots := make(chan struct{}, jobs)
	var wg sync.WaitGroup
	for _, b := range boots {
		wg.Add(1)
		go func() {
			defer wg.Done()
			slots <- struct{}{}
			defer func() { <-slots }()
			var renderer rendererIface
			if !a.Raw {
				// No live bar: N boots cannot share one line of a terminal.
				br := NewBarRenderer(out, verbosity, colour, a.WarnMs, false, cols)
				br.Label = "[" + b.label() + "] "
				renderer = br
			}
			b.run(ctx, r, renderer, sink)
		}()
	}
	wg.Wait()

	var parts []*RunSummary
	var partShards []int
	var codes []int
	userAborted := false
	for _, b := range boots {
		if b.err != nil {
			fmt.Fprintf(os.Stderr, "run_tests: shard %s: %v\n", b.label(), b.err)
			codes = append(codes, 64)
			continue
		}
		parts = append(parts, b.recorder.Summary)
		partShards = append(partShards, b.index)
		codes = append(codes, b.verdict.Code)
		userAborted = userAborted || b.driver.UserAborted
		for _, note := range []string{b.verdict.QemuStatusWarning, b.verdict.Diagnostic} {
			if note != "" {
				fmt.Fprintf(os.Stderr, "shard %s: %s\n", b.label(), note)
			}
		}
	}
	merged := MergeShardSummaries(parts, partShards)
	exitCode := CombineShardExit(codes)

	if !a.Raw {
		NewBarRenderer(os.Stdout, verbosity, colour, a.WarnMs, false, cols).Finalize(merged)
	}
	if !userAborted {
		updateLastFailList(r.root, r.lastFailList, merged.Failures(), !a.Raw)
	}
	if !a.Raw {
		fmt.Printf("exit: %d\n", exitCode)
	}

	if sink != nil {
		for _, b := range boots {
			if b.err != nil {
				continue
			}
			if err := sink.WriteShardEnd(b.index, b.recorder.Summary, b.verdict.Code); err != nil {
				fmt.Fprintln(os.Stderr, "run_tests:", err)
			}
		}
		if err := sink.WriteRunEnd(merged, exitCode); err != nil {
			fmt.Fprintln(os.Stderr, "run_tests:", err)
		}
	}
	return exitCode
}

// run boots one shard, writing every line it prints to its log and feeding
// its events to `renderer` (nil under --raw) and `sink`.
func (b *shardBoot) run(ctx context.Context, r *shardedRun, renderer rendererIface, sink *JsonlSink) {
	log, err := os.Create(b.logPath)
	if err != nil {
		b.err = err
		return
	}
	defer log.Close()

	parser := NewKtapParser()
	b.recorder = NewRecorder()
	driver := &QemuDriver{
		RepoRoot:       r.root,
		Iso:            b.iso,
		FsImage:        b.fsImage,
		WallTimeoutSec: float64(r.args.TimeoutSecs),
		SilenceSec:     float64(r.args.SilenceSecs),
		Env:            []string{"SCRATCH_DIR=" + b.dir},
	}
	handle := func(ev Event) {
		if t, ok := ev.(*EvTest); ok {
			t.Record.Shard = b.index
		}
		b.recorder.Record(ev)
		if renderer != nil {
			renderer.OnEvent(ev, b.recorder.Summary)
		}
		if _, ok := ev.(*EvKernelAbort); ok {
			driver.TightenSilence(PostAbortSilenceSec * time.Second)
		}
		if sink != nil {
			if err := sink.Write(ev, b.index); err != nil {
				fmt.Fprintln(os.Stderr, "run_tests: jsonl:", err)
			}
		}
	}
	var logErr error
	onLine := func(line string) {
		if _, err := io.WriteString(log, line+"\n"); err != nil && logErr == nil {
			logErr = err
		}
		for _, ev := range parser.Feed(line) {
			handle(ev)
		}
	}
	res, err := driver.Run(ctx, onLine)
	if err != nil {
		b.err = err
		return
	}
	if logErr != nil {
		b.err = fmt.Errorf("log %s: %w", b.logPath, logErr)
		return
	}
	for _, ev := range parser.Flush() {
		handle(ev)
	}
	s := b.recorder.Summary
	b.recorder.Finalize(res.QemuStatus)
	s.UserAborted = res.UserAborted
	s.TimedOut = res.TimedOut
	s.SilenceHit = res.SilenceHit
	if res.TimedOut || res.UserAborted || s.Truncated {
		s.AbortKlogTail = parser.KlogTail()
	}
	b.driver = res
	b.verdict = ClassifyRun(s, res, r.hasSelection)
}

// MergeShardSummaries folds the summaries of the shards numbered `shards`
// into one run: phases by ordinal with their plans and counters summed and
// tests concatenated in shard order, every abort flag or-ed, and each reason
// or klog line that reaches the combined summary tagged with its shard.
func MergeShardSummaries(parts []*RunSummary, shards []int) *RunSummary {
	m := &RunSummary{phaseByIdx: make(map[int]*PhaseRecord)}
	for i, s := range parts {
		shard := shards[i]
		tag := func(text string) string {
			return fmt.Sprintf("shard %d: %s", shard, text)
		}
		if m.StartedMonotonic.IsZero() || s.StartedMonotonic.Before(m.StartedMonotonic) {
			m.StartedMonotonic = s.StartedMonotonic
		}
		if s.FinishedMonotonic.After(m.FinishedMonotonic) {
			m.FinishedMonotonic = s.FinishedMonotonic
		}
		m.Truncated = m.Truncated || s.Truncated
		m.UserAborted = m.UserAborted || s.UserAborted
		m.TimedOut = m.TimedOut || s.TimedOut
		m.SilenceHit = m.SilenceHit || s.SilenceHit
		m.KernelAbort = m.KernelAbort || s.KernelAbort
		if m.KernelAbortReason == "" && s.KernelAbortReason != "" {
			m.KernelAbortReason = tag(s.KernelAbortReason)
		}
		for _, ln := range s.AbortKlogTail {
			m.AbortKlogTail = append(m.AbortKlogTail, tag(ln))
		}
		for _, p := range s.Phases {
			mp := m.phaseByIdx[p.Idx]
			if mp == nil {
				zero := 0
				mp = &PhaseRecord{Idx: p.Idx, Name: p.Name, PlanN: &zero}
				m.Phases = append(m.Phases, mp)
				m.phaseByIdx[p.Idx] = mp
			}
			plan := len(p.Tests)
			if p.PlanN != nil {
				plan = *p.PlanN
			}
			*mp.PlanN += plan
			if p.ElapsedMs != nil && (mp.ElapsedMs == nil || *p.ElapsedMs > *mp.ElapsedMs) {
				ms := *p.ElapsedMs
				mp.ElapsedMs = &ms
			}
			mp.Counters.Pass += p.Counters.Pass
			mp.Counters.Fail += p.Counters.Fail
			mp.Counters.Skip += p.Counters.Skip
			mp.Counters.OverTime += p.Counters.OverTime
			if mp.BailReason == nil && p.BailReason != nil {
				reason := tag(*p.BailReason)
				mp.BailReason = &reason
			}
			mp.Tests = append(mp.Tests, p.Tests...)
		}
	}
	sort.SliceStable(m.Phases, func(i, j int) bool { return m.Phases[i].Idx < m.Phases[j].Idx })
	return m
}

// CombineShardExit is the run's exit status from its shards': an interrupt
// wins, then any failed shard makes the run fail, then any shard that gave no
// trustworthy answer makes the run untrustworthy.
func CombineShardExit(codes []int) int {
	worst := 0
	for _, c := range codes {
		switch {
		case c == 130:
			return 130
		case c == 1:
			worst = 1
		case c != 0 && worst != 1 && c > worst:
			worst = c
		}
	}
	return worst
}

// runJustIsoTestsShards builds every shard's ISO, or `only`'s alone, from one
// kernel and base.
func runJustIsoTestsShards(root, cmdline string, shards, only int) error {
	argv := []string{"_iso-tests-shards", strconv.Itoa(shards)}
	if only > 0 {
		argv = append(argv, strconv.Itoa(only))
	}
	cmd := exec.Command("just", argv...)
	cmd.Dir = root
	cmd.Env = append(os.Environ(), "TEST_CMDLINE="+cmdline)
	cmd.Stdout = os.Stderr
	cmd.Stderr = os.Stderr
	fmt.Fprintf(os.Stderr, "==> TEST_CMDLINE=%q\n", cmdline)
	fmt.Fprintf(os.Stderr, "==> just %v\n", argv)
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("just _iso-tests-shards failed: %w", err)
	}
	return nil
}

// sparseCopy copies src to dst leaving every all-zero 64 KiB run a hole, so N
// copies of a mostly empty image cost what its data does.
func sparseCopy(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("copy %s: %w", src, err)
	}
	defer in.Close()
	st, err := in.Stat()
	if err != nil {
		return fmt.Errorf("copy %s: %w", src, err)
	}
	if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
		return fmt.Errorf("copy %s: %w", src, err)
	}
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
	if err != nil {
		return fmt.Errorf("copy %s: %w", dst, err)
	}
	buf := make([]byte, 64<<10)
	var off int64
	for {
		n, rerr := io.ReadFull(in, buf)
		if n > 0 && !allZero(buf[:n]) {
			if _, err := out.WriteAt(buf[:n], off); err != nil {
				out.Close()
				return fmt.Errorf("copy %s: %w", dst, err)
			}
		}
		off += int64(n)
		if errors.Is(rerr, io.EOF) || errors.Is(rerr, io.ErrUnexpectedEOF) {
			break
		}
		if rerr != nil {
			out.Close()
			return fmt.Errorf("copy %s: %w", src, rerr)
		}
	}
	if err := out.Truncate(st.Size()); err != nil {
		out.Close()
		return fmt.Errorf("copy %s: %w", dst, err)
	}
	return out.Close()
}

func allZero(b []byte) bool {
	for _, c := range b {
		if c != 0 {
			return false
		}
	}
	return true
}

// lockedWriter serialises whole writes from the shards' renderers, each of
// which writes one line per call.
type lockedWriter struct {
	mu sync.Mutex
	w  io.Writer
}

func (l *lockedWriter) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()
	return l.w.Write(p)
}
