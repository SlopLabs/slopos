//! `libc.so`'s interface only ever grows.
//!
//! `libc.so` is the stable interface for programs built outside the base, the
//! role libSystem has on macOS and relibc on Redox. Such a program breaks when
//! a function it calls is removed or renamed, or when a structure it shares
//! with the library changes size or layout, and nothing else in the tree
//! notices, because everything in it is rebuilt against the new headers
//! together.
//!
//! - exports: every defined, non-local dynamic symbol, held to
//!   `scripts/gates/libc/exports.txt` as `<name> func`,
//!   `<name> object|tls <size>`, or `<name> <type>` for any other symbol type.
//!   A data export's size is ABI: a non-PIE executable copy-relocates it, so
//!   growth breaks it as surely as removal.
//! - layouts: the size, alignment and every field offset of each complete
//!   struct/union the headers in `slibc/include` define, held to
//!   `scripts/gates/libc/layouts.txt`. Every record, not only those a
//!   prototype names: exports also take structures through `void *`
//!   (`setsockopt`'s `ip_mreq` and `linger`, `ioctl`'s `winsize`). Measured by
//!   clang's own record-layout dump (`$CLANG`, else `clang` on `PATH`); field
//!   types are left out because their spelling varies across clang releases.
//!
//! Findings: `removed` (a recorded export is gone or changed kind/size),
//! `added` (an export not yet recorded), `layout` (a recorded record changed),
//! `gone` (a recorded record is no longer defined), `new` (a record not yet
//! recorded), `floor` (under 500 exports or 60 records: a parse that read
//! nothing, not a smaller library). An addition is recorded with `--record`,
//! which judges both lists before writing either, only adds lines, and refuses
//! while anything recorded was removed or changed, so a removal can only land
//! as a hand edit a reviewer sees. Both halves always run: a missing library,
//! a clang that cannot be run or a dump that does not parse is an error, never
//! a skip.

mod exports;
mod layouts;

use std::collections::HashMap;
use std::path::Path;

use crate::Result;
use crate::repo::Repo;
use crate::report::{self, Report};

const GATE: &str = "libc-exports";
const USAGE: &str = "usage: xtask libc-exports [--record] LIBC_SO";
const MIN_EXPORTS: usize = 500;
const MIN_RECORDS: usize = 60;
const EXPORTS: &str = "scripts/gates/libc/exports.txt";
const LAYOUTS: &str = "scripts/gates/libc/layouts.txt";
const INCLUDE: &str = "slibc/include";

const EXPORTS_HEADER: &str = "\
# libc.so dynamic symbol table, held by `xtask libc-exports`.
#
# `<name> func`, `<name> object|tls <size>` for data (a data export's size
# is ABI: a non-PIE executable copy-relocates it), or `<name> <type>` for any
# other symbol type. Lines are only ever added, by `xtask libc-exports
# --record`; removing or changing one breaks programs built outside the base.";

const LAYOUTS_HEADER: &str = "\
# Layout of every struct/union slibc/include defines, held by
# `xtask libc-exports`.
#
# `<record> size <n> align <n>`, then `<record> <member.path> <offset>` per
# field in declaration order (bit-fields as `<byte>:<first>-<last>`). An
# anonymous record named by a typedef is `typedef <name>`. Lines are only ever
# added, by `xtask libc-exports --record`; changing one breaks programs built
# outside the base.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Removed,
    Added,
    Layout,
    Gone,
    New,
    Floor,
}

impl Tag {
    const ALL: [Tag; 6] = [
        Tag::Removed,
        Tag::Added,
        Tag::Layout,
        Tag::Gone,
        Tag::New,
        Tag::Floor,
    ];

    fn name(self) -> &'static str {
        match self {
            Tag::Removed => "removed",
            Tag::Added => "added",
            Tag::Layout => "layout",
            Tag::Gone => "gone",
            Tag::New => "new",
            Tag::Floor => "floor",
        }
    }

    fn meaning(self) -> &'static str {
        match self {
            Tag::Removed => {
                "an export was removed or changed kind/size — programs built against it break"
            }
            Tag::Added => "exports are not recorded yet (run `just libc-exports --record`)",
            Tag::Layout => "a header structure changed size, alignment or a field offset",
            Tag::Gone => "a header structure is no longer defined",
            Tag::New => "header structures are not recorded yet (run `just libc-exports --record`)",
            Tag::Floor => "the scan found too little to be believed",
        }
    }

    /// Whether `--record` may write over it: an addition, nothing else.
    fn adds(self) -> bool {
        matches!(self, Tag::Added | Tag::New)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    tag: Tag,
    /// The export or record; the list's name for `floor`.
    key: String,
    detail: String,
}

