//! The kernel must not touch XCR0-managed register state outside the
//! sanctioned save/restore.
//!
//! A syscall or an exception taken from userland (page fault, IRQ) does not
//! save the interrupted task's FPU/vector state; only a full context switch
//! does, with xsave/xrstor in the scheduler. Kernel code that disturbs that
//! state on such a path clobbers the user task's live registers, and the
//! restarted user instruction reads garbage: a userland AVX memset that
//! demand-faults mid-fill and resumes with stale zeros.
//!
//! The state at risk is everything XCR0 enumerates, not just the vector
//! registers: x87 and MMX share one register file with XMM under XCR0 bit 0,
//! saved by the same xsave/xrstor pair, so a stray `fldz` corrupts user state
//! exactly as a stray `movups` does. Unreviewed xsave/xrstor-family
//! instructions are in scope as well; one overwrites the whole file at once.
//!
//! The soft-float guarantee comes from `targets/x86_64-slos.json`, which a
//! `RUSTFLAGS` override or a hand-written `asm!` sidesteps; this scan of the
//! linked `.text` is the only thing that sees either. Each build variant has a
//! measured allowlist, `scripts/gates/vector/<variant>.txt`, of
//! `<budget>\t<class-or-mnemonic>\t<symbol glob>` entries plus liveness floors
//! and the test-registry size the variant implies, so a mis-stated variant
//! fails instead of passing on the wrong list. A dead entry fails the gate.

use std::collections::HashMap;
use std::fmt::Write as _;

use iced_x86::{Code, CpuidFeature, Instruction, InstructionInfoFactory, Mnemonic, Register};
use object::{Object, ObjectSection};

use super::{Ctx, Image, NO_SYMBOL, Region, regions};
use crate::Result;
use crate::glob;
use crate::report::Report;

const TEST_REGISTRY_ENTRY_SIZE: u64 = 104;
const TEST_REGISTRY_MANY: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Class {
    /// SSE/AVX/AVX-512 registers and MXCSR, plus the other XCR0 components
    /// with registers of their own: opmasks, AMX tiles, MPX bounds.
    Vec,
    X87,
    Mmx,
    /// The xsave/xrstor and fxsave/fxrstor families.
    Xsave,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Vec => "vec",
            Class::X87 => "x87",
            Class::Mmx => "mmx",
            Class::Xsave => "xsave",
        }
    }
}

fn classify(instr: &Instruction, info: &mut InstructionInfoFactory) -> Option<Class> {
    use Mnemonic::*;
    let m = instr.mnemonic();
    if matches!(
        m,
        Xsave
            | Xsave64
            | Xsaveopt
            | Xsaveopt64
            | Xsavec
            | Xsavec64
            | Xsaves
            | Xsaves64
            | Xrstor
            | Xrstor64
            | Xrstors
            | Xrstors64
            | Fxsave
            | Fxsave64
            | Fxrstor
            | Fxrstor64
    ) {
        return Some(Class::Xsave);
    }
    if matches!(
        m,
        Ldmxcsr | Stmxcsr | Vldmxcsr | Vstmxcsr | Vzeroupper | Vzeroall
    ) {
        return Some(Class::Vec);
    }
    let x87_op = instr.cpuid_features().iter().any(|f| {
        matches!(
            f,
            CpuidFeature::FPU
                | CpuidFeature::FPU287
                | CpuidFeature::FPU287XL_ONLY
                | CpuidFeature::FPU387
                | CpuidFeature::FPU387SL_ONLY
                | CpuidFeature::CYRIX_FPU
        )
    });
    if !x87_op && m == Mnemonic::INVALID {
        return None;
    }
    let (mut vec, mut st, mut mm) = (false, false, false);
    for used in info.info(instr).used_registers() {
        let r: Register = used.register();
        vec |= r.is_xmm() || r.is_ymm() || r.is_zmm() || r.is_k() || r.is_tmm() || r.is_bnd();
        st |= r.is_st();
        mm |= r.is_mm();
    }
    if vec {
        Some(Class::Vec)
    } else if x87_op || st {
        Some(Class::X87)
    } else if mm || matches!(m, Emms | Femms) {
        Some(Class::Mmx)
    } else {
        None
    }
}

