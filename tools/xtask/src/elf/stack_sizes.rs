//! No function's stack frame exceeds `STACK_SIZE_THRESHOLD` bytes (default
//! 2048, Linux's `CONFIG_FRAME_WARN` default on x86_64, enforced as a hard
//! failure). This is SlopOS invariant S-5: an OSTD client's frame cannot grow
//! large enough to puncture a stack guard page in one function entry. The
//! linked ELF's `.stack_sizes` is measured rather than a compile-time
//! heuristic, so inlining, NRVO failures and trait-object dispatch all count.
//!
//! Above the threshold sits a limit no allowlist raises: the 4096 B guard page
//! below each stack (`KSTACK_GUARD_SIZE`, `USTACK_GUARD_SIZE` in
//! `mm/src/memory_layout_defs.rs`). The target builds without stack probes,
//! so a larger frame steps clean over its guard in one instruction.
//!
//! What `.stack_sizes` records under `-Z sanitizer=safestack` is not
//! established: a split frame has two allocations and one number. The 2 KiB
//! cap against a 4 KiB guard is the margin that absorbs that uncertainty.
//!
//! Each variant has its own allowlist, `scripts/gates/stack/<variant>.txt`:
//! codegen differs, and a union would hide a regression in whichever is
//! looser. Entries are `<cap>\t<glob over the mangled symbol>`; an entry that
//! matches no over-threshold frame is dead and fails. The file also carries
//! the input floors, so weakening one is a diff on a tracked file:
//! `min-records N` stops a dropped `-Zemit-stack-sizes` from reading as a
//! kernel with no large frames, and `expect-test-registry few|many|any`
//! stops a variant being graded against the wrong image.

use object::elf::{STT_FUNC, STT_GNU_IFUNC};
use object::{Object, ObjectSection, ObjectSymbol};

use super::{Ctx, Image};
use crate::Result;
use crate::glob;
use crate::report::Report;

const DEFAULT_THRESHOLD: u64 = 2048;

/// A property of the memory layout, not a policy knob.
const GUARD_PAGE: u64 = 4096;

/// `size_of` a `.test_registry` entry.
const TEST_REGISTRY_ENTRY_SIZE: u64 = 104;

/// A kernel/tests image registers thousands of tests; dev and release a
/// handful.
const TEST_REGISTRY_MANY: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TestRegistry {
    Few,
    Many,
    Any,
}

struct Entry {
    cap: u64,
    glob: String,
    line: usize,
}

struct Allowlist {
    path: String,
    entries: Vec<Entry>,
    min_records: u64,
    test_registry: TestRegistry,
}

/// One `.stack_sizes` record under one of the names its address carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    size: u64,
    name: String,
}

pub fn run(ctx: &Ctx) -> Result<Report> {
    let threshold = threshold()?;
    let allowlist = Allowlist::load(ctx)?;
    check_test_registry(ctx, &allowlist)?;
    let (records, frames) = over_threshold(ctx.image, threshold)?;
    check_min_records(ctx, &allowlist, records)?;

    let mut report = Report::new();
    let mut hits = vec![0usize; allowlist.entries.len()];
    let (mut allowed, mut allowed_bytes, mut largest_cap) = (0usize, 0u64, 0u64);
    for frame in &frames {
        if frame.size > GUARD_PAGE {
            report.finding(
                &frame.name,
                format!(
                    "{} B frame is above the {GUARD_PAGE} B stack guard page — the target has no stack probes, so this frame steps over the guard in one instruction; no allowlist entry can cover it, move the large locals off the stack",
                    frame.size
                ),
            );
            continue;
        }
        let matched = allowlist
            .entries
            .iter()
            .position(|e| glob::matches(&e.glob, &frame.name));
        if let Some(i) = matched {
            hits[i] += 1;
            let cap = allowlist.entries[i].cap;
            if frame.size <= cap {
                allowed += 1;
                allowed_bytes += frame.size;
                largest_cap = largest_cap.max(cap);
                continue;
            }
            report.finding(
                &frame.name,
                format!(
                    "{} B frame exceeds its {cap} B cap at {}:{} — shrink the frame; raise the cap only with a measured reason",
                    frame.size, allowlist.path, allowlist.entries[i].line
                ),
            );
            continue;
        }
        report.finding(
            &frame.name,
            format!(
                "{} B frame exceeds STACK_SIZE_THRESHOLD={threshold} — move large locals off the stack, or allowlist a known frame with its measured cap in {}",
                frame.size, allowlist.path
            ),
        );
    }
    for (entry, &n) in allowlist.entries.iter().zip(&hits) {
        if n == 0 {
            report.at(
                &allowlist.path,
                entry.line,
                format!(
                    "allowlist entry `{}` matches no over-threshold frame — the frame shrank below {threshold} B or the symbol was renamed; delete the dead exemption",
                    entry.glob
                ),
            );
        }
    }

    let n = allowlist.entries.len();
    report.summary(format!(
        "variant={}, {records} records, {allowed} frame(s) over {threshold} B covered by {n}/{n} allowlist entries ({allowed_bytes} B allowlisted, largest cap {largest_cap} B, guard page {GUARD_PAGE} B)",
        ctx.variant
    ));
    Ok(report)
}