/// `xtask libc-exports [--record] LIBC_SO`.
pub fn main(repo: &Repo, args: &[String]) -> Result<bool> {
    let mut record = false;
    let mut libc = None;
    for arg in args {
        match arg.as_str() {
            "--record" => record = true,
            a if a.starts_with('-') => return Err(USAGE.into()),
            a => {
                if libc.replace(a).is_some() {
                    return Err(USAGE.into());
                }
            }
        }
    }
    let libc = Path::new(libc.ok_or(USAGE)?);
    let clang = std::env::var("CLANG")
        .ok()
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "clang".to_string());

    let data = std::fs::read(libc).map_err(|e| format!("{}: {e}", libc.display()))?;
    let exports_recorded = recorded(repo, EXPORTS, record)?;
    let layouts_recorded = recorded(repo, LAYOUTS, record)?;
    let exports = exports::extract(&data).map_err(|e| format!("{}: {e}", libc.display()))?;
    let layouts = layouts::measure(&repo.path(INCLUDE), &clang)?;

    let export_findings = scan_exports(&exports_recorded, &exports);
    let layout_findings = scan_layouts(&layouts_recorded, &layouts);
    let summary = format!(
        "{} exports, {} header records",
        line_count(&exports),
        record_count(&layouts)
    );

    if record {
        let blocking: Vec<Finding> = export_findings
            .iter()
            .chain(&layout_findings)
            .filter(|f| !f.tag.adds())
            .cloned()
            .collect();
        if !blocking.is_empty() {
            print(&blocking, &summary);
            eprintln!(
                "{GATE}: --record refuses: it only adds. A removal or change is a hand edit to\n  scripts/gates/libc/, for review to see."
            );
            return Ok(false);
        }
        std::fs::create_dir_all(repo.path("scripts/gates/libc"))?;
        let ok = record_list(
            &repo.path(EXPORTS),
            EXPORTS_HEADER,
            &exports,
            &export_findings,
        )? & record_list(
            &repo.path(LAYOUTS),
            LAYOUTS_HEADER,
            &layouts,
            &layout_findings,
        )?;
        println!(
            "{GATE}: recorded {} new exports, {} new records ({summary})",
            export_findings.len(),
            layout_findings.len()
        );
        return Ok(ok);
    }

    let findings: Vec<Finding> = export_findings.into_iter().chain(layout_findings).collect();
    let ok = print(&findings, &format!("{summary} held to scripts/gates/libc/"));
    if !ok {
        eprintln!(
            "
  libc.so is the interface programs outside the base link against: it only
  grows. Restore what was removed or changed; record an addition with
  `just libc-exports --record`. A deliberate break is a hand edit to
  scripts/gates/libc/ that says why in the commit."
        );
    }
    Ok(ok)
}

/// A tracked list's text; empty when `--record` is about to start it.
fn recorded(repo: &Repo, rel: &str, record: bool) -> Result<String> {
    if !repo.exists(rel) && record {
        return Ok(String::new());
    }
    repo.read(rel)
}

/// The findings on stdout, what each kind means on stderr.
fn print(findings: &[Finding], summary: &str) -> bool {
    let mut report = Report::new();
    report.summary(summary);
    for f in findings {
        report.finding(f.key.as_str(), format!("{} — {}", f.tag.name(), f.detail));
    }
    let ok = report::print_all(&[(GATE, report)]);
    for tag in Tag::ALL {
        if findings.iter().any(|f| f.tag == tag) {
            eprintln!("{GATE}: {}: {}", tag.name(), tag.meaning());
        }
    }
    ok
}

/// A list's data lines: neither a comment nor blank.
fn data_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split_terminator('\n')
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
}

fn line_count(text: &str) -> usize {
    text.split_terminator('\n')
        .filter(|l| !l.is_empty())
        .count()
}