fn mnemonic_name(m: Mnemonic) -> String {
    format!("{m:?}").to_ascii_lowercase()
}

#[derive(Debug)]
struct Entry {
    line: usize,
    budget: u64,
    class: String,
    glob: String,
    hits: u64,
}

#[derive(Debug)]
struct Allowlist {
    rel: String,
    entries: Vec<Entry>,
    min_returns: u64,
    min_symbols: u64,
    expect_test_registry: String,
}

fn allowlist_rel(variant: &str) -> String {
    format!("scripts/gates/vector/{variant}.txt")
}

fn load_allowlist(ctx: &Ctx) -> Result<Allowlist> {
    let rel = allowlist_rel(ctx.variant);
    if !ctx.repo.exists(&rel) {
        return Err(format!(
            "no allowlist for variant `{}` at {rel}; every gated build variant needs its own \
             measured allowlist (generate one with `--emit-allowlist softfloat` and review it)",
            ctx.variant
        )
        .into());
    }
    parse_allowlist(&rel, &ctx.repo.read(&rel)?)
}

fn parse_allowlist(rel: &str, text: &str) -> Result<Allowlist> {
    let number = |line: usize, s: &str| -> Result<u64> {
        s.trim()
            .parse()
            .map_err(|_| format!("{rel}:{line}: `{s}` is not a count").into())
    };
    let mut entries = Vec::new();
    let (mut min_returns, mut min_symbols, mut expect) = (None, None, None);
    for (i, line) in text.lines().enumerate() {
        let n = i + 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains('\t') {
            let mut fields = line.splitn(3, '\t');
            let (Some(budget), Some(class), Some(glob)) =
                (fields.next(), fields.next(), fields.next())
            else {
                return Err(format!("{rel}:{n}: an entry is `<budget>\\t<class>\\t<glob>`").into());
            };
            entries.push(Entry {
                line: n,
                budget: number(n, budget)?,
                class: class.to_string(),
                glob: glob.to_string(),
                hits: 0,
            });
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, line));
        match key {
            "min-returns" => min_returns = Some(number(n, value)?),
            "min-symbols" => min_symbols = Some(number(n, value)?),
            "expect-test-registry" => match value {
                "few" | "many" | "any" => expect = Some(value.to_string()),
                _ => {
                    return Err(
                        format!("{rel}:{n}: expect-test-registry must be few|many|any").into(),
                    );
                }
            },
            _ => return Err(format!("{rel}:{n}: unknown directive `{key}`").into()),
        }
    }
    let (Some(min_returns), Some(min_symbols), Some(expect_test_registry)) =
        (min_returns, min_symbols, expect)
    else {
        return Err(
            format!("{rel} must set min-returns, min-symbols and expect-test-registry").into(),
        );
    };
    Ok(Allowlist {
        rel: rel.to_string(),
        entries,
        min_returns,
        min_symbols,
        expect_test_registry,
    })
}

fn test_registry_entries(ctx: &Ctx) -> Result<u64> {
    let section = ctx
        .image
        .file
        .section_by_name(".test_registry")
        .ok_or_else(|| {
            format!(
                "{} has no .test_registry section, so it is not a SlopOS kernel image \
             (link.ld brackets that section unconditionally)",
                ctx.path.display()
            )
        })?;
    Ok(section.size() / TEST_REGISTRY_ENTRY_SIZE)
}

fn check_test_registry(ctx: &Ctx, expect: &str) -> Result<()> {
    if expect == "any" {
        return Ok(());
    }
    let entries = test_registry_entries(ctx)?;
    let path = ctx.path.display();
    match expect {
        "many" if entries < TEST_REGISTRY_MANY => Err(format!(
            "--variant {} expects a kernel/tests image, but {path} registers only {entries} \
             test(s): wrong binary",
            ctx.variant
        )
        .into()),
        "few" if entries >= TEST_REGISTRY_MANY => Err(format!(
            "--variant {} expects a non-tests image, but {path} registers {entries} tests: \
             that is the kernel/tests build",
            ctx.variant
        )
        .into()),
        _ => Ok(()),
    }
}

