package main

import (
	"strings"
	"testing"
)

const testBase = "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on"

func TestAssembleNoFilterNoSkipNoVerbosity(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "", "")
	if got != testBase {
		t.Errorf("got %q, want %q", got, testBase)
	}
}

func TestAssembleSingleFilter(t *testing.T) {
	got := assembleTestCmdline(testBase, []string{"mm::*"}, nil, "", "")
	if !strings.Contains(got, "tests.run=mm::*") {
		t.Errorf("missing tests.run=mm::* in %q", got)
	}
}

func TestAssembleMultipleFiltersJoinedWithComma(t *testing.T) {
	got := assembleTestCmdline(testBase, []string{"mm::*", "core::*"}, nil, "", "")
	if !strings.Contains(got, "tests.run=mm::*,core::*") {
		t.Errorf("missing comma-joined filters in %q", got)
	}
}

func TestAssembleSkipGlob(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, []string{"*::tcp_live::*"}, "", "")
	if !strings.Contains(got, "tests.skip=*::tcp_live::*") {
		t.Errorf("missing tests.skip in %q", got)
	}
}

func TestAssembleVerbosityReplacesDefault(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "verbose", "")
	if !strings.Contains(got, "tests.verbosity=verbose") {
		t.Errorf("missing tests.verbosity=verbose in %q", got)
	}
	if strings.Contains(got, "tests.verbosity=summary") {
		t.Errorf("default tests.verbosity=summary should have been replaced in %q", got)
	}
}

func TestAssembleExtraAppended(t *testing.T) {
	got := assembleTestCmdline(testBase, nil, nil, "", "tests.warn_ms=500")
	if !strings.HasSuffix(got, "tests.warn_ms=500") {
		t.Errorf("expected suffix tests.warn_ms=500, got %q", got)
	}
}

func TestPreprocessFlagValueFusion(t *testing.T) {
	in := []string{"--filter", "mm::*", "--skip", "tcp_live::*", "--verbose"}
	out := preprocessArgv(in)
	want := []string{"--filter=mm::*", "--skip=tcp_live::*", "--verbose"}
	if !equalSlices(out, want) {
		t.Errorf("got %v, want %v", out, want)
	}
}

func TestPreprocessAlreadyFusedPassesThrough(t *testing.T) {
	in := []string{"--filter=mm::*", "--no-color"}
	out := preprocessArgv(in)
	if !equalSlices(out, in) {
		t.Errorf("got %v, want unchanged %v", out, in)
	}
}

func TestPreprocessLeavesBooleansAlone(t *testing.T) {
	in := []string{"--rerun-failed", "--verbose", "--quiet", "--raw", "--no-build", "--dry-run"}
	out := preprocessArgv(in)
	if !equalSlices(out, in) {
		t.Errorf("booleans should pass through unchanged; got %v", out)
	}
}

func TestParseArgsNoColor(t *testing.T) {
	args, err := parseArgs(preprocessArgv([]string{"--no-color"}))
	if err != nil {
		t.Fatal(err)
	}
	if !args.NoColor || UseColour(args.ColorMode, args.NoColor, 0) {
		t.Errorf("--no-color: NoColor %v, ColorMode %q", args.NoColor, args.ColorMode)
	}
}

func TestParseArgsColorTakesAutoOrAlways(t *testing.T) {
	for _, mode := range []string{"auto", "always"} {
		args, err := parseArgs(preprocessArgv([]string{"--color", mode}))
		if err != nil || args.ColorMode != mode {
			t.Errorf("--color %s: args %+v, err %v", mode, args, err)
		}
	}
	if _, err := parseArgs(preprocessArgv([]string{"--color=never"})); err == nil {
		t.Errorf("--color=never: want an error, --no-color is the spelling")
	}
	if _, err := parseArgs(preprocessArgv([]string{"--color=always", "--no-color"})); err == nil {
		t.Errorf("--color=always with --no-color: want a mutex error")
	}
}

