package gates

import (
	"errors"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
)

// capture is one boot's raw serial log, split into lines with the CR the
// serial console leaves on each one removed.
type capture struct {
	path  string
	lines []string
}

func loadCaptures(paths []string) ([]capture, error) {
	out := make([]capture, 0, len(paths))
	for _, p := range paths {
		data, err := os.ReadFile(p)
		if err != nil {
			return nil, fmt.Errorf("cannot read log %s: %v — nothing was measured", p, err)
		}
		lines := strings.Split(string(data), "\n")
		for i, l := range lines {
			lines[i] = strings.TrimRight(l, "\r")
		}
		out = append(out, capture{path: p, lines: lines})
	}
	return out, nil
}

// reportAt returns the line from the first match of marker on. Report lines
// go out over a console other CPUs and TTY tests are writing to, so one can
// arrive with a foreign prefix glued to its head.
func reportAt(line string, marker *regexp.Regexp) (string, bool) {
	loc := marker.FindStringIndex(line)
	if loc == nil {
		return "", false
	}
	return line[loc[0]:], true
}

// num parses a field a regexp has already proven to be digits, saturating
// instead of wrapping so an absurd value still fails every ceiling.
func num(s string) int64 {
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil {
		if s != "" && s[0] == '-' {
			return math.MinInt64
		}
		return math.MaxInt64
	}
	return n
}

// findings collects every reason a run fails, so one run reports them all.
type findings []string

func (f *findings) add(format string, args ...any) {
	*f = append(*f, fmt.Sprintf(format, args...))
}

func (f findings) err() error {
	if len(f) == 0 {
		return nil
	}
	return errors.New(strings.Join(f, "\n"))
}

// gateLine is one directive of a gate file with its comment stripped.
type gateLine struct {
	no   int
	text string
}

func gateRel(gate, variant string) string {
	return filepath.Join("scripts", "gates", gate, variant+".txt")
}

// emitCommand is the documented way to (re)measure a gate file.
func emitCommand(gate, variant string) string {
	return fmt.Sprintf("run_tests gate %s --variant %s --emit-allowlist --log RUN.log [--log RUN.log...] > %s",
		gate, variant, gateRel(gate, variant))
}

// readGateFile returns the directives of scripts/gates/<gate>/<variant>.txt.
// A missing file is a gate-file error: every graded variant needs its own
// measured baseline.
func readGateFile(cfg Config, gate string) (string, []gateLine, error) {
	rel := gateRel(gate, cfg.Variant)
	data, err := os.ReadFile(filepath.Join(cfg.RepoRoot, rel))
	if err != nil {
		return rel, nil, gateFileErrorf("no gate data at %s (%v)\nEvery graded variant needs its own measured baseline. Create it with:\n    %s",
			rel, err, emitCommand(gate, cfg.Variant))
	}
	var out []gateLine
	for i, raw := range strings.Split(string(data), "\n") {
		text := raw
		if idx := strings.IndexByte(text, '#'); idx >= 0 {
			text = text[:idx]
		}
		text = strings.TrimRight(text, " \t\r")
		if strings.TrimSpace(text) == "" {
			continue
		}
		out = append(out, gateLine{no: i + 1, text: text})
	}
	return rel, out, nil
}

// parseCount reads a non-negative integer gate value.
func parseCount(rel string, l gateLine, s string) (int64, error) {
	n, err := strconv.ParseInt(s, 10, 64)
	if err != nil || n < 0 {
		return 0, gateFileErrorf("%s:%d: want a non-negative integer, got %q", rel, l.no, s)
	}
	return n, nil
}