struct Hit {
    count: u64,
    first: u64,
}

struct Scan<'a> {
    regions: Vec<Region<'a>>,
    symbols: u64,
    returns: u64,
    /// Keyed by class, mnemonic and region index.
    hits: HashMap<(Class, Mnemonic, usize), Hit>,
}

fn scan<'a>(image: &Image<'a>) -> Result<Scan<'a>> {
    let regions = regions(image, Some(".text"))?;
    let mut info = InstructionInfoFactory::new();
    let mut hits: HashMap<(Class, Mnemonic, usize), Hit> = HashMap::new();
    let mut returns = 0;
    let mut instr = Instruction::default();
    for (i, region) in regions.iter().enumerate() {
        let mut decoder = region.decoder();
        while decoder.can_decode() {
            decoder.decode_out(&mut instr);
            if instr.code() == Code::Retnq {
                returns += 1;
                continue;
            }
            if let Some(class) = classify(&instr, &mut info) {
                hits.entry((class, instr.mnemonic(), i))
                    .and_modify(|h| h.count += 1)
                    .or_insert(Hit {
                        count: 1,
                        first: instr.ip(),
                    });
            }
        }
    }
    let symbols = regions.iter().filter(|r| !r.names.is_empty()).count() as u64;
    Ok(Scan {
        regions,
        symbols,
        returns,
        hits,
    })
}

pub fn run(ctx: &Ctx) -> Result<Report> {
    let mut allow = load_allowlist(ctx)?;
    check_test_registry(ctx, &allow.expect_test_registry)?;
    let scan = scan(ctx.image)?;
    if scan.symbols < allow.min_symbols || scan.returns < allow.min_returns {
        return Err(format!(
            "only {} symbol(s) and {} return(s) decoded from {} (min-symbols {}, min-returns {} \
             in {}): refusing to report OK, a zero match count means nothing when nothing was \
             disassembled",
            scan.symbols,
            scan.returns,
            ctx.path.display(),
            allow.min_symbols,
            allow.min_returns,
            allow.rel
        )
        .into());
    }

    let mut keys: Vec<_> = scan.hits.iter().collect();
    keys.sort_by(|a, b| b.1.count.cmp(&a.1.count).then(a.0.cmp(b.0)));
    let mut report = Report::new();
    let mut per_class = [0u64; 4];
    let mut allowed = 0;
    for (&(class, mnemonic, region), hit) in keys {
        per_class[class as usize] += hit.count;
        let mnem = mnemonic_name(mnemonic);
        let names = &scan.regions[region].names;
        let entry = allow.entries.iter_mut().find(|e| {
            (e.class == class.name() || e.class == mnem)
                && if names.is_empty() {
                    glob::matches(&e.glob, NO_SYMBOL)
                } else {
                    names.iter().any(|n| glob::matches(&e.glob, n))
                }
        });
        if let Some(entry) = entry {
            entry.hits += hit.count;
            allowed += hit.count;
            continue;
        }
        let name = scan.regions[region].name();
        report.finding(
            name,
            format!(
                "{} × `{mnem}` ({}, first at {:#x} in {}) touches XCR0-managed state outside \
                 the allowlist — keep the kernel +soft-float (targets/x86_64-slos.json features \
                 and rustc-abi); hand-written asm! needs an entry in {} with a reviewed \
                 justification and a measured budget",
                hit.count,
                class.name(),
                hit.first,
                super::demangle(name),
                allow.rel
            ),
        );
    }
    for e in &allow.entries {
        if e.hits == 0 {
            report.at(
                &allow.rel,
                e.line,
                format!(
                    "allowlist entry `{} {}` matches no instruction — delete it; the \
                     instruction is gone or the symbol was renamed, and a dead exemption is how \
                     a mis-stated --variant would otherwise pass",
                    e.class, e.glob
                ),
            );
        } else if e.hits > e.budget {
            report.at(
                &allow.rel,
                e.line,
                format!(
                    "{} — {} hits, budget {}: over budget — the exemption was measured and more \
                     instructions than were reviewed now carry it; re-review before raising the \
                     number",
                    e.glob, e.hits, e.budget
                ),
            );
        }
    }
    let [vec, x87, mmx, xsave] = per_class;
    report.summary(format!(
        "variant={}, {} symbols / {} returns decoded from .text; {allowed} XCR0-touching \
         instruction(s) [{vec} vec, {x87} x87, {mmx} mmx, {xsave} xsave], all in {n}/{n} \
         allowlist entries",
        ctx.variant,
        scan.symbols,
        scan.returns,
        n = allow.entries.len()
    ));
    Ok(report)
}