fn key(line: &str, fields: usize) -> String {
    line.split_ascii_whitespace()
        .take(fields)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Distinct records: runs of lines sharing their first two fields.
fn record_count(text: &str) -> usize {
    let mut count = 0;
    let mut last = None;
    for line in text.split_terminator('\n') {
        let k = key(line, 2);
        if last.as_ref() != Some(&k) {
            count += 1;
            last = Some(k);
        }
    }
    count
}

/// Data lines grouped by their first `fields` fields, groups in order of
/// first appearance.
fn groups(text: &str, fields: usize) -> (Vec<String>, HashMap<String, Vec<&str>>) {
    let mut order = Vec::new();
    let mut map: HashMap<String, Vec<&str>> = HashMap::new();
    for line in data_lines(text) {
        let k = key(line, fields);
        map.entry(k.clone())
            .or_insert_with(|| {
                order.push(k);
                Vec::new()
            })
            .push(line);
    }
    (order, map)
}

/// Findings over a recorded list and a current one. A group present in both
/// must match line for line.
fn compare(
    fields: usize,
    [missing, changed, added]: [Tag; 3],
    recorded: &str,
    current: &str,
) -> Vec<Finding> {
    let (rec_order, rec) = groups(recorded, fields);
    let (cur_order, cur) = groups(current, fields);
    let mut findings = Vec::new();
    for k in &rec_order {
        let was = &rec[k];
        match cur.get(k) {
            None => findings.push(Finding {
                tag: missing,
                key: k.clone(),
                detail: "recorded, no longer present".into(),
            }),
            Some(now) if now != was => {
                let at = (0..).find(|&j| was.get(j) != now.get(j)).unwrap_or(0);
                findings.push(Finding {
                    tag: changed,
                    key: k.clone(),
                    detail: format!(
                        "recorded \"{}\", now \"{}\"",
                        was.get(at).unwrap_or(&""),
                        now.get(at).unwrap_or(&"")
                    ),
                });
            }
            Some(_) => {}
        }
    }
    for k in cur_order.into_iter().filter(|k| !rec.contains_key(k)) {
        findings.push(Finding {
            tag: added,
            key: k,
            detail: "not recorded".into(),
        });
    }
    findings
}

fn scan_exports(recorded: &str, current: &str) -> Vec<Finding> {
    let mut findings = compare(
        1,
        [Tag::Removed, Tag::Removed, Tag::Added],
        recorded,
        current,
    );
    let count = line_count(current);
    if count < MIN_EXPORTS {
        findings.push(Finding {
            tag: Tag::Floor,
            key: "exports".into(),
            detail: format!("{count} exports parsed, under the floor of {MIN_EXPORTS}"),
        });
    }
    findings
}

fn scan_layouts(recorded: &str, current: &str) -> Vec<Finding> {
    let mut findings = compare(2, [Tag::Gone, Tag::Layout, Tag::New], recorded, current);
    let count = record_count(current);
    if count < MIN_RECORDS {
        findings.push(Finding {
            tag: Tag::Floor,
            key: "layouts".into(),
            detail: format!("{count} records parsed, under the floor of {MIN_RECORDS}"),
        });
    }
    findings
}

/// Rewrites `path` as its own leading comment block (or `header` for a new
/// file) followed by `current`, which only adds when `findings` carries
/// nothing but additions. Returns false, writing nothing, otherwise.
fn record_list(path: &Path, header: &str, current: &str, findings: &[Finding]) -> Result<bool> {
    let blocking: Vec<&Finding> = findings.iter().filter(|f| !f.tag.adds()).collect();
    if !blocking.is_empty() {
        eprintln!("{GATE}: --record refuses: a recorded line was removed or changed");
        for f in blocking {
            eprintln!("    {}: {}", f.key, f.detail);
        }
        return Ok(false);
    }
    let mut next = String::new();
    match std::fs::read_to_string(path) {
        Ok(old) => {
            for line in old
                .split_terminator('\n')
                .take_while(|l| l.starts_with('#') || l.trim().is_empty())
            {
                next.push_str(line);
                next.push('\n');
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            next.push_str(header);
            next.push_str("\n\n");
        }
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    }
    next.push_str(current);
    let tmp = path.with_extension("txt.next");
    std::fs::write(&tmp, next).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::layouts::tests::{Variant, canonical_of, dump, expected, headers};
    use super::*;
    use crate::fixture::Tree;

    fn count(findings: &[Finding], tag: Tag) -> usize {
        findings.iter().filter(|f| f.tag == tag).count()
    }

    fn lines(lines: &[String]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    /// The recorded list under its header, and the same lines reversed.
    fn export_lists() -> (String, Vec<String>) {
        let mut syms: Vec<String> = (1..=MIN_EXPORTS)
            .map(|i| format!("sym{i:04} func"))
            .collect();
        syms.push("grow object 4".into());
        syms.push("tlsvar tls 8".into());
        syms.sort();
        let recorded = format!("{EXPORTS_HEADER}\n\n{}", lines(&syms));
        syms.reverse();
        (recorded, syms)
    }

    fn edited(current: &[String], from: &str, to: Option<&str>) -> String {
        lines(
            &current
                .iter()
                .filter_map(|l| {
                    if l == from {
                        to.map(str::to_string)
                    } else {
                        Some(l.clone())
                    }
                })
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn an_identical_export_list_reordered_under_a_header_holds() {
        let (rec, cur) = export_lists();
        assert_eq!(scan_exports(&rec, &lines(&cur)), []);
    }

    #[test]
    fn a_recorded_export_missing_is_removed() {
        let (rec, cur) = export_lists();
        let found = scan_exports(&rec, &edited(&cur, "sym0001 func", None));
        assert_eq!(count(&found, Tag::Removed), 1);
        assert_eq!(count(&found, Tag::Added), 0);
    }

    #[test]
    fn an_export_not_yet_recorded_is_added() {
        let (rec, mut cur) = export_lists();
        cur.push("newfn func".into());
        let found = scan_exports(&rec, &lines(&cur));
        assert_eq!(count(&found, Tag::Added), 1);
        assert_eq!(count(&found, Tag::Removed), 0);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_data_export_whose_size_grew_is_removed() {
        let (rec, cur) = export_lists();
        let found = scan_exports(&rec, &edited(&cur, "grow object 4", Some("grow object 8")));
        assert_eq!(count(&found, Tag::Removed), 1);
        assert_eq!(
            found[0].detail,
            "recorded \"grow object 4\", now \"grow object 8\""
        );
    }

    #[test]
    fn a_function_export_that_became_data_is_removed() {
        let (rec, cur) = export_lists();
        let found = scan_exports(
            &rec,
            &edited(&cur, "sym0002 func", Some("sym0002 object 8")),
        );
        assert_eq!(count(&found, Tag::Removed), 1);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn an_export_list_under_the_floor_fails() {
        let (rec, cur) = export_lists();
        let few: Vec<String> = cur
            .into_iter()
            .filter(|l| l.starts_with("sym000"))
            .collect();
        assert_eq!(count(&scan_exports(&rec, &lines(&few)), Tag::Floor), 1);
    }

    #[test]
    fn record_adds_exactly_the_new_export_and_keeps_the_header() {
        let (rec, mut cur) = export_lists();
        let tree = Tree::new(&[("exports.txt", &rec)]);
        let path = tree.repo.path("exports.txt");
        cur.push("newfn func".into());
        cur.sort();
        let cur = lines(&cur);
        let found = scan_exports(&rec, &cur);
        assert!(record_list(&path, EXPORTS_HEADER, &cur, &found).unwrap());
        let now = std::fs::read_to_string(&path).unwrap();
        let mut want: Vec<&str> = rec.lines().collect();
        let at = want
            .iter()
            .position(|l| *l > "newfn func" && !l.starts_with('#') && !l.is_empty())
            .unwrap();
        want.insert(at, "newfn func");
        assert_eq!(now.lines().collect::<Vec<_>>(), want);
        assert_eq!(scan_exports(&now, &cur), []);
    }

    #[test]
    fn record_refuses_a_removal_or_a_change_writing_nothing() {
        let (rec, cur) = export_lists();
        let tree = Tree::new(&[("exports.txt", &rec)]);
        let path = tree.repo.path("exports.txt");
        for (from, to) in [
            ("sym0003 func", None),
            ("grow object 4", Some("grow object 8")),
        ] {
            let cur = edited(&cur, from, to);
            let found = scan_exports(&rec, &cur);
            assert!(!record_list(&path, EXPORTS_HEADER, &cur, &found).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), rec);
        }
    }

    #[test]
    fn record_starts_a_new_list_under_its_header() {
        let tree = Tree::new(&[]);
        let path = tree.repo.path("exports.txt");
        let cur = "a func\n";
        let found = scan_exports("", cur);
        assert!(!record_list(&path, EXPORTS_HEADER, cur, &found).unwrap());
        assert!(record_list(&path, EXPORTS_HEADER, cur, &[]).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{EXPORTS_HEADER}\n\na func\n")
        );
    }

    /// The recorded layouts of the base fixture, under the header.
    fn layout_fixture() -> (Tree, String) {
        let tree = Tree::new(&[]);
        headers(&tree, "inc", 0, false);
        headers(&tree, "moved", 3, false);
        headers(&tree, "grown", 0, true);
        let rec = format!("{LAYOUTS_HEADER}\n\n{}", expected());
        (tree, rec)
    }

    fn current(tree: &Tree, dir: &str, pad: usize, variant: Variant) -> String {
        canonical_of(tree, dir, &dump(tree, dir, pad, variant)).unwrap()
    }

    #[test]
    fn headers_whose_line_numbers_moved_hold() {
        let (tree, rec) = layout_fixture();
        assert_eq!(
            scan_layouts(&rec, &current(&tree, "moved", 3, Variant::Base)),
            []
        );
    }

    #[test]
    fn a_record_whose_size_or_offset_changed_is_one_layout_finding() {
        let (tree, rec) = layout_fixture();
        for variant in [Variant::Size, Variant::Offset] {
            let found = scan_layouts(&rec, &current(&tree, "inc", 0, variant));
            assert_eq!(count(&found, Tag::Layout), 1);
            assert_eq!(found.len(), 1);
        }
    }

    #[test]
    fn a_recorded_record_no_longer_defined_is_gone() {
        let (tree, rec) = layout_fixture();
        let found = scan_layouts(&rec, &current(&tree, "inc", 0, Variant::Gone));
        assert_eq!(count(&found, Tag::Gone), 1);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_record_not_yet_recorded_is_new() {
        let (tree, rec) = layout_fixture();
        let found = scan_layouts(&rec, &current(&tree, "grown", 0, Variant::New));
        assert_eq!(count(&found, Tag::New), 1);
        assert_eq!(count(&found, Tag::Layout), 0);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_layout_list_under_the_floor_fails() {
        let (tree, rec) = layout_fixture();
        let few: String = current(&tree, "inc", 0, Variant::Base)
            .lines()
            .filter(|l| !l.starts_with("struct fill_"))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_eq!(count(&scan_layouts(&rec, &few), Tag::Floor), 1);
    }

    #[test]
    fn record_adds_exactly_the_new_records_lines_and_refuses_a_change() {
        let (tree, rec) = layout_fixture();
        tree.write("layouts.txt", &rec);
        let path = tree.repo.path("layouts.txt");
        for variant in [Variant::Offset, Variant::Gone] {
            let cur = current(&tree, "inc", 0, variant);
            let found = scan_layouts(&rec, &cur);
            assert!(!record_list(&path, LAYOUTS_HEADER, &cur, &found).unwrap());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), rec);
        }
        let new = current(&tree, "grown", 0, Variant::New);
        let found = scan_layouts(&rec, &new);
        assert!(record_list(&path, LAYOUTS_HEADER, &new, &found).unwrap());
        let now = std::fs::read_to_string(&path).unwrap();
        let mut added: Vec<&str> = now.lines().collect();
        for line in rec.lines() {
            let at = added.iter().position(|l| *l == line).unwrap();
            added.remove(at);
        }
        assert_eq!(added, ["struct extra size 4 align 4", "struct extra e 0"]);
        assert_eq!(scan_layouts(&now, &new), []);
    }

    #[test]
    fn the_command_refuses_a_bad_command_line_or_a_missing_library() {
        let tree = Tree::new(&[]);
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(main(&tree.repo, &args(&[])).is_err());
        assert!(main(&tree.repo, &args(&["--bogus", "a.so"])).is_err());
        assert!(main(&tree.repo, &args(&["a.so", "b.so"])).is_err());
        let missing = tree.repo.path("absent.so").display().to_string();
        let err = main(&tree.repo, &args(&[&missing]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absent.so"), "{err}");
    }

    #[test]
    fn the_tracked_headers_match_what_record_writes_into_a_new_list() {
        let repo = Repo::at(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
        for (rel, header) in [(EXPORTS, EXPORTS_HEADER), (LAYOUTS, LAYOUTS_HEADER)] {
            let text = repo.read(rel).unwrap();
            assert!(text.starts_with(&format!("{header}\n\n")), "{rel}");
        }
    }
}
