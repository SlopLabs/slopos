package gates

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// fixture is a throwaway checkout: gate files under scripts/gates/ and logs
// beside them, graded through the same entry point the CLI uses.
type fixture struct {
	t    *testing.T
	root string
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	return &fixture{t: t, root: t.TempDir()}
}

func (f *fixture) gateFile(gate, variant, content string) {
	f.t.Helper()
	path := filepath.Join(f.root, gateRel(gate, variant))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		f.t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fixture) removeGateFile(gate, variant string) {
	f.t.Helper()
	if err := os.Remove(filepath.Join(f.root, gateRel(gate, variant))); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fixture) log(name, content string) string {
	f.t.Helper()
	path := filepath.Join(f.root, name)
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		f.t.Fatal(err)
	}
	return path
}

type result struct {
	code        int
	out, errOut string
}

func (r result) all() string { return r.out + r.errOut }

// run grades logs with gate; extra are further CLI flags.
func (f *fixture) run(gate string, logs []string, extra ...string) result {
	f.t.Helper()
	args := []string{gate}
	for _, l := range logs {
		args = append(args, "--log", l)
	}
	args = append(args, extra...)
	var out, errOut bytes.Buffer
	code := run(args, f.root, &out, &errOut)
	return result{code, out.String(), errOut.String()}
}

// expect asserts the exit code and that every want appears in the output.
func expect(t *testing.T, label string, r result, code int, want ...string) {
	t.Helper()
	if r.code != code {
		t.Errorf("[%s] exit %d, want %d\n%s", label, r.code, code, indentText(r.all()))
		return
	}
	for _, w := range want {
		if !strings.Contains(r.all(), w) {
			t.Errorf("[%s] output missing %q\n%s", label, w, indentText(r.all()))
		}
	}
}

func expectNot(t *testing.T, label string, r result, unwanted string) {
	t.Helper()
	if strings.Contains(r.all(), unwanted) {
		t.Errorf("[%s] output contains %q and must not\n%s", label, unwanted, indentText(r.all()))
	}
}

func indentText(s string) string {
	return "    " + strings.ReplaceAll(strings.TrimRight(s, "\n"), "\n", "\n    ")
}

// emit runs gate with --emit-allowlist and installs the result as the
// variant's gate file, so the next run grades against what was measured.
func (f *fixture) emitInto(gate, variant string, logs []string) string {
	f.t.Helper()
	r := f.run(gate, logs, "--variant", variant, "--emit-allowlist")
	if r.code != 0 {
		f.t.Fatalf("emit %s exited %d\n%s", gate, r.code, indentText(r.all()))
	}
	f.gateFile(gate, variant, r.out)
	return r.out
}

func lines(ls ...string) string { return strings.Join(ls, "\n") + "\n" }
