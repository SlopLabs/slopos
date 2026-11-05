//! The trusted computing base stays small: lines of `unsafe` in slopos-ostd
//! and the named TCB annexes (the vendored unwinder and gimli), over the Rust
//! lines of every kernel crate plus the annexes, may not exceed 1.0 %. The
//! kernel crates are the `kernel` binary's closure over normal dependencies,
//! so the denominator is the code in the image and nothing else.
//!
//! The counts are line counts. An unsafe line is a line that holds the word
//! `unsafe` and does not begin (after whitespace) with `//` or `/*`; a code
//! line is one that is neither blank nor begins so. Every `.rs` file under a
//! crate's `src/` counts, as the filesystem has it.

use std::path::Path;

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::workspace::Workspace;

const MAX_PCT: f64 = 1.0;
const OSTD: &str = "slopos-ostd";
const ANNEXES: &[&str] = &["vendor/unwinding", "vendor/gimli"];

pub fn run(repo: &Repo) -> Result<Report> {
    let ws = Workspace::load(repo)?;
    let crates: Vec<String> = ws.kernel_crates()?.iter().map(|p| p.dir.clone()).collect();
    if crates.is_empty() {
        return Err("the kernel closure holds no crates, so there is nothing to measure".into());
    }
    Ok(Tally::measure(repo, &crates)?.report())
}

#[derive(Debug, PartialEq)]
struct Tally {
    ostd_unsafe: usize,
    annex_unsafe: Vec<usize>,
    crates: usize,
    crate_loc: usize,
    annex_loc: Vec<usize>,
}

#[derive(Default)]
struct Counts {
    unsafe_lines: usize,
    loc: usize,
}

impl Tally {
    fn measure(repo: &Repo, crates: &[String]) -> Result<Self> {
        let ostd_unsafe = count(&repo.path(&format!("{OSTD}/src")))?.unsafe_lines;
        let mut annex_unsafe = Vec::new();
        let mut annex_loc = Vec::new();
        for annex in ANNEXES {
            let c = count(&repo.path(&format!("{annex}/src")))?;
            annex_unsafe.push(c.unsafe_lines);
            annex_loc.push(c.loc);
        }
        let mut crate_loc = 0;
        let mut counted = 0;
        for dir in crates {
            if ANNEXES.contains(&dir.as_str()) {
                continue;
            }
            let src = repo.path(&format!("{dir}/src"));
            if !src.is_dir() {
                continue;
            }
            counted += 1;
            crate_loc += count(&src)?.loc;
        }
        let tally = Self {
            ostd_unsafe,
            annex_unsafe,
            crates: counted,
            crate_loc,
            annex_loc,
        };
        if tally.loc() == 0 {
            return Err("no kernel Rust lines found, so there is no ratio to hold".into());
        }
        Ok(tally)
    }

    fn unsafe_lines(&self) -> usize {
        self.ostd_unsafe + self.annex_unsafe.iter().sum::<usize>()
    }

    fn loc(&self) -> usize {
        self.crate_loc + self.annex_loc.iter().sum::<usize>()
    }

    /// The ratio in percent, to the three places it is reported and held at.
    fn ratio(&self) -> String {
        format!(
            "{:.3}",
            self.unsafe_lines() as f64 * 100.0 / self.loc() as f64
        )
    }

    fn describe(&self) -> String {
        let annex = |counts: &[usize]| -> String {
            ANNEXES
                .iter()
                .zip(counts)
                .map(|(a, n)| format!(", {a} {n}"))
                .collect()
        };
        format!(
            "TCB ratio {} %: {} unsafe lines ({OSTD} {}{}) over {} kernel Rust lines \
             ({} in {} kernel crates{})",
            self.ratio(),
            self.unsafe_lines(),
            self.ostd_unsafe,
            annex(&self.annex_unsafe),
            self.loc(),
            self.crate_loc,
            self.crates,
            annex(&self.annex_loc),
        )
    }

    fn report(&self) -> Report {
        let mut report = Report::new();
        let ratio = self.ratio();
        if ratio.parse::<f64>().unwrap_or(f64::INFINITY) > MAX_PCT {
            report.finding(
                OSTD,
                format!(
                    "{} exceeds {MAX_PCT:.1} % — the TCB grows only by shrinking: move unsafe \
                     behind an existing slopos-ostd abstraction rather than adding a new one",
                    self.describe()
                ),
            );
        }
        report.summary(format!("{}, within {MAX_PCT:.1} %", self.describe()));
        report
    }
}

