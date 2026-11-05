package main

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

func summaryFromLines(t *testing.T, shard int, lines []string) *RunSummary {
	t.Helper()
	p := NewKtapParser()
	rec := NewRecorder()
	for _, ln := range lines {
		for _, ev := range p.Feed(ln) {
			if e, ok := ev.(*EvTest); ok {
				e.Record.Shard = shard
			}
			rec.Record(ev)
		}
	}
	for _, ev := range p.Flush() {
		rec.Record(ev)
	}
	zero := 0
	rec.Finalize(&zero)
	return rec.Summary
}

func shardLines(k, n int, kernel, userland []string) []string {
	out := []string{"KTAP\tTAP version 14", "KTAP\t1.." + itoa(len(kernel)), "KTAP\t# shard " + itoa(k) + "/" + itoa(n)}
	out = append(out, "KTAP\t# fixture slopos_fs::tests::vfs_ready ok")
	for i, l := range kernel {
		out = append(out, "KTAP\t"+strings.Replace(l, "IDX", itoa(i+1), 1))
	}
	out = append(out, "KTAP\t# elapsed_ms=10 pass=0 fail=0 skip=0 over_time=0")
	out = append(out, "KTAP\tTAP version 14", "KTAP\t1.."+itoa(len(userland)), "KTAP\t# shard "+itoa(k)+"/"+itoa(n))
	for i, l := range userland {
		out = append(out, "KTAP\t"+strings.Replace(l, "IDX", itoa(i+1), 1))
	}
	out = append(out, "KTAP\t# elapsed_ms=10 pass=0 fail=0 skip=0 over_time=0")
	return out
}

func itoa(n int) string {
	return strconv.Itoa(n)
}

func TestMergeShardSummariesSumsPhasesAndKeepsShard(t *testing.T) {
	one := summaryFromLines(t, 1, shardLines(1, 2,
		[]string{"ok IDX - boot::first # time_ms=1", "ok IDX - a::x # time_ms=1"},
		[]string{"ok IDX - u::p # time_ms=1"}))
	two := summaryFromLines(t, 2, shardLines(2, 2,
		[]string{"ok IDX - boot::first # time_ms=1", "not ok IDX - b::y # time_ms=1"},
		[]string{"ok IDX - u::q # time_ms=1", "ok IDX - u::r # SKIP nope"}))
	m := MergeShardSummaries([]*RunSummary{one, two}, []int{1, 2})

	if len(m.Phases) != 2 || m.Phases[0].Name != "kernel" || m.Phases[1].Name != "userland" {
		t.Fatalf("phases: got %+v", m.Phases)
	}
	if got := *m.Phases[0].PlanN; got != 4 {
		t.Errorf("kernel plan: got %d, want 4", got)
	}
	if got := *m.Phases[1].PlanN; got != 3 {
		t.Errorf("userland plan: got %d, want 3", got)
	}
	if m.Total() != 7 || m.CounterSum("pass") != 5 || m.CounterSum("fail") != 1 || m.CounterSum("skip") != 1 {
		t.Errorf("counters: total=%d pass=%d fail=%d skip=%d",
			m.Total(), m.CounterSum("pass"), m.CounterSum("fail"), m.CounterSum("skip"))
	}
	f := m.Failures()
	if len(f) != 1 || f[0].Name != "b::y" || f[0].Shard != 2 {
		t.Errorf("failures: got %+v", f)
	}
	if m.Truncated {
		t.Errorf("a merge of complete shards is not truncated")
	}
}

func TestMergeShardSummariesTagsBailAndAbortWithShard(t *testing.T) {
	green := summaryFromLines(t, 1, shardLines(1, 2, []string{"ok IDX - a::x"}, nil))
	bailed := summaryFromLines(t, 2, []string{
		"KTAP\tTAP version 14",
		"KTAP\t1..3",
		"KTAP\tBail out! fixture slopos_fs::tests::vfs_ready failed",
	})
	bailed.KernelAbort = true
	bailed.KernelAbortReason = "page fault"
	m := MergeShardSummaries([]*RunSummary{green, bailed}, []int{1, 2})
	if m.Phases[0].BailReason == nil || *m.Phases[0].BailReason != "shard 2: fixture slopos_fs::tests::vfs_ready failed" {
		t.Errorf("bail: got %v", m.Phases[0].BailReason)
	}
	if !m.KernelAbort || m.KernelAbortReason != "shard 2: page fault" {
		t.Errorf("abort: got %v %q", m.KernelAbort, m.KernelAbortReason)
	}
}

