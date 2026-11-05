//! The linked kernel keeps to the linker-section contract `link.ld` states. A
//! source scan sees what the tree expands to; only the linked image shows a
//! third-party crate's `#[unsafe(link_section)]`, since registry dependencies
//! expand their own macros and no first-party scan reads them.
//!
//! 1. Every PROGBITS/NOBITS section is an output section `link.ld` declares or
//!    one the toolchain adds beside them. Anything else is a static placed
//!    outside the sanctioned set.
//! 2. Each registry — an output section `link.ld` brackets with
//!    `__start_<name>` and `__stop_<name>` — is tiled by its entries' symbols:
//!    one size, no byte uncovered, no overlap. `registry_slice` derives its
//!    count from the span and `ptr::offset_from` requires a whole number of
//!    elements, so a wrong-sized entry, reachable from a crate that forbids
//!    unsafe, is undefined behaviour at boot. The sizes come from the image
//!    and the registries from the script, so neither is restated here.
//! 3. The unwinder's FDE index is present and the only finder linked.
//!    `.eh_frame_hdr` is a `12 + 8N` byte search table over `.eh_frame`;
//!    without it every lookup parses every FDE (~47 ms at opt-level 0), and
//!    that path fails open: the parse error is swallowed and the finder
//!    falls through to a linear scan. Requiring the `fde-static` finder's
//!    symbol to be absent pins the configuration from the other side.

use std::collections::{BTreeMap, HashMap};

use object::elf::{SHT_NOBITS, SHT_PROGBITS};
use object::read::elf::SectionHeader as _;
use object::{Object, ObjectSection, ObjectSymbol, SectionIndex, SymbolKind};

use super::{Ctx, demangle};
use crate::Result;
use crate::report::Report;

const LINK_SCRIPT: &str = "link.ld";

const EH_FRAME_HDR_PROLOGUE: u64 = 12;
const EH_FRAME_HDR_ENTRY: u64 = 8;

/// Sections the toolchain adds beside `link.ld`'s: the records
/// `-Zemit-stack-sizes` asks for, and DWARF.
fn toolchain_section(name: &str) -> bool {
    name == ".stack_sizes" || name.starts_with(".debug_")
}

pub fn run(ctx: &Ctx) -> Result<Report> {
    let script =
        Script::parse(&ctx.repo.read(LINK_SCRIPT)?).map_err(|e| format!("{LINK_SCRIPT}: {e}"))?;
    if script.registries.is_empty() {
        return Err(format!(
            "{LINK_SCRIPT} brackets no section with __start_/__stop_ symbols — refusing to report OK"
        )
        .into());
    }

    let file = &ctx.image.file;
    let endian = file.endian();
    let mut report = Report::new();
    let mut placed = Vec::new();
    for section in file.sections() {
        let sh_type = section.elf_section_header().sh_type(endian);
        if sh_type != SHT_PROGBITS && sh_type != SHT_NOBITS {
            continue;
        }
        let name = section.name()?;
        if !script.sections.iter().any(|s| s == name) && !toolchain_section(name) {
            report.finding(
                name,
                "section link.ld does not declare — something placed a static outside the sanctioned set, most likely a dependency's own #[unsafe(link_section)]; identify it before declaring it there",
            );
        }
        placed.push((name, section.index(), section.address(), section.size()));
    }

    let mut entries: HashMap<SectionIndex, Vec<Entry>> = HashMap::new();
    let mut registries = Vec::new();
    for registry in &script.registries {
        match placed.iter().find(|&&(name, ..)| name == registry) {
            Some(&(name, index, start, span)) => {
                entries.insert(index, Vec::new());
                registries.push((name, index, start, span));
            }
            None => report.finding(
                registry.as_str(),
                "link.ld brackets this registry but the image has no such section — the image was linked from another script",
            ),
        }
    }

    for sym in file.symbols() {
        let name = sym.name()?;
        if is_linear_finder(name) {
            report.finding(
                name,
                "the linear fde-static finder is linked: it is tried whenever the indexed lookup misses, silently restoring the cost .eh_frame_hdr removes — root Cargo.toml must select fde-gnu-eh-frame-hdr instead of fde-static, not alongside it",
            );
        }
        if sym.kind() != SymbolKind::Data {
            continue;
        }
        if let Some(list) = sym.section_index().and_then(|i| entries.get_mut(&i)) {
            list.push(Entry {
                name,
                at: sym.address(),
                size: sym.size(),
            });
        }
    }

    let mut tiled = 0;
    for &(name, index, start, span) in &registries {
        let list = entries.get_mut(&index).expect("registered above");
        tiled += tile(&mut report, name, start, span, list);
    }

    let mut index = 0;
    match placed.iter().find(|&&(n, ..)| n == ".eh_frame_hdr") {
        None => report.finding(
            ctx.path.display().to_string(),
            "carries no .eh_frame_hdr, so the unwinder falls back to a linear .eh_frame scan per lookup — link.ld must declare the output section and targets/x86_64-slos.json must set eh-frame-header true",
        ),
        Some(&(.., size))
            if size < EH_FRAME_HDR_PROLOGUE + EH_FRAME_HDR_ENTRY
                || !(size - EH_FRAME_HDR_PROLOGUE).is_multiple_of(EH_FRAME_HDR_ENTRY) =>
        {
            report.finding(
                ".eh_frame_hdr",
                format!(
                    "{size} bytes, not {EH_FRAME_HDR_PROLOGUE} + {EH_FRAME_HDR_ENTRY} x N — the search table is malformed or absent, and a header that parses without a table degrades to the linear scan"
                ),
            )
        }
        Some(&(.., size)) => index = (size - EH_FRAME_HDR_PROLOGUE) / EH_FRAME_HDR_ENTRY,
    }

    report.summary(format!(
        "{} registries tiled by {tiled} entries of one size each; {} PROGBITS/NOBITS sections all declared by link.ld or the toolchain; unwind index holds {index} entries",
        registries.len(),
        placed.len(),
    ));
    Ok(report)
}