/// A measured allowlist for `ctx.variant`: every hit as an entry keyed by
/// class and symbol with its count as the budget, and floors at half what the
/// image decodes to. Review every line: an entry records what an instruction
/// is, not that it is acceptable.
pub fn emit_allowlist(ctx: &Ctx) -> Result<String> {
    let registry = match test_registry_entries(ctx)? {
        n if n >= TEST_REGISTRY_MANY => "many",
        _ => "few",
    };
    let scan = scan(ctx.image)?;
    if scan.symbols == 0 || scan.returns == 0 {
        return Err(format!(
            "{} decodes to {} symbol(s) and {} return(s) in .text: nothing to measure",
            ctx.path.display(),
            scan.symbols,
            scan.returns
        )
        .into());
    }
    let mut merged: HashMap<(Class, &str), u64> = HashMap::new();
    for (&(class, _, region), hit) in &scan.hits {
        *merged
            .entry((class, scan.regions[region].name()))
            .or_default() += hit.count;
    }
    let mut lines: Vec<(u64, String)> = merged
        .into_iter()
        .map(|((class, sym), count)| (count, format!("{count}\t{}\t{sym}", class.name())))
        .collect();
    lines.sort_by(|a, b| b.cmp(a));
    let mut out = String::new();
    let _ = write!(
        out,
        "# softfloat allowlist — variant: {}\n#\n# Emitted from {}. Review every line before \
         committing: an emitted entry\n# records what the instruction *is*, not that it is \
         acceptable.\n\nmin-returns {}\nmin-symbols {}\nexpect-test-registry {registry}\n\n",
        ctx.variant,
        ctx.path.display(),
        scan.returns / 2,
        scan.symbols / 2
    );
    for (_, line) in lines {
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use iced_x86::{Decoder, DecoderOptions, Formatter, IntelFormatter};

    use super::super::fixture::{Elf, ctx, with_image};
    use super::*;
    use crate::fixture::Tree;

    const MOVUPS_LOAD_STORE: &[u8] = &[0x0f, 0x10, 0x00, 0x0f, 0x11, 0x00, 0xc3];
    const FNINIT: &[u8] = &[0xdb, 0xe3, 0xc3];
    const EMMS: &[u8] = &[0x0f, 0x77, 0xc3];
    const XSAVE64: &[u8] = &[0x48, 0x0f, 0xae, 0x27, 0xc3];

    /// The script's self-test probe: one user of each class, and two
    /// integer-only functions, one merely *named* like an FPU test.
    fn probe() -> Vec<u8> {
        let mut e = Elf::new();
        e.function("vec_user", MOVUPS_LOAD_STORE);
        e.function("x87_user", FNINIT);
        e.function("mmx_user", EMMS);
        e.function("xsave_user", XSAVE64);
        e.function("plain", &[0x48, 0x8d, 0x47, 0x01, 0xc3]);
        e.function("fpu_xmm_roundtrip_a", &[0x48, 0x8d, 0x04, 0x7f, 0xc3]);
        e.finish()
    }

    fn allowlist(entries: &[&str]) -> String {
        let mut s = "min-returns 1\nmin-symbols 1\nexpect-test-registry any\n".to_string();
        for e in entries {
            s.push_str(e);
            s.push('\n');
        }
        s
    }

    fn gate(elf: &[u8], allow: &str) -> Result<Report> {
        let tree = Tree::new(&[("scripts/gates/vector/selftest.txt", allow)]);
        with_image(elf, |image| run(&ctx(&tree.repo, "selftest", image)))
    }

    fn located(report: &Report) -> Vec<&str> {
        report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect()
    }

    #[test]
    fn every_class_is_rejected_and_integer_code_is_not() {
        let report = gate(&probe(), &allowlist(&[])).unwrap();
        let mut at = located(&report);
        at.sort();
        assert_eq!(at, ["mmx_user", "vec_user", "x87_user", "xsave_user"]);
        let text = report.render("softfloat");
        assert!(text.contains("2 × `movups` (vec"), "{text}");
        assert!(text.contains("`fninit` (x87"), "{text}");
        assert!(text.contains("`emms` (mmx"), "{text}");
        assert!(text.contains("`xsave64` (xsave"), "{text}");
        assert!(!text.contains("plain") && !text.contains("fpu_xmm_roundtrip_a"));
    }

    #[test]
    fn measured_budgets_pass() {
        let report = gate(
            &probe(),
            &allowlist(&[
                "2\tvec\tvec_user",
                "1\tfninit\tx87_user",
                "1\tmmx\tmmx_user",
                "1\txsave\txsave_user",
            ]),
        )
        .unwrap();
        assert!(report.passed(), "{}", report.render("softfloat"));
        assert!(
            report
                .summary
                .contains("5 XCR0-touching instruction(s) [2 vec, 1 x87, 1 mmx, 1 xsave]"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn a_budget_below_the_measured_count_fails() {
        let report = gate(
            &probe(),
            &allowlist(&[
                "1\tvec\tvec_user",
                "1\tfninit\tx87_user",
                "1\tmmx\tmmx_user",
                "1\txsave\txsave_user",
            ]),
        )
        .unwrap();
        assert_eq!(located(&report), ["scripts/gates/vector/selftest.txt:4"]);
        assert!(
            report.findings[0]
                .message
                .contains("2 hits, budget 1: over budget")
        );
    }

    #[test]
    fn an_allowlisted_xsave_within_budget_passes_and_over_budget_fails() {
        let mut e = Elf::new();
        e.function(
            "_RNvNtNtCs1_11slopos_ostd4task3fpu9fpu_xsave",
            &[0x48, 0x0f, 0xae, 0x27, 0x48, 0x0f, 0xae, 0x2f, 0xc3],
        );
        let elf = e.finish();
        let within = gate(&elf, &allowlist(&["3\txsave\t*slopos_ostd4task3fpu*"])).unwrap();
        assert!(within.passed(), "{}", within.render("softfloat"));
        let over = gate(&elf, &allowlist(&["1\txsave\t*slopos_ostd4task3fpu*"])).unwrap();
        assert_eq!(located(&over), ["scripts/gates/vector/selftest.txt:4"]);
        assert!(over.findings[0].message.contains("2 hits, budget 1"));
    }

    #[test]
    fn a_class_exemption_does_not_excuse_another_class() {
        let report = gate(
            &probe(),
            &allowlist(&[
                "9\tx87\tvec_user",
                "1\tfninit\tx87_user",
                "1\tmmx\tmmx_user",
                "1\txsave\txsave_user",
            ]),
        )
        .unwrap();
        let mut at = located(&report);
        at.sort();
        assert_eq!(at, ["scripts/gates/vector/selftest.txt:4", "vec_user"]);
    }

    #[test]
    fn a_literal_mnemonic_does_not_excuse_another_mnemonic() {
        let mut e = Elf::new();
        e.function("x87_user", &[0xdb, 0xe3, 0xd9, 0xee, 0xc3]);
        let report = gate(&e.finish(), &allowlist(&["1\tfninit\tx87_user"])).unwrap();
        assert_eq!(located(&report), ["x87_user"]);
        assert!(report.findings[0].message.contains("`fldz` (x87"));
    }

    #[test]
    fn a_stale_entry_fails() {
        let report = gate(
            &probe(),
            &allowlist(&[
                "2\tvec\tvec_user",
                "1\tfninit\tx87_user",
                "1\tmmx\tmmx_user",
                "1\txsave\txsave_user",
                "1\tvec\t*no_such_symbol*",
            ]),
        )
        .unwrap();
        assert_eq!(located(&report), ["scripts/gates/vector/selftest.txt:8"]);
        assert!(report.findings[0].message.contains("*no_such_symbol*"));
    }

    #[test]
    fn the_liveness_floors_refuse_a_vacuous_pass() {
        let low_returns = "min-returns 99999\nmin-symbols 1\nexpect-test-registry any\n";
        let err = gate(&probe(), low_returns).unwrap_err().to_string();
        assert!(err.contains("refusing to report OK"), "{err}");
        let low_symbols = "min-returns 1\nmin-symbols 7\nexpect-test-registry any\n";
        let err = gate(&probe(), low_symbols).unwrap_err().to_string();
        assert!(err.contains("only 6 symbol(s) and 6 return(s)"), "{err}");
        let exact = "min-returns 6\nmin-symbols 6\nexpect-test-registry any\n";
        assert!(gate(&probe(), exact).is_ok());
    }

    #[test]
    fn the_test_registry_size_must_fit_the_variant() {
        let image = |entries: usize| {
            let mut e = Elf::new();
            e.function("plain", &[0xc3]);
            e.section(".test_registry", entries * 104);
            e.finish()
        };
        let allow =
            |expect: &str| format!("min-returns 1\nmin-symbols 1\nexpect-test-registry {expect}\n");
        assert!(gate(&image(3), &allow("few")).unwrap().passed());
        assert!(gate(&image(100), &allow("many")).unwrap().passed());
        let err = gate(&image(3), &allow("many")).unwrap_err().to_string();
        assert!(err.contains("registers only 3 test(s)"), "{err}");
        let err = gate(&image(100), &allow("few")).unwrap_err().to_string();
        assert!(err.contains("that is the kernel/tests build"), "{err}");
        let err = gate(&probe(), &allow("few")).unwrap_err().to_string();
        assert!(err.contains("no .test_registry section"), "{err}");
    }

    #[test]
    fn a_missing_or_malformed_allowlist_is_an_error() {
        let tree = Tree::new(&[]);
        let elf = probe();
        let err = with_image(&elf, |image| run(&ctx(&tree.repo, "nope", image)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no allowlist for variant `nope`"), "{err}");
        let err = gate(&elf, "min-returns 1\nbogus 1\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("selftest.txt:2: unknown directive `bogus`"),
            "{err}"
        );
        let err = gate(&elf, "min-returns 1\n").unwrap_err().to_string();
        assert!(err.contains("must set min-returns, min-symbols"), "{err}");
    }

    #[test]
    fn an_entry_glob_matches_any_alias_at_the_address() {
        let mut e = Elf::new();
        let at = e.function("_start", FNINIT);
        e.label("_text_start", at);
        e.label("_kernel_start", at);
        let elf = e.finish();
        with_image(&elf, |image| {
            let regions = regions(image, Some(".text")).unwrap();
            assert_eq!(regions[0].names, ["_kernel_start", "_start", "_text_start"]);
            assert_eq!(regions[0].name(), "_text_start");
        });
        assert!(
            gate(&elf, &allowlist(&["1\tfninit\t_start"]))
                .unwrap()
                .passed()
        );
        assert!(
            gate(&elf, &allowlist(&["1\tfninit\t_*start"]))
                .unwrap()
                .passed()
        );
    }

    #[test]
    fn code_before_the_first_symbol_has_no_exemption() {
        let mut e = Elf::new();
        e.code(EMMS);
        e.function("after", &[0xc3]);
        let report = gate(&e.finish(), &allowlist(&["1\tmmx\tafter"])).unwrap();
        let mut at = located(&report);
        at.sort();
        assert_eq!(
            at,
            [
                "<no-enclosing-symbol>",
                "scripts/gates/vector/selftest.txt:4"
            ]
        );
    }

    #[test]
    fn state_the_script_never_named_is_flagged_too() {
        let cases: &[(&[u8], &str, &str)] = &[
            (&[0xc5, 0xf8, 0x92, 0xc8], "kmovw", "vec"),
            (&[0x0f, 0xae, 0x10], "ldmxcsr", "vec"),
            (&[0xc5, 0xf8, 0x77], "vzeroupper", "vec"),
            (&[0x48, 0x0f, 0xae, 0x07], "fxsave64", "xsave"),
            (&[0x48, 0x0f, 0xae, 0x2f], "xrstor64", "xsave"),
            (&[0x0f, 0x0e], "femms", "mmx"),
        ];
        let mut info = InstructionInfoFactory::new();
        for (bytes, mnem, class) in cases {
            let instr = Decoder::with_ip(64, bytes, 0, DecoderOptions::NONE).decode();
            assert_eq!(mnemonic_name(instr.mnemonic()), *mnem);
            assert_eq!(
                classify(&instr, &mut info).map(Class::name),
                Some(*class),
                "{mnem}"
            );
        }
    }

    #[test]
    fn emitted_allowlist_round_trips_through_the_gate() {
        let mut e = Elf::new();
        e.function("vec_user", MOVUPS_LOAD_STORE);
        e.function("mixed", &[0x0f, 0x10, 0x00, 0xc5, 0xf8, 0x77, 0xc3]);
        let at = e.function("_start", FNINIT);
        e.label("_text_start", at);
        e.function("xsave_user", XSAVE64);
        e.section(".test_registry", 104);
        let elf = e.finish();
        let tree = Tree::new(&[]);
        let emitted =
            with_image(&elf, |image| emit_allowlist(&ctx(&tree.repo, "rt", image))).unwrap();
        assert_eq!(
            emitted,
            "# softfloat allowlist — variant: rt\n#\n# Emitted from kernel.elf. Review every line \
             before committing: an emitted entry\n# records what the instruction *is*, not that \
             it is acceptable.\n\nmin-returns 2\nmin-symbols 2\nexpect-test-registry few\n\n\
             2\tvec\tvec_user\n2\tvec\tmixed\n1\txsave\txsave_user\n1\tx87\t_text_start\n"
        );
        tree.write("scripts/gates/vector/rt.txt", &emitted);
        let report = with_image(&elf, |image| run(&ctx(&tree.repo, "rt", image))).unwrap();
        assert!(report.passed(), "{}", report.render("softfloat"));
    }

    /// The script's classifier, over Intel-syntax text: an operand naming an
    /// XMM/YMM/ZMM, ST or MM register, or a mnemonic in its lists.
    fn script_flags(mnemonic: &str, operands: &str) -> bool {
        let tokens: Vec<&str> = operands
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        let reg = |prefix: &str, max: u32| {
            tokens.iter().any(|t| {
                t.strip_prefix(prefix)
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_some_and(|n| n <= max)
            })
        };
        let m = mnemonic.strip_prefix('v').unwrap_or(mnemonic);
        reg("xmm", 99)
            || reg("ymm", 99)
            || reg("zmm", 99)
            || reg("mm", 7)
            || tokens.contains(&"st")
            || matches!(m, "ldmxcsr" | "stmxcsr" | "zeroupper" | "zeroall")
            || mnemonic.starts_with('f')
            || mnemonic == "emms"
            || ["xsave", "xrstor", "fxsave", "fxrstor"]
                .iter()
                .any(|p| mnemonic.starts_with(p))
    }

    #[test]
    fn every_mnemonic_the_script_named_is_flagged() {
        let mut info = InstructionInfoFactory::new();
        let mut checked = 0;
        for code in Code::values().filter(|&c| c != Code::INVALID) {
            let name = mnemonic_name(code.mnemonic());
            if !script_flags(&name, "") {
                continue;
            }
            let mut instr = Instruction::default();
            instr.set_code(code);
            assert!(
                classify(&instr, &mut info).is_some(),
                "{code:?} ({name}) slipped through"
            );
            checked += 1;
        }
        assert!(
            checked > 150,
            "only {checked} codes carried a flagged mnemonic"
        );
    }

    /// Decodes a broad corpus: every opcode of the legacy 0F, 0F38 and 0F3A
    /// maps under each mandatory prefix with and without REX.W, the x87
    /// escapes with every ModRM, and VEX/EVEX opcodes over each map, length
    /// and prefix, with register and memory ModRM forms.
    fn corpus() -> Vec<Vec<u8>> {
        let modrms: Vec<u8> = (0..8u8)
            .flat_map(|reg| [reg << 3, 0xc0 | reg << 3 | 1])
            .collect();
        let tail = [0u8; 8];
        let mut out = Vec::new();
        for prefix in [None, Some(0x66u8), Some(0xf2), Some(0xf3)] {
            for rex in [None, Some(0x48u8)] {
                for map in [&[0x0fu8][..], &[0x0f, 0x38], &[0x0f, 0x3a]] {
                    for op in 0..=255u8 {
                        for &modrm in &modrms {
                            let mut v: Vec<u8> = prefix.into_iter().chain(rex).collect();
                            v.extend_from_slice(map);
                            v.push(op);
                            v.push(modrm);
                            v.extend_from_slice(&tail);
                            out.push(v);
                        }
                    }
                }
            }
        }
        for esc in 0xd8..=0xdfu8 {
            for modrm in 0..=255u8 {
                let mut v = vec![esc, modrm];
                v.extend_from_slice(&tail);
                out.push(v);
            }
        }
        for map in 1..=3u8 {
            for w in [0u8, 0x80] {
                for l in [0u8, 4] {
                    for pp in 0..4u8 {
                        for op in 0..=255u8 {
                            for &modrm in &modrms {
                                let mut v = vec![0xc4, 0xe0 | map, w | 0x78 | l | pp, op, modrm];
                                v.extend_from_slice(&tail);
                                out.push(v);
                            }
                        }
                    }
                }
            }
        }
        for map in [1u8, 2, 3, 5, 6] {
            for w in [0u8, 0x80] {
                for ll in [0u8, 0x20, 0x40] {
                    for pp in 0..4u8 {
                        for op in 0..=255u8 {
                            for &modrm in &modrms[..4] {
                                let mut v =
                                    vec![0x62, 0xf0 | map, w | 0x7c | pp, 0x08 | ll, op, modrm];
                                v.extend_from_slice(&tail);
                                out.push(v);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_instruction_the_script_flags_is_flagged() {
        let mut info = InstructionInfoFactory::new();
        let mut formatter = IntelFormatter::new();
        let (mut mnemonic, mut operands) = (String::new(), String::new());
        let mut flagged = 0;
        for bytes in corpus() {
            let instr = Decoder::with_ip(64, &bytes, 0x1000, DecoderOptions::NONE).decode();
            if instr.is_invalid() {
                continue;
            }
            mnemonic.clear();
            operands.clear();
            formatter.format_mnemonic(&instr, &mut mnemonic);
            formatter.format_all_operands(&instr, &mut operands);
            let mnemonic = mnemonic.rsplit(' ').next().unwrap_or_default();
            if script_flags(mnemonic, &operands) {
                flagged += 1;
                assert!(
                    classify(&instr, &mut info).is_some(),
                    "`{mnemonic} {operands}` ({bytes:02x?}) slipped through"
                );
            }
        }
        assert!(
            flagged > 10_000,
            "only {flagged} corpus instructions were in scope"
        );
    }
}
