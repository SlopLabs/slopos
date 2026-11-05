package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
)

// JsonlSink emits one compact JSON object per line. The schema is documented
// in the public KTAP docs. Safe for concurrent use: a sharded run's boots all
// write to one sink.
type JsonlSink struct {
	path string
	mu   sync.Mutex
	f    *os.File
}

// NewJsonlSink creates the output file (parent dirs included) and returns
// an open sink. Caller must `Close` at end of run.
func NewJsonlSink(path string) (*JsonlSink, error) {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return nil, fmt.Errorf("jsonl: mkdir %q: %w", filepath.Dir(path), err)
	}
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o644)
	if err != nil {
		return nil, fmt.Errorf("jsonl: open %q: %w", path, err)
	}
	return &JsonlSink{path: path, f: f}, nil
}

// Write emits one event; events deliberately kept out of the stream
// (currently EvNonKtap) are skipped. `shard` is the 1-based shard of a
// sharded run that produced it, carried as a `shard` key; 0 omits the key.
func (s *JsonlSink) Write(ev Event, shard int) error {
	obj := encodeEvent(ev)
	if obj == nil {
		return nil
	}
	if shard > 0 {
		obj["shard"] = shard
	}
	return s.writeObj(obj)
}

// WriteRunEnd emits the `run_end` summary, the last event in any run; a
// sharded run's is the combined summary.
func (s *JsonlSink) WriteRunEnd(summary *RunSummary, exitCode int) error {
	return s.writeObj(runEndObject("run_end", summary, exitCode))
}

// WriteShardEnd emits one shard's own summary, ahead of the combined
// `run_end`.
func (s *JsonlSink) WriteShardEnd(shard int, summary *RunSummary, exitCode int) error {
	obj := runEndObject("shard_end", summary, exitCode)
	obj["shard"] = shard
	return s.writeObj(obj)
}

func (s *JsonlSink) writeObj(obj map[string]any) error {
	b, err := json.Marshal(obj)
	if err != nil {
		return fmt.Errorf("jsonl: marshal %v: %w", obj["t"], err)
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, err := s.f.Write(append(b, '\n')); err != nil {
		return fmt.Errorf("jsonl: write %v: %w", obj["t"], err)
	}
	return nil
}

func runEndObject(kind string, summary *RunSummary, exitCode int) map[string]any {
	phases := make([]map[string]any, 0, len(summary.Phases))
	for _, p := range summary.Phases {
		var planN any
		if p.PlanN != nil {
			planN = *p.PlanN
		}
		var elapsed any
		if p.ElapsedMs != nil {
			elapsed = *p.ElapsedMs
		}
		var bail any
		if p.BailReason != nil {
			bail = *p.BailReason
		}
		phases = append(phases, map[string]any{
			"idx":        p.Idx,
			"name":       p.Name,
			"plan_n":     planN,
			"elapsed_ms": elapsed,
			"pass":       p.Counters.Pass,
			"fail":       p.Counters.Fail,
			"skip":       p.Counters.Skip,
			"over_time":  p.Counters.OverTime,
			"bail":       bail,
		})
	}
	var qemuStatus any
	if summary.QemuStatus != nil {
		qemuStatus = *summary.QemuStatus
	}
	return map[string]any{
		"t":                   kind,
		"wall_ms":             summary.WallMs(),
		"exit":                exitCode,
		"qemu_status":         qemuStatus,
		"user_aborted":        summary.UserAborted,
		"timed_out":           summary.TimedOut,
		"silence_hit":         summary.SilenceHit,
		"truncated":           summary.Truncated,
		"kernel_abort":        summary.KernelAbort,
		"kernel_abort_reason": summary.KernelAbortReason,
		"phases":              phases,
	}
}

// Close closes the file after a best-effort fsync, so a crashed wrapper
// doesn't lose the last few events.
func (s *JsonlSink) Close() error {
	if s.f == nil {
		return nil
	}
	_ = s.f.Sync()
	err := s.f.Close()
	s.f = nil
	return err
}

// encodeEvent turns a parser Event into a marshallable map, or nil for events
// deliberately suppressed from the JSONL stream.
func encodeEvent(ev Event) map[string]any {
	switch e := ev.(type) {
	case *EvPhaseStart:
		return map[string]any{
			"t":    "phase_start",
			"idx":  e.PhaseIdx,
			"name": e.Name,
		}
	case *EvPlan:
		return map[string]any{
			"t":         "plan",
			"phase_idx": e.PhaseIdx,
			"n":         e.N,
		}
	case *EvTest:
		r := e.Record
		subs := make([]map[string]any, 0, len(r.Subtests))
		for _, s := range r.Subtests {
			subs = append(subs, map[string]any{
				"idx":     s.Idx,
				"name":    s.Name,
				"outcome": string(s.Outcome),
				"msg":     s.Msg,
			})
		}
		// The schema keeps every key: an absent field renders as JSON
		// `null` rather than being omitted.
		out := map[string]any{
			"t":                  "test",
			"phase":              r.PhaseName,
			"phase_idx":          r.PhaseIdx,
			"idx":                r.Idx,
			"name":               r.Name,
			"outcome":            string(r.Outcome),
			"over_time":          r.OverTime,
			"expected_panic":     r.ExpectedPanic,
			"subtests":           subs,
			"pre_fail_klog_tail": r.PreFailKlogTail,
		}
		if r.TimeMs != nil {
			out["time_ms"] = *r.TimeMs
		} else {
			out["time_ms"] = nil
		}
		out["skip_reason"] = ptrOrNil(r.SkipReason)
		out["fail_file"] = ptrOrNil(r.FailFile)
		out["fail_outcome_kind"] = ptrOrNil(r.FailOutcomeKind)
		out["log"] = ptrOrNil(r.Log)
		return out
	case *EvPhaseEnd:
		return map[string]any{
			"t":          "phase_end",
			"idx":        e.PhaseIdx,
			"name":       e.Name,
			"elapsed_ms": e.ElapsedMs,
			"pass":       e.Pass,
			"fail":       e.Fail,
			"skip":       e.Skip,
			"over_time":  e.OverTime,
		}
	case *EvBail:
		return map[string]any{
			"t":         "bail",
			"phase_idx": e.PhaseIdx,
			"reason":    e.Reason,
		}
	case *EvFixture:
		return map[string]any{
			"t":         "fixture",
			"phase_idx": e.PhaseIdx,
			"name":      e.Name,
		}
	case *EvShard:
		return map[string]any{
			"t":         "shard",
			"phase_idx": e.PhaseIdx,
			"index":     e.Index,
			"count":     e.Count,
		}
	case *EvKernelAbort:
		return map[string]any{
			"t":      "kernel_abort",
			"reason": e.Reason,
		}
	}
	return nil
}

func ptrOrNil(p *string) any {
	if p == nil {
		return nil
	}
	return *p
}
