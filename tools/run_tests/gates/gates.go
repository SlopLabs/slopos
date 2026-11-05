// Package gates grades captured test runs: the invariants and ratchets a
// boot's serial log is held to. Each gate is a file in this package that
// registers itself from init(); `run_tests gate NAME --log FILE...` runs it.
// Several captures can be graded at once, one --log each, every one a whole
// run of the suite: each gate states how it aggregates them.
package gates

import (
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"sort"
	"strings"
)

// Config is what every gate is handed.
type Config struct {
	// Logs are raw captures (`run_tests --raw --no-color`), one per boot.
	Logs []string
	// Variant names the gate data under scripts/gates/<gate>/<variant>.txt.
	Variant string
	// Emit prints a fresh gate file measured from Logs instead of grading.
	Emit bool
	// RequireCapacity makes fsperf's FSCAP rows mandatory: set for a boot
	// with the capacity volume attached, where their absence is a failure.
	RequireCapacity bool
	// RepoRoot is the checkout whose scripts/gates/ the gate reads.
	RepoRoot string
	// Out receives emitted gate files and pass summaries.
	Out io.Writer
}

// Gate grades Logs; a nil error is a pass. An error wrapping a
// *GateFileError is a gate file the gate cannot apply (exit 2); any other
// error is a failed run (exit 1).
type Gate struct {
	Name    string
	Summary string
	Run     func(Config) error
}

// GateFileError is a gate file that cannot be applied — missing, malformed,
// or naming an unknown directive. It is misuse, never a regression to
// re-measure.
type GateFileError struct{ Msg string }

func (e *GateFileError) Error() string { return e.Msg }

func gateFileErrorf(format string, args ...any) error {
	return &GateFileError{Msg: fmt.Sprintf(format, args...)}
}

var registry = map[string]Gate{}

func register(g Gate) {
	if _, dup := registry[g.Name]; dup {
		panic("gates: duplicate gate " + g.Name)
	}
	registry[g.Name] = g
}

type logList []string

func (l *logList) String() string     { return strings.Join(*l, ",") }
func (l *logList) Set(v string) error { *l = append(*l, v); return nil }

// Main runs `run_tests gate NAME... --log FILE... [--variant V] [--emit-allowlist]
// [--require-capacity]` and returns the process exit code: 0 pass, 1 a gate
// failed, 2 misuse or an unusable gate file.
func Main(args []string, repoRoot string) int {
	return run(args, repoRoot, os.Stdout, os.Stderr)
}

func run(args []string, repoRoot string, stdout, stderr io.Writer) int {
	fs := flag.NewFlagSet("run_tests gate", flag.ContinueOnError)
	fs.SetOutput(stderr)
	var logs logList
	fs.Var(&logs, "log", "raw capture of one whole run to grade (repeatable)")
	variant := fs.String("variant", "tests", "gate data variant")
	emit := fs.Bool("emit-allowlist", false, "print a gate file measured from the logs")
	requireCapacity := fs.Bool("require-capacity", false, "fsperf: fail when no log carries the FSCAP rows")
	fs.Usage = func() {
		fmt.Fprintln(stderr, "usage: run_tests gate NAME... --log FILE [--log FILE...] [--variant V] [--emit-allowlist] [--require-capacity]")
		fmt.Fprintln(stderr, "gates:")
		for _, name := range names() {
			fmt.Fprintf(stderr, "  %-14s %s\n", name, registry[name].Summary)
		}
	}
	// Flags and gate names may interleave.
	var selected []string
	rest := args
	for {
		if err := fs.Parse(rest); err != nil {
			return 2
		}
		if fs.NArg() == 0 {
			break
		}
		selected = append(selected, fs.Arg(0))
		rest = fs.Args()[1:]
	}
	if len(selected) == 0 || len(logs) == 0 {
		fs.Usage()
		return 2
	}
	if len(selected) == 1 && selected[0] == "all" {
		selected = names()
	}
	if *emit && len(selected) != 1 {
		fmt.Fprintln(stderr, "run_tests gate: --emit-allowlist prints one gate file; name exactly one gate")
		return 2
	}
	for _, name := range selected {
		if _, ok := registry[name]; !ok {
			fmt.Fprintf(stderr, "run_tests gate: no gate named %q\n", name)
			return 2
		}
	}
	cfg := Config{
		Logs:            logs,
		Variant:         *variant,
		Emit:            *emit,
		RequireCapacity: *requireCapacity,
		RepoRoot:        repoRoot,
		Out:             stdout,
	}
	code := 0
	for _, name := range selected {
		err := registry[name].Run(cfg)
		if err == nil {
			continue
		}
		fmt.Fprintf(stderr, "%s: FAIL\n", name)
		for _, line := range strings.Split(err.Error(), "\n") {
			fmt.Fprintf(stderr, "  %s\n", line)
		}
		var fileErr *GateFileError
		if errors.As(err, &fileErr) {
			code = 2
		} else if code == 0 {
			code = 1
		}
	}
	return code
}

func names() []string {
	out := make([]string, 0, len(registry))
	for n := range registry {
		out = append(out, n)
	}
	sort.Strings(out)
	return out
}
