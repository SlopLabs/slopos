//! `/bin/cpufreq` and `prof` as a user runs them, on a machine that may report
//! no frequency at all: a hypervisor's CPUs have neither HWP nor the
//! APERF/MPERF pair, and every report must still be whole.

use slopos_userland as _;

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use slopos_slibc::test_harness::note;

fn spawn(path: &str, args: &[&str]) -> Option<Output> {
    match Command::new(path).args(args).output() {
        Ok(out) => Some(out),
        Err(e) => {
            note(&format!("spawning {path} {args:?} failed: {e:?}"));
            None
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `path args` exited 0; its stdout.
fn succeeds(path: &str, args: &[&str]) -> Option<String> {
    let out = spawn(path, args)?;
    if out.status.code() != Some(0) {
        note(&format!(
            "{path} {args:?} ended {:?}/{:?}: {}",
            out.status.code(),
            out.status.signal(),
            text(&out.stderr).trim()
        ));
        return None;
    }
    Some(text(&out.stdout))
}

fn nproc() -> Option<usize> {
    let out = succeeds("/bin/nproc", &[])?;
    out.trim().parse().ok()
}

fn test_status_has_a_row_per_cpu() -> bool {
    let (Some(cpus), Some(out)) = (nproc(), succeeds("/bin/cpufreq", &["status"])) else {
        return false;
    };
    let mut lines = out.lines().skip_while(|line| !line.starts_with("CPU "));
    if lines.next().is_none() {
        note(&format!("no per-CPU table in: {out}"));
        return false;
    }
    let rows = lines.take_while(|line| !line.is_empty()).count();
    if rows != cpus {
        note(&format!("{rows} per-CPU rows for {cpus} CPUs: {out}"));
        return false;
    }
    true
}

/// The single `CPUFREQ[run]:` line `cpufreq run` ends with.
fn run_line(stderr: &str) -> Option<&str> {
    let mut lines = stderr.lines().filter(|l| l.starts_with("CPUFREQ[run]: "));
    let line = lines.next();
    if line.is_none() || lines.next().is_some() {
        note(&format!("wanted one CPUFREQ[run] line in: {stderr}"));
        return None;
    }
    line
}

fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split(' ')
        .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
}

fn test_run_reports_the_command() -> bool {
    let Some(out) = spawn("/bin/cpufreq", &["run", "--", "/bin/true"]) else {
        return false;
    };
    let stderr = text(&out.stderr);
    if out.status.code() != Some(0) {
        note(&format!(
            "cpufreq run -- /bin/true ended {:?}: {stderr}",
            out.status
        ));
        return false;
    }
    let Some(line) = run_line(&stderr) else {
        return false;
    };
    let complete = [
        "wall_ms",
        "busy_ms",
        "eff_mhz",
        "p_busy_ms",
        "p_eff_mhz",
        "e_busy_ms",
        "e_eff_mhz",
        "policy",
        "epp",
        "placement",
        "pkg_temp_max",
        "limited",
    ]
    .iter()
    .all(|key| field(line, key).is_some_and(|v| !v.is_empty()));
    let wall_ok = field(line, "wall_ms").is_some_and(|v| v.parse::<u64>().is_ok());
    if !complete || !wall_ok || field(line, "status") != Some("0") {
        note(&format!("incomplete run line: {line}"));
        return false;
    }
    true
}

fn test_run_passes_the_exit_status_on() -> bool {
    let Some(out) = spawn("/bin/cpufreq", &["run", "--", "/bin/false"]) else {
        return false;
    };
    let stderr = text(&out.stderr);
    let Some(line) = run_line(&stderr) else {
        return false;
    };
    if out.status.code() != Some(1) || field(line, "status") != Some("1") {
        note(&format!(
            "cpufreq run -- /bin/false ended {:?}: {line}",
            out.status
        ));
        return false;
    }
    true
}

/// Without the Power grant (`EPERM`) or without HWP (`EOPNOTSUPP`) the
/// change is refused, said so, and nothing crashes.
fn test_unsupported_set_fails_cleanly() -> bool {
    let Some(out) = spawn("/bin/cpufreq", &["set", "epp", "performance"]) else {
        return false;
    };
    let stderr = text(&out.stderr);
    match out.status.code() {
        Some(code) if code != 0 && stderr.starts_with("cpufreq: ") => true,
        _ => {
            note(&format!(
                "cpufreq set epp performance ended {:?}: {stderr}",
                out.status
            ));
            false
        }
    }
}

fn spin(duration: Duration) {
    let start = Instant::now();
    let mut x = 1u64;
    while start.elapsed() < duration {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
    }
}

fn prof_state() -> Option<String> {
    Some(succeeds("/bin/prof", &["status"])?.trim().to_owned())
}

fn reports_under_utest() -> bool {
    spin(Duration::from_millis(100));
    let Some(report) = succeeds("/bin/prof", &["report", "utest"]) else {
        return false;
    };
    let stray = report.lines().find(|l| !l.starts_with("PROF[utest]: "));
    if report.lines().next().is_none() || stray.is_some() {
        note(&format!("prof report utest printed: {report}"));
        return false;
    }
    true
}

/// The profiler is left as found, whatever the outcome: a boot with
/// `prof=on` keeps sampling into its tables, and one without stays stopped.
fn test_prof_reports_under_a_label() -> bool {
    let Some(state) = prof_state() else {
        return false;
    };
    if state != "stopped" {
        return reports_under_utest();
    }
    let started = succeeds("/bin/prof", &["start"]).is_some();
    let running = started && prof_state().as_deref() == Some("running");
    if started && !running {
        note("prof status is not running after prof start");
    }
    let reported = running && reports_under_utest();
    let stopped =
        succeeds("/bin/prof", &["stop"]).is_some() && prof_state().as_deref() == Some("stopped");
    if !stopped {
        note("the profiler is not stopped after prof stop");
    }
    reported && stopped
}

const CASES: &[(&str, fn() -> bool)] = &[
    ("status_has_a_row_per_cpu", test_status_has_a_row_per_cpu),
    ("run_reports_the_command", test_run_reports_the_command),
    (
        "run_passes_the_exit_status_on",
        test_run_passes_the_exit_status_on,
    ),
    (
        "unsupported_set_fails_cleanly",
        test_unsupported_set_fails_cleanly,
    ),
    (
        "prof_reports_under_a_label",
        test_prof_reports_under_a_label,
    ),
];

fn main() {
    slopos_slibc::test_harness::run(CASES);
}
