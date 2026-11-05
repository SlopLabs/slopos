package gates

import (
	"fmt"
	"strings"
	"testing"
)

// ktapRun renders one boot: a kernel and a userland phase, each a plan line
// and one `ok` per name.
func ktapRun(kernel, userland []string) string {
	var b strings.Builder
	phase := func(label string, names []string) {
		fmt.Fprintf(&b, "TESTS: Starting %s phase\r\nKTAP\tTAP version 14\r\nKTAP\t1..%d\r\n", label, len(names))
		for i, n := range names {
			fmt.Fprintf(&b, "KTAP\tok %d - %s # time_ms=0\r\n", i+1, n)
		}
		fmt.Fprintf(&b, "KTAP\t# elapsed_ms=1 pass=%d fail=0 skip=0 over_time=0\r\n", len(names))
	}
	phase("kernel", kernel)
	b.WriteString("LOCKDEP[post-kernel-tests]: ACTIVE ...\r\n")
	phase("userland", userland)
	return b.String()
}

const testListGate = `# header
kernel	a::one
kernel	a::two
userland	core::utests::utest_x
`

func testListFixture(t *testing.T) *fixture {
	f := newFixture(t)
	f.gateFile("tests", "tests", testListGate)
	return f
}

var (
	kernelNames   = []string{"a::one", "a::two"}
	userlandNames = []string{"core::utests::utest_x"}
)

func TestTestListAccepts(t *testing.T) {
	f := testListFixture(t)
	expect(t, "every listed test ran once", f.run("tests", []string{f.log("ok.log", ktapRun(kernelNames, userlandNames))}), 0, "2 kernel + 1 userland tests")

	// A failing test, a skipped one, a result glued to a TTY test's output,
	// and subtests indented under the utest: all present, subtests uncounted.
	odd := lines(
		"KTAP\tTAP version 14", "KTAP\t1..2",
		"KTAP\tnot ok 1 - a::one # time_ms=3",
		"KTAP\t  ---", "KTAP\t  outcome: Fail", "KTAP\t  log: |",
		"KTAP\t   captured line",
		"helloKTAP\tok 9 - fake::inside_a_literal",
		"KTAP\t  ...",
		"\x1b[AhelloKTAP\tok 2 - a::two # SKIP not on this machine",
		"KTAP\tTAP version 14", "KTAP\t1..1", "KTAP\t# fixture vfs_ready ok",
		"KTAP\t  ok 1 - a subtest name with spaces",
		"KTAP\t  not ok 2 - another_subtest",
		"KTAP\tok 1 - core::utests::utest_x # time_ms=85",
	)
	expect(t, "fail, skip, glued and subtests", f.run("tests", []string{f.log("odd.log", odd)}), 0)

	// One name in both phases is two tests.
	f.gateFile("tests", "both", "kernel\ta::one\nuserland\ta::one\n")
	expect(t, "same name in two phases", f.run("tests", []string{f.log("both.log", ktapRun([]string{"a::one"}, []string{"a::one"}))}, "--variant", "both"), 0)
}

func TestTestListRejects(t *testing.T) {
	f := testListFixture(t)
	r := f.run("tests", []string{f.log("missing.log", ktapRun([]string{"a::one"}, userlandNames))})
	expect(t, "a listed test that did not run", r, 1, "1 listed test(s) did not run", "kernel a::two")

	r = f.run("tests", []string{f.log("extra.log", ktapRun(append(kernelNames, "a::three"), userlandNames))})
	expect(t, "an unlisted test", r, 1, "1 test(s) ran but are not listed", "kernel a::three",
		"run_tests gate tests --variant tests --emit-allowlist")

	// One test registered twice; the plan sum counted it as two.
	r = f.run("tests", []string{f.log("dup.log", ktapRun([]string{"a::one", "a::two", "a::one"}, userlandNames))})
	expect(t, "a duplicate within a phase", r, 1, "kernel a::one ×2 in", "registered twice")

	r = f.run("tests", []string{f.log("noplan.log", "kernel panicked before the harness\n")})
	expect(t, "a log with no plan line", r, 1, "no KTAP plan line was emitted")

	r = f.run("tests", []string{f.log("moved.log", ktapRun(nil, append(kernelNames, userlandNames...)))})
	expect(t, "a test that moved phase", r, 1, "kernel a::one", "userland a::one")

	many := make([]string, 0, 100)
	for i := range 100 {
		many = append(many, fmt.Sprintf("b::t%03d", i))
	}
	r = f.run("tests", []string{f.log("many.log", ktapRun(append(many, kernelNames...), userlandNames))})
	expect(t, "a long list is capped but counted", r, 1, "100 test(s) ran but are not listed", "… and 60 more")
}

