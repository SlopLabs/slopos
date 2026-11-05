package gates

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

func TestMainUsage(t *testing.T) {
	f := newFixture(t)
	l := f.log("x.log", "")
	cases := []struct {
		label string
		args  []string
		want  string
	}{
		{"no log", []string{"tests"}, "usage:"},
		{"no gate", []string{"--log", l}, "usage:"},
		{"unknown gate", []string{"nope", "--log", l}, `no gate named "nope"`},
		{"emit names one gate", []string{"tests", "lockdep", "--log", l, "--emit-allowlist"}, "name exactly one gate"},
	}
	for _, c := range cases {
		var out, errOut bytes.Buffer
		code := run(c.args, f.root, &out, &errOut)
		expect(t, c.label, result{code, out.String(), errOut.String()}, 2, c.want)
	}
}

// The repository's full captures, each graded as one boot and as the same run
// split into shards, must get the same verdict from every gate.
const fullCaptures = "../../../builddir/ci-test*.log"

var (
	splitDenialsRE = regexp.MustCompile(`denials=\d+`)
	splitResultRE  = regexp.MustCompile(`^(ok|not ok) \d+ - \S+`)
)

// splitCapture deals one boot's log out as n shard-shaped boots: every shard
// keeps the boot and the per-phase reports; each top-level result goes to one
// shard; the lines a single test measures (FSPERF, FSCAP, QUOTACOST) go to
// shard 0; and a test phase's denials are dealt across the shards so only
// their sum is the original — a gate taking the max instead would see less.
func splitCapture(text string, n int) []string {
	shards := make([]strings.Builder, n)
	next := 0
	for _, line := range strings.SplitAfter(text, "\n") {
		target := -1
		body := line
		if i := strings.Index(line, ktapPrefix); i >= 0 {
			body = strings.TrimRight(line[i+len(ktapPrefix):], "\r\n")
		}
		switch {
		case body != line && splitResultRE.MatchString(body):
			target = next % n
			next++
		case strings.Contains(line, "FSPERF[") || strings.Contains(line, "FSCAP[") || strings.Contains(line, "QUOTACOST"):
			target = 0
		}
		for s := range shards {
			if target >= 0 && s != target {
				continue
			}
			out := line
			if strings.Contains(line, "QUOTA[") && !strings.Contains(line, "QUOTA[boot]") {
				out = splitDenialsRE.ReplaceAllStringFunc(line, func(field string) string {
					d := int(num(strings.TrimPrefix(field, "denials=")))
					share := d / n
					if s < d%n {
						share++
					}
					return fmt.Sprintf("denials=%d", share)
				})
			}
			shards[s].WriteString(out)
		}
	}
	out := make([]string, n)
	for i := range shards {
		out[i] = shards[i].String()
	}
	return out
}

func TestShardSplitKeepsEveryVerdict(t *testing.T) {
	captures, _ := filepath.Glob(fullCaptures)
	if len(captures) == 0 {
		t.Skipf("no full capture matches %s", fullCaptures)
	}
	repo, err := filepath.Abs("../../..")
	if err != nil {
		t.Fatal(err)
	}
	for _, capture := range captures {
		t.Run(filepath.Base(capture), func(t *testing.T) {
			data, err := os.ReadFile(capture)
			if err != nil {
				t.Fatal(err)
			}
			// The split must actually divide the suite, or this proves nothing.
			parts := splitCapture(string(data), 2)
			whole := strings.Count(string(data), "KTAP\tok ")
			if a, b := strings.Count(parts[0], "KTAP\tok "), strings.Count(parts[1], "KTAP\tok "); a == 0 || b == 0 || a+b != whole {
				t.Fatalf("splitCapture dealt %d+%d of %d results", a, b, whole)
			}
			dir := t.TempDir()
			runs := map[string][]string{"single": {capture}}
			for _, n := range []int{2, 3} {
				name := fmt.Sprintf("split%d", n)
				for i, shard := range splitCapture(string(data), n) {
					p := filepath.Join(dir, fmt.Sprintf("%s-%d.log", name, i))
					if err := os.WriteFile(p, []byte(shard), 0o644); err != nil {
						t.Fatal(err)
					}
					runs[name] = append(runs[name], p)
				}
			}
			for _, gate := range []string{"tests", "lockdep", "sched", "quota", "fsperf"} {
				verdicts := map[string]int{}
				for name, logs := range runs {
					args := []string{gate}
					for _, l := range logs {
						args = append(args, "--log", l)
					}
					var out, errOut bytes.Buffer
					verdicts[name] = run(args, repo, &out, &errOut)
				}
				t.Logf("%s: single=%d split2=%d split3=%d", gate, verdicts["single"], verdicts["split2"], verdicts["split3"])
				if verdicts["split2"] != verdicts["single"] || verdicts["split3"] != verdicts["single"] {
					t.Errorf("%s: sharding changed the verdict", gate)
				}
			}
		})
	}
}