/// The variant's allowlist as measured from this image: every over-threshold
/// frame at its current size. It records what each frame is, not that it is
/// acceptable, so every line is reviewed before it is committed.
pub fn emit_allowlist(ctx: &Ctx) -> Result<String> {
    let threshold = threshold()?;
    let allowlist = Allowlist::load(ctx)?;
    check_test_registry(ctx, &allowlist)?;
    let (records, mut frames) = over_threshold(ctx.image, threshold)?;
    check_min_records(ctx, &allowlist, records)?;
    frames.sort_by(|a, b| (b.size, &b.name).cmp(&(a.size, &a.name)));

    let mut out = format!(
        "# stack-sizes allowlist — variant: {}\n#\n\
         # Emitted from {} ({records} records). Review every line before committing:\n\
         # an emitted entry records what the frame *is*, not that it is acceptable.\n\n\
         min-records {}\n\
         expect-test-registry few\n\n",
        ctx.variant,
        ctx.path.display(),
        records / 2
    );
    for frame in &frames {
        out.push_str(&format!("{}\t*{}*\n", frame.size, frame.name));
    }
    Ok(out)
}

fn threshold() -> Result<u64> {
    match std::env::var("STACK_SIZE_THRESHOLD") {
        Ok(v) => v
            .parse()
            .map_err(|_| format!("STACK_SIZE_THRESHOLD=`{v}` is not a byte count").into()),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_THRESHOLD),
        Err(e) => Err(format!("STACK_SIZE_THRESHOLD: {e}").into()),
    }
}

impl Allowlist {
    fn load(ctx: &Ctx) -> Result<Self> {
        let path = format!("scripts/gates/stack/{}.txt", ctx.variant);
        if !ctx.repo.exists(&path) {
            return Err(format!(
                "no allowlist for variant `{}` at {path}: every gated variant needs its own measured allowlist (emit one with --emit-allowlist)",
                ctx.variant
            )
            .into());
        }
        Self::parse(&path, &ctx.repo.read(&path)?)
    }

    fn parse(path: &str, text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        let (mut min_records, mut test_registry) = (None, None);
        for (i, line) in text.lines().enumerate() {
            let lineno = i + 1;
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((cap, glob)) = line.split_once('\t') {
                let cap = cap
                    .parse()
                    .map_err(|_| format!("{path}:{lineno}: cap `{cap}` is not a byte count"))?;
                entries.push(Entry {
                    cap,
                    glob: glob.to_string(),
                    line: lineno,
                });
                continue;
            }
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "min-records" => {
                    min_records = Some(value.parse().map_err(|_| {
                        format!("{path}:{lineno}: min-records `{value}` is not a count")
                    })?)
                }
                "expect-test-registry" => {
                    test_registry = Some(match value {
                        "few" => TestRegistry::Few,
                        "many" => TestRegistry::Many,
                        "any" => TestRegistry::Any,
                        _ => {
                            return Err(format!(
                                "{path}:{lineno}: expect-test-registry must be few|many|any"
                            )
                            .into());
                        }
                    })
                }
                _ => return Err(format!("{path}:{lineno}: unknown directive `{key}`").into()),
            }
        }
        match (min_records, test_registry) {
            (Some(min_records), Some(test_registry)) => Ok(Self {
                path: path.to_string(),
                entries,
                min_records,
                test_registry,
            }),
            _ => Err(format!("{path} must set min-records and expect-test-registry").into()),
        }
    }
}