func TestTestListGateFileErrors(t *testing.T) {
	cases := []struct{ label, gate, want string }{
		{"unsorted", "kernel\ta::two\nkernel\ta::one\n", "out of order or repeated"},
		{"repeated", "kernel\ta::one\nkernel\ta::one\n", "out of order or repeated"},
		{"malformed", "kernel a::one\n", "want '<phase> <TAB> <module::name>'"},
		{"empty", "# nothing\n", "lists no tests"},
	}
	for _, c := range cases {
		f := newFixture(t)
		f.gateFile("tests", "tests", c.gate)
		expect(t, c.label, f.run("tests", []string{f.log("ok.log", ktapRun(kernelNames, userlandNames))}), 2, c.want)
	}
	f := newFixture(t)
	expect(t, "no gate data", f.run("tests", []string{f.log("ok.log", ktapRun(kernelNames, userlandNames))}), 2, "no gate data")
}

func TestTestListLogs(t *testing.T) {
	f := testListFixture(t)
	a := f.log("a.log", ktapRun(kernelNames, userlandNames))
	b := f.log("b.log", ktapRun(kernelNames, userlandNames))
	expect(t, "two complete runs", f.run("tests", []string{a, b}), 0, "in each of 2 log(s)")

	// Every log is a whole run, so two partial ones do not add up to one.
	half1 := f.log("half1.log", ktapRun([]string{"a::one"}, nil))
	half2 := f.log("half2.log", ktapRun([]string{"a::two"}, userlandNames))
	expect(t, "partial runs fail on their own", f.run("tests", []string{half1, half2}), 1,
		"kernel a::two in", "half1.log", "kernel a::one in", "half2.log")

	dead := f.log("dead.log", "boot panic\n")
	expect(t, "a log with no plan line fails the run", f.run("tests", []string{a, b, dead}), 1, "dead.log: no KTAP plan line")

	dupInLog := f.log("dup.log", ktapRun([]string{"a::one", "a::two", "a::two"}, userlandNames))
	expect(t, "a duplicate within one log", f.run("tests", []string{a, dupInLog}), 1, "kernel a::two ×2 in")
}

func TestTestListEmitRoundTrips(t *testing.T) {
	f := newFixture(t)
	a := f.log("a.log", ktapRun([]string{"z::last", "a::first", "m::mid"}, userlandNames))
	b := f.log("b.log", ktapRun([]string{"z::last", "a::first", "m::mid"}, userlandNames))
	out := f.emitInto("tests", "tests", []string{a, b})
	body := out[strings.Index(out, "\nkernel"):]
	if want := "\nkernel\ta::first\nkernel\tm::mid\nkernel\tz::last\nuserland\tcore::utests::utest_x\n"; body != want {
		t.Errorf("emitted list %q, want %q", body, want)
	}
	if !strings.Contains(out, "Explicit tests run only when named") {
		t.Errorf("emitted header does not say explicit tests are absent:\n%s", out)
	}
	expect(t, "emitted list accepts its own logs", f.run("tests", []string{a, b}), 0)
	expect(t, "emit refuses a log that measured nothing",
		f.run("tests", []string{f.log("dead.log", "panic\n")}, "--emit-allowlist"), 1, "no KTAP plan line")
	short := f.log("short.log", ktapRun([]string{"a::first"}, userlandNames))
	expect(t, "emit refuses runs that ran different tests",
		f.run("tests", []string{a, short}, "--emit-allowlist"), 1, "ran different tests")
}
