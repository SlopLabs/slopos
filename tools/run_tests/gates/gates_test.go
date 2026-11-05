package gates

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
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

// The repository's full captures, each graded alone and as two or three
// identical runs, must get the same verdict from every gate.
const fullCaptures = "../../../builddir/ci-test*.log"

func TestRepeatedRunsKeepEveryVerdict(t *testing.T) {
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
			dir := t.TempDir()
			runs := map[int][]string{}
			for _, n := range []int{1, 2, 3} {
				for i := range n {
					p := filepath.Join(dir, fmt.Sprintf("runs%d-%d.log", n, i))
					if err := os.WriteFile(p, data, 0o644); err != nil {
						t.Fatal(err)
					}
					runs[n] = append(runs[n], p)
				}
			}
			for _, gate := range []string{"tests", "lockdep", "sched", "quota", "fsperf"} {
				verdicts := map[int]int{}
				for n, logs := range runs {
					args := []string{gate}
					for _, l := range logs {
						args = append(args, "--log", l)
					}
					var out, errOut bytes.Buffer
					verdicts[n] = run(args, repo, &out, &errOut)
				}
				t.Logf("%s: one=%d two=%d three=%d", gate, verdicts[1], verdicts[2], verdicts[3])
				if verdicts[2] != verdicts[1] || verdicts[3] != verdicts[1] {
					t.Errorf("%s: repeating the run changed the verdict", gate)
				}
			}
		})
	}
}