/// Catches a `--variant` stated as tests against a non-tests kernel and the
/// reverse; dev against release is min-records' and the dead-entry rule's.
fn check_test_registry(ctx: &Ctx, allowlist: &Allowlist) -> Result<()> {
    if allowlist.test_registry == TestRegistry::Any {
        return Ok(());
    }
    let Some(section) = ctx.image.file.section_by_name(".test_registry") else {
        return Err(format!(
            "{} has no .test_registry section — not a SlopOS kernel image (link.ld brackets that section unconditionally)",
            ctx.path.display()
        )
        .into());
    };
    let entries = section.size() / TEST_REGISTRY_ENTRY_SIZE;
    match allowlist.test_registry {
        TestRegistry::Many if entries < TEST_REGISTRY_MANY => Err(format!(
            "--variant {} expects a kernel/tests image, but {} registers only {entries} test(s): this is the wrong binary",
            ctx.variant,
            ctx.path.display()
        )
        .into()),
        TestRegistry::Few if entries >= TEST_REGISTRY_MANY => Err(format!(
            "--variant {} expects a non-tests image, but {} registers {entries} tests: that is the kernel/tests build, gate kernel-tests.elf as tests",
            ctx.variant,
            ctx.path.display()
        )
        .into()),
        _ => Ok(()),
    }
}

fn check_min_records(ctx: &Ctx, allowlist: &Allowlist, records: u64) -> Result<()> {
    if records < allowlist.min_records {
        return Err(format!(
            "only {records} .stack_sizes record(s) in {} (min-records {} in {}) — refusing to report OK: either -Zemit-stack-sizes stopped reaching the build (see scripts/build_kernel.sh) or this is not the {} kernel",
            ctx.path.display(),
            allowlist.min_records,
            allowlist.path,
            ctx.variant
        )
        .into());
    }
    Ok(())
}

/// The record count, and every record over `threshold` under each function
/// name its address carries (`?` when none does).
fn over_threshold(image: &Image, threshold: u64) -> Result<(u64, Vec<Frame>)> {
    let mut functions: Vec<(u64, &str)> = Vec::new();
    for sym in image.file.symbols() {
        let st_type = sym.elf_symbol().st_type();
        if (st_type == STT_FUNC || st_type == STT_GNU_IFUNC) && sym.is_definition() {
            functions.push((sym.address(), sym.name()?));
        }
    }
    functions.sort_unstable();

    let mut records = 0;
    let mut frames = Vec::new();
    for section in image.file.sections() {
        if section.name()? != ".stack_sizes" {
            continue;
        }
        for (address, size) in parse_records(section.data()?)? {
            records += 1;
            if size <= threshold {
                continue;
            }
            let start = functions.partition_point(|&(a, _)| a < address);
            let names: Vec<&str> = functions[start..]
                .iter()
                .take_while(|&&(a, _)| a == address)
                .map(|&(_, n)| n)
                .collect();
            if names.is_empty() {
                frames.push(Frame {
                    size,
                    name: "?".to_string(),
                });
            }
            frames.extend(names.into_iter().map(|name| Frame {
                size,
                name: name.to_string(),
            }));
        }
    }
    Ok((records, frames))
}