/// Unsafe and code lines over every `.rs` file under `dir`; a missing
/// directory counts nothing.
fn count(dir: &Path) -> Result<Counts> {
    let mut counts = Counts::default();
    if !dir.is_dir() {
        return Ok(counts);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let path = entry.path();
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && path.extension().is_some_and(|e| e == "rs") {
                let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                for line in bytes.split(|&b| b == b'\n') {
                    let rest = trim_start(line);
                    if rest.starts_with(b"//") || rest.starts_with(b"/*") {
                        continue;
                    }
                    if !rest.is_empty() {
                        counts.loc += 1;
                    }
                    if has_word(line, b"unsafe") {
                        counts.unsafe_lines += 1;
                    }
                }
            }
        }
    }
    Ok(counts)
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c)
}

fn trim_start(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|&b| !is_space(b))
        .unwrap_or(line.len());
    &line[start..]
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn has_word(line: &[u8], word: &[u8]) -> bool {
    (0..line.len().saturating_sub(word.len() - 1)).any(|i| {
        line[i..].starts_with(word)
            && (i == 0 || !is_word(line[i - 1]))
            && line.get(i + word.len()).is_none_or(|&b| !is_word(b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const OSTD_LIB: &str = "\
// unsafe in a line comment
/// unsafe in a doc comment
    /* unsafe in a block comment's first line
 * unsafe on a block comment's later line counts, as the line count has it
 */
pub unsafe fn a() {}
unsafe { unsafe {} }
fn not_unsafe_op() {}
fn b() { unsafe_op(); }
const S: &str = \"unsafe\";
\tunsafe impl Send for X {}\r

";

    fn tree() -> Tree {
        Tree::new(&[
            ("slopos-ostd/src/lib.rs", OSTD_LIB),
            ("slopos-ostd/src/nested/mod.rs", "unsafe fn c() {}\n"),
            ("slopos-ostd/build.rs", "unsafe fn outside_src() {}\n"),
            ("mm/src/lib.rs", "fn m() {}\n\n// note\nfn n() {}"),
            ("mm/src/notes.txt", "unsafe\n"),
            (
                "vendor/unwinding/src/lib.rs",
                "unsafe fn u() {}\nfn v() {}\n",
            ),
            ("vendor/gimli/src/lib.rs", "fn g() {}\n"),
            ("nosrc/lib.rs", "fn x() {}\n"),
        ])
    }

    #[test]
    fn lines_are_counted_as_the_ratio_defines_them() {
        let tree = tree();
        let crates = ["mm", "nosrc", "slopos-ostd", "vendor/unwinding"].map(String::from);
        let tally = Tally::measure(&tree.repo, &crates).unwrap();
        assert_eq!(
            tally,
            Tally {
                ostd_unsafe: 6,
                annex_unsafe: vec![1, 0],
                crates: 2,
                crate_loc: 2 + 9,
                annex_loc: vec![2, 1],
            }
        );
        assert_eq!(tally.ratio(), format!("{:.3}", 7.0 * 100.0 / 14.0));
        let report = tally.report();
        assert!(!report.passed());
        assert!(report.findings[0].message.contains("exceeds 1.0 %"));
    }

    #[test]
    fn a_small_tcb_passes_and_says_what_it_measured() {
        let tree = tree();
        tree.write("mm/src/big.rs", &"fn f() {}\n".repeat(2000));
        let tally = Tally::measure(&tree.repo, &["mm".into(), "slopos-ostd".into()]).unwrap();
        let report = tally.report();
        assert!(report.passed(), "{:?}", report.findings);
        assert!(
            report
                .summary
                .starts_with(&format!("TCB ratio {} %", tally.ratio())),
            "{}",
            report.summary
        );
        assert!(
            report.summary.contains("7 unsafe lines"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn nothing_to_measure_is_an_error() {
        let tree = Tree::new(&[("x/src/lib.rs", "")]);
        assert!(Tally::measure(&tree.repo, &["x".into()]).is_err());
    }

    #[test]
    fn unsafe_is_matched_as_a_word() {
        assert!(has_word(b"unsafe", b"unsafe"));
        assert!(has_word(b"x(unsafe)", b"unsafe"));
        assert!(!has_word(b"unsafe_op", b"unsafe"));
        assert!(!has_word(b"is_unsafe", b"unsafe"));
        assert!(!has_word(b"unsaf", b"unsafe"));
    }
}