func TestUseColourAlwaysOverridesNoColorEnv(t *testing.T) {
	t.Setenv("NO_COLOR", "1")
	if !UseColour("always", false, 0) {
		t.Errorf("--color=always must colour under NO_COLOR")
	}
	if UseColour("auto", false, 0) {
		t.Errorf("auto must honour NO_COLOR")
	}
}

func TestParseArgsEmptyFilterDropped(t *testing.T) {
	// The justfile invokes `--filter ""`, which preprocess fuses to `--filter=`.
	args, err := parseArgs(preprocessArgv([]string{"--filter="}))
	if err != nil {
		t.Fatal(err)
	}
	if len(args.Filters) != 0 {
		t.Errorf("filters: got %v, want empty", args.Filters)
	}
}

func TestParseArgsFilterAndRerunFailedMutuallyExclusive(t *testing.T) {
	_, err := parseArgs(preprocessArgv([]string{"--filter", "x", "--rerun-failed"}))
	if err == nil {
		t.Errorf("expected mutex error, got nil")
	}
}

func TestParseArgsVerbosityModesMutuallyExclusive(t *testing.T) {
	_, err := parseArgs(preprocessArgv([]string{"--verbose", "--quiet"}))
	if err == nil {
		t.Errorf("expected verbose+quiet mutex error")
	}
	_, err = parseArgs(preprocessArgv([]string{"--quiet", "--raw"}))
	if err == nil {
		t.Errorf("expected quiet+raw mutex error")
	}
}

func TestParseArgsShardsDefaultToOneBoot(t *testing.T) {
	args, err := parseArgs(preprocessArgv(nil))
	if err != nil {
		t.Fatal(err)
	}
	if args.Shards != 1 || args.Jobs != 0 || args.OnlyShard != 0 || args.CaptureDir != "" {
		t.Errorf("got shards=%d jobs=%d only=%d dir=%q, want 1/0/0/\"\"",
			args.Shards, args.Jobs, args.OnlyShard, args.CaptureDir)
	}
}

func TestParseArgsShardFlags(t *testing.T) {
	args, err := parseArgs(preprocessArgv([]string{
		"--shards", "3", "--jobs", "2", "--capture-dir", "/tmp/caps", "--raw",
	}))
	if err != nil {
		t.Fatal(err)
	}
	if args.Shards != 3 || args.Jobs != 2 || args.CaptureDir != "/tmp/caps" || !args.Raw {
		t.Errorf("got %+v", args)
	}
}

func TestParseArgsOnlyShardRunsOneOfN(t *testing.T) {
	args, err := parseArgs(preprocessArgv([]string{"--shards", "4", "--only-shard", "3"}))
	if err != nil {
		t.Fatal(err)
	}
	if args.Shards != 4 || args.OnlyShard != 3 {
		t.Errorf("got shards=%d only=%d, want 4/3", args.Shards, args.OnlyShard)
	}
	if got := shardIndices(args.Shards, args.OnlyShard); !equalInts(got, []int{3}) {
		t.Errorf("boots: got %v, want [3]", got)
	}
}

func TestParseArgsRejectsBadShardFlags(t *testing.T) {
	for _, argv := range [][]string{
		{"--shards", "0"},
		{"--shards=-2"},
		{"--jobs=-1"},
		{"--shards", "2", "--only-shard", "3"},
		{"--only-shard", "2"},
		{"--shards=2", "--only-shard=-1"},
		{"--shards", "2", "--iso", "x.iso"},
	} {
		if _, err := parseArgs(preprocessArgv(argv)); err == nil {
			t.Errorf("%v: expected an error", argv)
		}
	}
}

func TestParseArgsOnlyShardOfOneIsTheWholeRun(t *testing.T) {
	args, err := parseArgs(preprocessArgv([]string{"--shards", "1", "--only-shard", "1"}))
	if err != nil {
		t.Fatal(err)
	}
	if args.Shards != 1 {
		t.Errorf("shards: got %d", args.Shards)
	}
}

func equalSlices(a, b []string) bool {
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