/// `.stack_sizes` is a run of (u64 function address, ULEB128 frame size).
fn parse_records(mut data: &[u8]) -> Result<Vec<(u64, u64)>> {
    let mut records = Vec::new();
    while !data.is_empty() {
        let (address, rest) = data
            .split_first_chunk::<8>()
            .ok_or(".stack_sizes ends inside a record's address")?;
        let address = u64::from_le_bytes(*address);
        let (mut size, mut shift, mut used) = (0u64, 0u32, 0usize);
        loop {
            let &byte = rest
                .get(used)
                .ok_or(".stack_sizes ends inside a record's size")?;
            used += 1;
            if shift >= 64 {
                return Err(".stack_sizes holds a size wider than 64 bits".into());
            }
            size |= u64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        records.push((address, size));
        data = &rest[used..];
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use object::write::{Object as Builder, StandardSection, Symbol, SymbolSection};
    use object::{
        Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolKind, SymbolScope,
    };

    use super::*;
    use crate::fixture::Tree;

    /// A relocatable x86_64 object: `functions` laid out back to back in
    /// `.text`, `stack_sizes` (when given) recording each function's frame
    /// at the address its symbol holds, and any extra raw sections.
    fn object(functions: &[(&str, u64)], stack_sizes: bool, extra: &[(&str, usize)]) -> Vec<u8> {
        let mut obj = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        let mut records = Vec::new();
        for (name, frame) in functions {
            let offset = obj.append_section_data(text, &[0xc3; 16], 16);
            obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: offset,
                size: 16,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
            records.extend_from_slice(&offset.to_le_bytes());
            let mut v = *frame;
            loop {
                let byte = (v & 0x7f) as u8;
                v >>= 7;
                if v == 0 {
                    records.push(byte);
                    break;
                }
                records.push(byte | 0x80);
            }
        }
        if stack_sizes {
            let s = obj.add_section(Vec::new(), b".stack_sizes".to_vec(), SectionKind::Other);
            obj.append_section_data(s, &records, 1);
        }
        for (name, len) in extra {
            let s = obj.add_section(Vec::new(), name.as_bytes().to_vec(), SectionKind::Data);
            obj.append_section_data(s, &vec![0; *len], 1);
        }
        obj.write().unwrap()
    }

    /// The self-test's `probe.ll`: 3000 and 2200 sit between the threshold
    /// and the guard page, 24 below the threshold.
    fn probe(stack_sizes: bool) -> Vec<u8> {
        object(
            &[
                ("mid_frame", 3000),
                ("low_frame", 2200),
                ("small_frame", 24),
            ],
            stack_sizes,
            &[],
        )
    }

    fn gate(elf: &[u8], allowlist: &str, variant: &str) -> (Tree, Result<Report>) {
        let tree = Tree::new(&[(&format!("scripts/gates/stack/{variant}.txt"), allowlist)]);
        let image = Image::parse(elf).unwrap();
        let ctx = Ctx {
            repo: &tree.repo,
            path: Path::new("fixture.o"),
            variant,
            image: &image,
        };
        let result = run(&ctx);
        (tree, result)
    }

    fn selftest(elf: &[u8], entries: &[&str]) -> Result<Report> {
        let mut text = String::from("min-records 1\nexpect-test-registry any\n");
        for e in entries {
            text.push_str(e);
            text.push('\n');
        }
        gate(elf, &text, "selftest").1
    }

    fn rendered(report: &Report) -> String {
        report.render("stack-sizes")
    }

    #[test]
    fn an_over_threshold_frame_is_rejected_and_a_small_one_is_silent() {
        let r = selftest(&probe(true), &[]).unwrap();
        assert!(!r.passed());
        assert_eq!(r.findings.len(), 2);
        let text = rendered(&r);
        assert!(text.contains("mid_frame: 3000 B frame exceeds STACK_SIZE_THRESHOLD=2048"));
        assert!(text.contains("low_frame"));
        assert!(!text.contains("small_frame"));
    }

    #[test]
    fn measured_caps_are_accepted() {
        let r = selftest(&probe(true), &["3000\t*mid_frame*", "2200\t*low_frame*"]).unwrap();
        assert!(r.passed(), "{}", rendered(&r));
        assert_eq!(
            r.summary,
            "variant=selftest, 3 records, 2 frame(s) over 2048 B covered by 2/2 allowlist entries (5200 B allowlisted, largest cap 3000 B, guard page 4096 B)"
        );
    }

    #[test]
    fn a_cap_one_byte_short_still_fails() {
        let r = selftest(&probe(true), &["2999\t*mid_frame*", "2200\t*low_frame*"]).unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].location, "mid_frame");
        assert!(r.findings[0].message.contains("exceeds its 2999 B cap"));
    }

    #[test]
    fn the_first_matching_entry_decides() {
        let r = selftest(&probe(true), &["2100\t*_frame", "3000\t*mid_frame*"]).unwrap();
        let locations: Vec<&str> = r.findings.iter().map(|f| f.location.as_str()).collect();
        assert!(locations.contains(&"mid_frame"));
        assert!(locations.contains(&"low_frame"));
        assert!(locations.contains(&"scripts/gates/stack/selftest.txt:4"));
    }

    #[test]
    fn a_guard_page_frame_is_not_allowlistable() {
        let guard = object(&[("guard_jumper", 4104)], true, &[]);
        let r = selftest(&guard, &["9999\t*guard_jumper*"]).unwrap();
        let text = rendered(&r);
        assert!(text.contains("guard_jumper: 4104 B frame is above the 4096 B stack guard page"));
        assert!(text.contains("selftest.txt:3: allowlist entry `*guard_jumper*` matches no"));
    }

    #[test]
    fn a_frame_of_exactly_the_guard_page_is_capped_not_refused() {
        let edge = object(&[("edge", 4096)], true, &[]);
        assert!(selftest(&edge, &["4096\tedge"]).unwrap().passed());
    }

    #[test]
    fn a_dead_entry_is_rejected_and_named_by_line() {
        let r = selftest(
            &probe(true),
            &[
                "3000\t*mid_frame*",
                "2200\t*low_frame*",
                "9999\t*no_such_symbol*",
            ],
        )
        .unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].location, "scripts/gates/stack/selftest.txt:5");
        assert!(r.findings[0].message.contains("*no_such_symbol*"));
    }

    #[test]
    fn the_record_floor_is_enforced() {
        let (_t, r) = gate(
            &probe(true),
            "min-records 99999\nexpect-test-registry any\n",
            "selftest",
        );
        let e = r.unwrap_err().to_string();
        assert!(e.contains("only 3 .stack_sizes record(s)"), "{e}");
        assert!(e.contains("refusing to report OK"));
    }

    #[test]
    fn an_elf_with_no_stack_sizes_is_rejected() {
        let e = selftest(&probe(false), &[]).unwrap_err().to_string();
        assert!(e.contains("only 0 .stack_sizes record(s)"), "{e}");
        assert!(e.contains("refusing to report OK"));
    }

    #[test]
    fn the_test_registry_must_match_the_variant() {
        let few = object(&[("f", 16)], true, &[(".test_registry", 104 * 3)]);
        let many = object(&[("f", 16)], true, &[(".test_registry", 104 * 100)]);
        let none = object(&[("f", 16)], true, &[]);
        let floors = |r: &str| format!("min-records 1\nexpect-test-registry {r}\n");
        assert!(gate(&few, &floors("few"), "dev").1.unwrap().passed());
        assert!(gate(&many, &floors("many"), "tests").1.unwrap().passed());
        let e = gate(&many, &floors("few"), "dev")
            .1
            .unwrap_err()
            .to_string();
        assert!(e.contains("registers 100 tests"), "{e}");
        let e = gate(&few, &floors("many"), "tests")
            .1
            .unwrap_err()
            .to_string();
        assert!(e.contains("registers only 3 test(s)"), "{e}");
        let e = gate(&none, &floors("few"), "dev")
            .1
            .unwrap_err()
            .to_string();
        assert!(e.contains("no .test_registry section"), "{e}");
    }

    #[test]
    fn a_bad_allowlist_is_an_error() {
        for (text, want) in [
            (
                "min-records 1\n",
                "must set min-records and expect-test-registry",
            ),
            ("min-records 1\nexpect-test-registry some\n", "few|many|any"),
            (
                "min-records 1\nexpect-test-registry any\nmax-records 9\n",
                ":3: unknown directive `max-records`",
            ),
            (
                "min-records 1\nexpect-test-registry any\nbig\t*f*\n",
                ":3: cap `big`",
            ),
        ] {
            let e = gate(&probe(true), text, "selftest")
                .1
                .unwrap_err()
                .to_string();
            assert!(e.contains(want), "{text:?}: {e}");
        }
    }

    #[test]
    fn a_missing_allowlist_is_an_error() {
        let tree = Tree::new(&[]);
        let elf = probe(true);
        let image = Image::parse(&elf).unwrap();
        let ctx = Ctx {
            repo: &tree.repo,
            path: Path::new("fixture.o"),
            variant: "nosuch",
            image: &image,
        };
        let e = run(&ctx).unwrap_err().to_string();
        assert!(e.contains("no allowlist for variant `nosuch`"), "{e}");
    }

    #[test]
    fn aliases_each_answer_for_the_frame() {
        let mut obj = Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let text = obj.section_id(StandardSection::Text);
        obj.append_section_data(text, &[0xc3; 16], 16);
        for name in ["alias_a", "alias_b"] {
            obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: 0,
                size: 16,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(text),
                flags: SymbolFlags::None,
            });
        }
        let s = obj.add_section(Vec::new(), b".stack_sizes".to_vec(), SectionKind::Other);
        let mut rec = 0u64.to_le_bytes().to_vec();
        rec.extend_from_slice(&[0xb8, 0x17]);
        obj.append_section_data(s, &rec, 1);
        let r = selftest(&obj.write().unwrap(), &["3000\talias_a"]).unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].location, "alias_b");
        assert!(r.findings[0].message.starts_with("3000 B frame"));
    }

    #[test]
    fn records_parse_as_address_and_uleb128() {
        let mut data = 0x1000u64.to_le_bytes().to_vec();
        data.extend_from_slice(&[0xe5, 0x8e, 0x26]);
        data.extend_from_slice(&0x2000u64.to_le_bytes());
        data.push(0x10);
        assert_eq!(
            parse_records(&data).unwrap(),
            vec![(0x1000, 624_485), (0x2000, 16)]
        );
        assert!(parse_records(&data[..5]).is_err());
        assert!(parse_records(&data[..9]).is_err());
    }

    #[test]
    fn an_emitted_allowlist_round_trips() {
        let elf = object(
            &[
                ("mid_frame", 3000),
                ("low_frame", 2200),
                ("tie_b", 2200),
                ("small_frame", 24),
            ],
            true,
            &[(".test_registry", 104)],
        );
        let (tree, _) = gate(&elf, "min-records 1\nexpect-test-registry few\n", "dev");
        let image = Image::parse(&elf).unwrap();
        let ctx = Ctx {
            repo: &tree.repo,
            path: Path::new("kernel-dev.elf"),
            variant: "dev",
            image: &image,
        };
        let emitted = emit_allowlist(&ctx).unwrap();
        assert_eq!(
            emitted,
            "# stack-sizes allowlist — variant: dev\n#\n\
             # Emitted from kernel-dev.elf (4 records). Review every line before committing:\n\
             # an emitted entry records what the frame *is*, not that it is acceptable.\n\n\
             min-records 2\nexpect-test-registry few\n\n\
             3000\t*mid_frame*\n2200\t*tie_b*\n2200\t*low_frame*\n"
        );
        tree.write("scripts/gates/stack/dev.txt", &emitted);
        let r = run(&ctx).unwrap();
        assert!(r.passed(), "{}", rendered(&r));
    }
}