struct Entry<'a> {
    name: &'a str,
    at: u64,
    size: u64,
}

/// Walks `entries` over the registry's `span` bytes from `start`, reporting
/// entries of more than one size, bytes no entry covers and an entry that
/// overlaps the one before it. Returns how many entries tile it.
fn tile(
    report: &mut Report,
    registry: &str,
    start: u64,
    span: u64,
    entries: &mut Vec<Entry>,
) -> usize {
    entries.sort_by_key(|e| (e.at, e.size));
    entries.dedup_by_key(|e| (e.at, e.size));

    let mut sizes: BTreeMap<u64, Vec<&str>> = BTreeMap::new();
    for e in entries.iter() {
        sizes.entry(e.size).or_default().push(e.name);
    }
    if sizes.is_empty() {
        if span != 0 {
            report.finding(
                registry,
                format!(
                    "{span} bytes and no entry symbol among them — the image carries no symbol table, or something placed unnamed bytes in the registry"
                ),
            );
        }
        return 0;
    }
    if sizes.len() > 1 {
        let common = sizes
            .iter()
            .max_by_key(|(_, names)| names.len())
            .map(|(&size, _)| size)
            .expect("not empty");
        let counts: Vec<String> = sizes
            .iter()
            .map(|(size, names)| format!("{size} bytes x{}", names.len()))
            .collect();
        let odd: Vec<String> = sizes
            .iter()
            .filter(|&(&size, _)| size != common)
            .flat_map(|(_, names)| names.iter().map(|n| demangle(n)))
            .collect();
        report.finding(
            registry,
            format!(
                "entries of {} sizes ({}): registry_slice walks the section as one type, so a static of another landed in it — {}",
                sizes.len(),
                counts.join(", "),
                odd.join(", ")
            ),
        );
    }

    let end = start + span;
    let mut at = start;
    for e in entries.iter() {
        if e.at > at {
            report.finding(registry, uncovered(at - start, e.at - start));
        } else if e.at < at {
            report.finding(
                registry,
                format!("{} overlaps the entry before it", demangle(e.name)),
            );
        }
        at = at.max(e.at + e.size);
    }
    if at < end {
        report.finding(registry, uncovered(at - start, span));
    } else if at > end {
        report.finding(
            registry,
            format!(
                "its entries run {} bytes past the section's {span}",
                at - end
            ),
        );
    }
    entries.len()
}

fn uncovered(from: u64, to: u64) -> String {
    format!(
        "bytes {from:#x}..{to:#x} hold no entry — a static with no symbol, or padding between entries, breaks the count registry_slice derives from the span"
    )
}