func TestCombineShardExit(t *testing.T) {
	cases := []struct {
		codes []int
		want  int
	}{
		{[]int{0, 0, 0}, 0},
		{[]int{0, 1, 0}, 1},
		{[]int{2, 1}, 1},
		{[]int{0, 2}, 2},
		{[]int{1, 130}, 130},
		{[]int{0, 64}, 64},
	}
	for _, c := range cases {
		if got := CombineShardExit(c.codes); got != c.want {
			t.Errorf("CombineShardExit(%v) = %d, want %d", c.codes, got, c.want)
		}
	}
}

func TestShardIndices(t *testing.T) {
	if got := shardIndices(3, 0); !equalInts(got, []int{1, 2, 3}) {
		t.Errorf("all: got %v", got)
	}
	if got := shardIndices(3, 2); !equalInts(got, []int{2}) {
		t.Errorf("only: got %v", got)
	}
}

func equalInts(a, b []int) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func TestSparseCopyKeepsBytesAndSize(t *testing.T) {
	dir := t.TempDir()
	src := filepath.Join(dir, "src.img")
	data := make([]byte, 300<<10)
	copy(data[70<<10:], "payload")
	data[len(data)-1] = 0x5a
	if err := os.WriteFile(src, data, 0o644); err != nil {
		t.Fatal(err)
	}
	dst := filepath.Join(dir, "sub", "dst.img")
	if err := sparseCopy(src, dst); err != nil {
		t.Fatal(err)
	}
	got, err := os.ReadFile(dst)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(got, data) {
		t.Errorf("copy differs from source")
	}
	// A stale longer copy from an earlier run must not survive.
	if err := os.WriteFile(src, data[:1000], 0o644); err != nil {
		t.Fatal(err)
	}
	if err := sparseCopy(src, dst); err != nil {
		t.Fatal(err)
	}
	if st, _ := os.Stat(dst); st.Size() != 1000 {
		t.Errorf("size: got %d, want 1000", st.Size())
	}
}

func TestJsonlEventsCarryTheShard(t *testing.T) {
	path := filepath.Join(t.TempDir(), "run.jsonl")
	sink, err := NewJsonlSink(path)
	if err != nil {
		t.Fatal(err)
	}
	rec := &TestRecord{PhaseIdx: 1, PhaseName: "kernel", Idx: 1, Name: "a::x", Outcome: OutcomePass, Shard: 2}
	if err := sink.Write(&EvTest{Record: rec}, 2); err != nil {
		t.Fatal(err)
	}
	if err := sink.Write(&EvFixture{PhaseIdx: 1, Name: "f"}, 2); err != nil {
		t.Fatal(err)
	}
	s := summaryFromLines(t, 2, shardLines(2, 2, []string{"ok IDX - a::x"}, nil))
	if err := sink.WriteShardEnd(2, s, 0); err != nil {
		t.Fatal(err)
	}
	if err := sink.Write(&EvTest{Record: rec}, 0); err != nil {
		t.Fatal(err)
	}
	if err := sink.Close(); err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(body)), "\n")
	if len(lines) != 4 {
		t.Fatalf("lines: got %d", len(lines))
	}
	for i, want := range []any{float64(2), float64(2), float64(2), nil} {
		var obj map[string]any
		if err := json.Unmarshal([]byte(lines[i]), &obj); err != nil {
			t.Fatal(err)
		}
		if obj["shard"] != want {
			t.Errorf("line %d (%v): shard %v, want %v", i, obj["t"], obj["shard"], want)
		}
	}
}

func TestFailureBlockNamesItsShard(t *testing.T) {
	s := summaryFromLines(t, 3, shardLines(3, 3, []string{"not ok IDX - a::x # time_ms=1"}, nil))
	m := MergeShardSummaries([]*RunSummary{s}, []int{3})
	var buf bytes.Buffer
	NewBarRenderer(&buf, "summary", false, 0, false, 100).Finalize(m)
	if !strings.Contains(buf.String(), "  shard:    3") {
		t.Errorf("failure block without its shard:\n%s", buf.String())
	}
}
