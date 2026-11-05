//! What a gate found, and how it is printed.

use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// `path:line`, a symbol, or whatever names the place; never empty.
    pub location: String,
    pub message: String,
}

/// A gate's verdict: it passed exactly when it found nothing.
#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    /// One line saying what was measured, printed on a pass, so a gate that
    /// scanned nothing cannot read the same as one that scanned the tree.
    pub summary: String,
}

impl Report {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn finding(&mut self, location: impl Into<String>, message: impl Into<String>) {
        self.findings.push(Finding {
            location: location.into(),
            message: message.into(),
        });
    }

    pub fn at(&mut self, path: &str, line: usize, message: impl Into<String>) {
        self.finding(format!("{path}:{line}"), message);
    }

    pub fn summary(&mut self, summary: impl Into<String>) {
        self.summary = summary.into();
    }

    pub fn passed(&self) -> bool {
        self.findings.is_empty()
    }

    /// Findings sorted by location, so two runs over one tree print alike.
    pub fn render(&self, gate: &str) -> String {
        let mut out = String::new();
        if self.passed() {
            let _ = writeln!(out, "{gate}: OK — {}", self.summary);
            return out;
        }
        let mut findings: Vec<&Finding> = self.findings.iter().collect();
        findings.sort_by(|a, b| (&a.location, &a.message).cmp(&(&b.location, &b.message)));
        for f in &findings {
            let _ = writeln!(out, "{gate}: {}: {}", f.location, f.message);
        }
        let n = findings.len();
        let _ = writeln!(
            out,
            "{gate}: FAIL — {n} finding{}",
            if n == 1 { "" } else { "s" }
        );
        out
    }
}

/// Print each gate's report; true when every gate passed.
pub fn print_all(results: &[(&str, Report)]) -> bool {
    let mut ok = true;
    for (gate, report) in results {
        print!("{}", report.render(gate));
        ok &= report.passed();
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_with_findings_fails_and_names_each_sorted() {
        let mut r = Report::new();
        r.at("b.rs", 2, "second");
        r.at("a.rs", 9, "first");
        assert!(!r.passed());
        let text = r.render("g");
        assert_eq!(
            text,
            "g: a.rs:9: first\ng: b.rs:2: second\ng: FAIL — 2 findings\n"
        );
    }

    #[test]
    fn a_clean_report_prints_its_summary() {
        let mut r = Report::new();
        r.summary("scanned 3 files");
        assert!(r.passed());
        assert_eq!(r.render("g"), "g: OK — scanned 3 files\n");
    }
}