/// What `link.ld`'s `SECTIONS` command declares.
#[derive(Debug, Default, PartialEq)]
struct Script {
    /// Every output section but `/DISCARD/`, in order.
    sections: Vec<String>,
    /// The output sections that set `__start_<name> = .` and
    /// `__stop_<name> = .` around their input: the registries.
    registries: Vec<String>,
}

const KEYWORD: &str = "SECTIONS";

impl Script {
    fn parse(text: &str) -> std::result::Result<Self, String> {
        let text = strip_comments(text);
        let at = text
            .match_indices(KEYWORD)
            .map(|(at, _)| at)
            .find(|&at| {
                !text[..at].ends_with(ident_char)
                    && !text[at + KEYWORD.len()..].starts_with(ident_char)
            })
            .ok_or("no SECTIONS command")?;
        let rest = &text[at + KEYWORD.len()..];
        let open = rest.find('{').ok_or("SECTIONS has no body")?;
        if !rest[..open].trim().is_empty() {
            return Err("SECTIONS is not followed by its body".into());
        }

        let mut script = Script::default();
        let mut head = open + 1;
        let mut body = 0;
        let mut name = None;
        let mut depth = 0usize;
        let mut quoted = false;
        for (i, c) in rest.char_indices().skip_while(|&(i, _)| i <= open) {
            match c {
                '"' => quoted = !quoted,
                _ if quoted => {}
                ';' if depth == 0 => head = i + 1,
                '{' => {
                    if depth == 0 {
                        name = output_section(&rest[head..i]);
                        body = i + 1;
                    }
                    depth += 1;
                }
                '}' if depth == 0 => return Ok(script),
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Some(name) = name.take() {
                            let bare = &name[1..];
                            let inner = &rest[body..i];
                            if assigns_dot(inner, &format!("__start_{bare}"))
                                && assigns_dot(inner, &format!("__stop_{bare}"))
                            {
                                script.registries.push(name.clone());
                            }
                            script.sections.push(name);
                        }
                        head = i + 1;
                    }
                }
                _ => {}
            }
        }
        Err("SECTIONS is not closed".into())
    }
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("/*") {
        out.push_str(&rest[..open]);
        out.push(' ');
        rest = match rest[open + 2..].find("*/") {
            Some(close) => &rest[open + 2 + close + 2..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The output section a statement's head names: `.name [address] [(type)] :`,
/// after whatever ended the statement before it — a `:phdr`, an `ASSERT`
/// without its `;`. `None` for `/DISCARD/` and for anything else.
fn output_section(head: &str) -> Option<String> {
    let tokens = tokens(head);
    let colon = tokens
        .iter()
        .position(|t| *t == ":" || (t.ends_with(':') && !t.starts_with(':')))?;
    let name = tokens[..=colon]
        .iter()
        .rev()
        .map(|t| t.trim_end_matches(':'))
        .find(|t| t.starts_with(['.', '/']))?;
    (name.len() > 1 && name.starts_with('.')).then(|| name.to_string())
}

/// `head` split at whitespace outside parentheses and strings, so
/// `ASSERT(SIZEOF(.x) >= 12, "a b")` and `ALIGN(8)` are one token each.
fn tokens(head: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut quoted = false;
    for (i, c) in head.char_indices() {
        match c {
            '"' => {
                quoted = !quoted;
                start.get_or_insert(i);
            }
            _ if quoted => {}
            '(' => {
                depth += 1;
                start.get_or_insert(i);
            }
            ')' => depth = depth.saturating_sub(1),
            c if c.is_whitespace() && depth == 0 => {
                if let Some(s) = start.take() {
                    out.push(&head[s..i]);
                }
            }
            _ => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(s) = start {
        out.push(&head[s..]);
    }
    out
}

/// Whether `body` holds `symbol = .;`, the symbol taking the location counter.
fn assigns_dot(body: &str, symbol: &str) -> bool {
    body.match_indices(symbol).any(|(at, _)| {
        if body[..at].ends_with(ident_char) {
            return false;
        }
        let next: String = body[at + symbol.len()..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(3)
            .collect();
        next == "=.;"
    })
}

/// `find_fde5fixed.*StaticFinder.*find_fde`, matched against the mangled name
/// so a rename fails loudly.
fn is_linear_finder(name: &str) -> bool {
    let Some(i) = name.find("find_fde5fixed") else {
        return false;
    };
    let rest = &name[i + "find_fde5fixed".len()..];
    let Some(j) = rest.find("StaticFinder") else {
        return false;
    };
    rest[j + "StaticFinder".len()..].contains("find_fde")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use object::write::{Object as Builder, SectionId, Symbol, SymbolSection};
    use object::{Architecture, BinaryFormat, Endianness, SectionKind, SymbolFlags, SymbolScope};

    use super::*;
    use crate::elf::Image;
    use crate::fixture::Tree;

    /// Shaped as `link.ld` is: phdr suffixes, `(NOLOAD)`, an `ASSERT` whose
    /// message holds braces and semicolons, a commented-out brace, and
    /// `/DISCARD/`.
    const SCRIPT: &str = r#"
OUTPUT_FORMAT(elf64-x86-64)
PHDRS
{
  text PT_LOAD FLAGS(5);
}
SECTIONS
{
  . = 0xFFFFFFFF80000000;
  .text ALIGN(4096) : {
    _text_start = .;
    *(.text .text.*)
  } :text
  /* a stray { in a comment */
  .eh_frame_hdr ALIGN(8) : {
    __GNU_EH_FRAME_HDR = .;
    KEEP(*(.eh_frame_hdr))
  } :rodata
  ASSERT(SIZEOF(.eh_frame_hdr) >= 12, "link.ld: { empty; index }")
  .driver_registry ALIGN(8) : {
    __start_driver_registry = .;
    KEEP(*(.driver_registry))
    __stop_driver_registry = .;
  } :rodata
  .kconsole_registry ALIGN(8) : {
    __start_kconsole_registry=.;
    KEEP(*(.kconsole_registry))
    __stop_kconsole_registry = . ;
  } :rodata
  .data ALIGN(4096) : {
    *(.data .data.*)
  } :data
  .bss (NOLOAD) : ALIGN(4096) {
    *(.bss .bss.*)
  } :data
  /DISCARD/ : {
    *(.comment*)
  }
}
"#;

    const INDEX: usize = 20;
    const FINDER: &str = "_RNvXNtNtNtCs0000000000_9unwinding8unwinder8find_fde5fixedNtB2_12StaticFinderNtB4_9FDEFinder8find_fde";

    struct Fixture {
        obj: Builder<'static>,
    }

    impl Fixture {
        fn bare() -> Self {
            Self {
                obj: Builder::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little),
            }
        }

        /// An unwind index of one entry and nothing else.
        fn new() -> Self {
            Self::bare().bytes(".eh_frame_hdr", INDEX)
        }

        fn section(&mut self, name: &str, len: usize) -> SectionId {
            let s = self.obj.add_section(
                Vec::new(),
                name.as_bytes().to_vec(),
                SectionKind::ReadOnlyData,
            );
            self.obj.append_section_data(s, &vec![0; len], 8);
            s
        }

        fn symbol(&mut self, section: SectionId, name: &str, at: u64, size: u64) {
            self.obj.add_symbol(Symbol {
                name: name.as_bytes().to_vec(),
                value: at,
                size,
                kind: SymbolKind::Data,
                scope: SymbolScope::Compilation,
                weak: false,
                section: SymbolSection::Section(section),
                flags: SymbolFlags::None,
            });
        }

        /// A section of `len` bytes no symbol names.
        fn bytes(mut self, name: &str, len: usize) -> Self {
            self.section(name, len);
            self
        }

        /// A `len`-byte section holding a data symbol per `(name, at, size)`.
        fn registry(mut self, name: &str, len: usize, entries: &[(&str, u64, u64)]) -> Self {
            let s = self.section(name, len);
            for &(entry, at, size) in entries {
                self.symbol(s, entry, at, size);
            }
            self
        }

        /// `n` contiguous `size`-byte entries.
        fn tiled(self, name: &str, size: u64, n: u64) -> Self {
            let entries: Vec<(String, u64, u64)> = (0..n)
                .map(|i| (format!("{name}_ENTRY_{i}"), i * size, size))
                .collect();
            let entries: Vec<(&str, u64, u64)> = entries
                .iter()
                .map(|(e, at, size)| (e.as_str(), *at, *size))
                .collect();
            self.registry(name, (size * n) as usize, &entries)
        }

        fn elf(self) -> Vec<u8> {
            self.obj.write().unwrap()
        }
    }

    fn gate_with(script: &str, elf: &[u8]) -> Result<Report> {
        let tree = Tree::new(&[("link.ld", script)]);
        let image = Image::parse(elf).unwrap();
        run(&Ctx {
            repo: &tree.repo,
            path: Path::new("kernel.elf"),
            variant: "dev",
            image: &image,
        })
    }

    fn gate(elf: &[u8]) -> Report {
        gate_with(SCRIPT, elf).unwrap()
    }

    fn both() -> Fixture {
        Fixture::new()
            .tiled(".driver_registry", 56, 3)
            .tiled(".kconsole_registry", 48, 2)
    }

    fn only(r: &Report, location: &str, message: &str) {
        assert_eq!(r.findings.len(), 1, "{}", r.render("registry-sections"));
        assert_eq!(r.findings[0].location, location);
        assert!(
            r.findings[0].message.contains(message),
            "{}",
            r.findings[0].message
        );
    }

    #[test]
    fn the_script_declares_its_sections_and_brackets_its_registries() {
        let script = Script::parse(SCRIPT).unwrap();
        assert_eq!(
            script.sections,
            [
                ".text",
                ".eh_frame_hdr",
                ".driver_registry",
                ".kconsole_registry",
                ".data",
                ".bss"
            ]
        );
        assert_eq!(
            script.registries,
            [".driver_registry", ".kconsole_registry"]
        );
    }

    #[test]
    fn a_registry_needs_both_brackets_at_the_location_counter() {
        for body in [
            "__start_r = .; KEEP(*(.r))",
            "KEEP(*(.r)) __stop_r = .;",
            "__start_r = . + 8; KEEP(*(.r)) __stop_r = .;",
            "x__start_r = .; KEEP(*(.r)) __stop_r = .;",
            "__start_r_more = .; KEEP(*(.r)) __stop_r = .;",
        ] {
            let script = Script::parse(&format!("SECTIONS {{ .r : {{ {body} }} }}")).unwrap();
            assert_eq!(script.sections, [".r"], "{body}");
            assert!(script.registries.is_empty(), "{body}");
        }
    }

    #[test]
    fn a_script_without_a_closed_sections_command_is_an_error() {
        for text in [
            "PHDRS { text PT_LOAD; }",
            "/* SECTIONS { .a : { } } */",
            "SECTIONS { .a : { }",
            "MY_SECTIONS { .a : { } }",
        ] {
            assert!(Script::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn tiled_registries_pass() {
        let r = gate(&both().elf());
        assert!(r.passed(), "{}", r.render("registry-sections"));
        assert!(
            r.summary
                .starts_with("2 registries tiled by 5 entries of one size each"),
            "{}",
            r.summary
        );
        assert!(r.summary.ends_with("unwind index holds 1 entries"));
    }

    #[test]
    fn an_empty_registry_passes() {
        let r = gate(
            &Fixture::new()
                .tiled(".driver_registry", 56, 1)
                .bytes(".kconsole_registry", 0)
                .elf(),
        );
        assert!(r.passed(), "{}", r.render("registry-sections"));
    }

    #[test]
    fn entries_of_another_size_are_rejected_even_when_the_span_divides() {
        let r = gate(
            &Fixture::new()
                .registry(
                    ".driver_registry",
                    168,
                    &[("DRIVER_A", 0, 56), ("FOREIGN", 56, 112)],
                )
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        only(
            &r,
            ".driver_registry",
            "entries of 2 sizes (56 bytes x1, 112 bytes x1)",
        );
    }

    #[test]
    fn the_odd_entry_out_is_named() {
        let r = gate(
            &Fixture::new()
                .registry(
                    ".driver_registry",
                    120,
                    &[
                        ("DRIVER_A", 0, 56),
                        ("DRIVER_B", 56, 56),
                        ("SNEAKY", 112, 8),
                    ],
                )
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        only(&r, ".driver_registry", "8 bytes x1, 56 bytes x2");
        assert!(r.findings[0].message.ends_with("— SNEAKY"));
    }

    #[test]
    fn bytes_no_entry_covers_are_rejected() {
        for (len, entries, uncovered) in [
            (
                120,
                &[("A", 0, 56), ("B", 64, 56)][..],
                "bytes 0x38..0x40 hold no entry",
            ),
            (112, &[("A", 0, 56)][..], "bytes 0x38..0x70 hold no entry"),
            (112, &[("B", 56, 56)][..], "bytes 0x0..0x38 hold no entry"),
            (56, &[][..], "56 bytes and no entry symbol among them"),
        ] {
            let r = gate(
                &Fixture::new()
                    .registry(".driver_registry", len, entries)
                    .tiled(".kconsole_registry", 48, 1)
                    .elf(),
            );
            only(&r, ".driver_registry", uncovered);
        }
    }

    #[test]
    fn overlapping_entries_are_rejected() {
        let r = gate(
            &Fixture::new()
                .registry(".driver_registry", 104, &[("A", 0, 56), ("B", 48, 56)])
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        only(&r, ".driver_registry", "B overlaps the entry before it");
    }

    #[test]
    fn an_entry_past_the_end_is_rejected() {
        let r = gate(
            &Fixture::new()
                .registry(".driver_registry", 56, &[("A", 0, 56), ("B", 56, 56)])
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        only(&r, ".driver_registry", "run 56 bytes past the section's 56");
    }

    #[test]
    fn aliases_of_one_entry_count_once() {
        let r = gate(
            &Fixture::new()
                .registry(
                    ".driver_registry",
                    112,
                    &[("A", 0, 56), ("A_ALIAS", 0, 56), ("B", 56, 56)],
                )
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        assert!(r.passed(), "{}", r.render("registry-sections"));
        assert!(r.summary.starts_with("2 registries tiled by 3 entries"));
    }

    #[test]
    fn an_undeclared_section_is_rejected() {
        let r = gate(&both().bytes(".sneaky_registry", 8).elf());
        only(&r, ".sneaky_registry", "link.ld does not declare");
    }

    #[test]
    fn declared_and_toolchain_sections_pass() {
        let r = gate(
            &both()
                .bytes(".text", 16)
                .bytes(".data", 8)
                .bytes(".stack_sizes", 4)
                .bytes(".debug_info", 4)
                .elf(),
        );
        assert!(r.passed(), "{}", r.render("registry-sections"));
    }

    #[test]
    fn a_registry_the_image_lacks_is_rejected() {
        let r = gate(&Fixture::new().tiled(".driver_registry", 56, 1).elf());
        only(&r, ".kconsole_registry", "the image has no such section");
    }

    #[test]
    fn a_script_bracketing_no_registry_is_an_error() {
        let e = gate_with(
            "SECTIONS { .data : { *(.data) } }",
            &Fixture::new().bytes(".data", 8).elf(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("refusing to report OK"), "{e}");
    }

    #[test]
    fn a_missing_unwind_index_is_rejected() {
        let r = gate(
            &Fixture::bare()
                .tiled(".driver_registry", 56, 1)
                .tiled(".kconsole_registry", 48, 1)
                .elf(),
        );
        only(&r, "kernel.elf", "carries no .eh_frame_hdr");
    }

    #[test]
    fn a_malformed_unwind_index_is_rejected() {
        for size in [18, 12, 21] {
            let r = gate(
                &Fixture::bare()
                    .bytes(".eh_frame_hdr", size)
                    .tiled(".driver_registry", 56, 1)
                    .tiled(".kconsole_registry", 48, 1)
                    .elf(),
            );
            only(&r, ".eh_frame_hdr", "not 12 + 8 x N");
        }
    }

    #[test]
    fn the_linear_fde_static_finder_is_rejected() {
        let mut fixture = both();
        let data = fixture.section(".data", 8);
        fixture.symbol(data, FINDER, 0, 8);
        let r = gate(&fixture.elf());
        only(&r, FINDER, "linear fde-static finder");
    }

    #[test]
    fn the_indexed_finder_is_not_the_linear_one() {
        assert!(!is_linear_finder(
            "_ZN9unwinding8unwinder8find_fde16gnu_eh_frame_hdr8find_fde17h0123456789abcdefE"
        ));
        assert!(is_linear_finder(FINDER));
    }
}
