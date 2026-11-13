//! No kernel crate's macro *expansion* carries `unsafe` that no source scan
//! can see.
//!
//! `#![forbid(unsafe_code)]` and the unsafe-outside-ostd source gate share a
//! blind spot: rustc drops an `unsafe_code` diagnostic whose span lies in an
//! external macro (the lint does not declare `report_in_external_macro`), so a
//! macro defined in another crate — OSTD's, a proc macro, a registry
//! dependency's — can expand `unsafe` into a forbid crate with no diagnostic
//! and no keyword at the call site. `--force-warn unsafe_code` fires in the
//! defining crate, not at the call site.
//!
//! So this gate expands each kernel crate (`-Zunpretty=expanded`, once per
//! feature configuration, since the `stest!` sites sit behind a test
//! feature), parses the expansion and holds it to a constant rather than a
//! recorded per-crate count, which would be re-recorded on every legitimate
//! change and stop meaning anything the first time someone bumped it:
//!
//! - executable unsafe: none (`unsafe` blocks, `unsafe fn` save the methods
//!   an allowlisted unsafe trait requires, `unsafe extern`, `unsafe trait`);
//! - `unsafe impl` only of a trait in [`TRAIT_ALLOWLIST`];
//! - `#[unsafe(link_section)]` only of a section in [`SECTION_ALLOWLIST`];
//! - `#[unsafe(no_mangle)]` / `#[unsafe(export_name)]` only of a symbol in
//!   [`SYMBOL_ALLOWLIST`].
//!
//! The compiler's own emissions are recognised by shape: `#[derive(Clone,
//! Copy)]` emits `unsafe impl ::core::clone::TrivialClone`, and
//! `#[derive(PartialEq)]` on a multi-variant enum with fields emits
//! `unsafe { ::core::intrinsics::unreachable() }`, both inside
//! `#[automatically_derived]` impls. `-Zunpretty=expanded` is unstable; a test
//! expands a fixture with the pinned toolchain and fails if those shapes move,
//! and a run that sees no `TrivialClone` impl at all refuses to pass.

use std::collections::HashSet;
use std::time::Instant;

use proc_macro2::{Span, TokenStream};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, Meta, Stmt};

use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{self, unsafe_keywords};
use crate::toolchain;
use crate::workspace::Workspace;

/// Crate directories that legitimately author unsafe, as in the
/// unsafe-outside-ostd lint-attribute scan, plus every vendored crate.
const EXEMPT: &[&str] = &["slopos-ostd", "slopos-ostd-derive", "kernel"];

/// Traits whose `unsafe impl` something mechanical discharges, so the impl is
/// sanctioned rather than merely tolerated. An entry with `::` names the
/// trait path exactly as the expansion spells it; a bare name matches the
/// last segment.
///
/// - `Pod` / `Zeroable`: `#[derive]`d; the derive checks repr and gives every
///   field the same bound.
/// - `HermeticState`: hand-written per impl. The obligation is real —
///   `restore` republishes saved PCR pointers and MSRs — and belongs to the
///   invoking crate.
/// - `PcrStackTy`: declares one of the two stack-handle types the live kernel
///   builds tasks from.
/// - `TrivialClone`: compiler-emitted by `#[derive(Clone, Copy)]`.
pub const TRAIT_ALLOWLIST: &[&str] = &[
    "::slopos_ostd::Pod",
    "::slopos_ostd::Zeroable",
    "slopos_ostd::Pod",
    "slopos_ostd::Zeroable",
    "crate::Pod",
    "crate::Zeroable",
    "HermeticState",
    "PcrStackTy",
    "::core::clone::TrivialClone",
];

/// The twelve sections link.ld brackets and the three the Limine boot
/// protocol reads. OSTD's `registry_entry!` / `limine_request!` own every
/// one of these.
pub const SECTION_ALLOWLIST: &[&str] = &[
    ".boot_init_early_hw",
    ".boot_init_memory",
    ".boot_init_drivers",
    ".boot_init_services",
    ".boot_init_optional",
    ".driver_registry",
    ".platform_driver_registry",
    ".usb_driver_registry",
    ".test_registry",
    ".hermetic_state_registry",
    ".kconsole_registry",
    ".charge_audit_registry",
    ".limine_requests",
    ".limine_requests_start_marker",
    ".limine_requests_end_marker",
];

/// C-ABI entry points whose callers are assembly, so the symbol has to
/// resolve at link time.
pub const SYMBOL_ALLOWLIST: &[&str] = &[
    "kernel_main",
    "common_exception_handler",
    "isr_iret_frame_corrupt",
];

/// `unsafe fn` methods an allowlisted unsafe trait requires, as (trait's last
/// segment, method). Implementing the trait means writing them.
/// `HermeticState::restore` republishes saved PCR pointers, IST stack
/// pointers and the syscall MSRs.
const METHOD_ALLOWLIST: &[(&str, &str)] = &[("HermeticState", "restore")];

const TRIVIAL_CLONE: &str = "::core::clone::TrivialClone";

/// The feature configurations each crate (by directory) is expanded under;
/// the empty one is default features.
fn configs(dir: &str) -> &'static [&'static [&'static str]] {
    match dir {
        "boot" | "fs" | "ktesting" => &[&[], &["--features", "tests"]],
        "core" | "drivers" | "mm" | "net" | "ring" | "sched" => {
            &[&[], &["--features", "test-hooks"]]
        }
        _ => &[&[]],
    }
}

pub fn run(repo: &Repo) -> Result<Report> {
    let started = Instant::now();
    let ws = Workspace::load(repo)?;
    let crates = ws.kernel_crates()?;
    let channel = toolchain::channel(repo)?;
    let target = std::env::var("RUST_TARGET").unwrap_or_else(|_| "targets/x86_64-slos.json".into());
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "builddir/target".into());
    let ksyms = match std::env::var("SLOPOS_KSYMS_RS") {
        Ok(p) => p,
        Err(_) => {
            let rel = "builddir/kallsyms-dev.rs";
            if !repo.exists(rel) {
                std::fs::create_dir_all(repo.path("builddir"))?;
                std::fs::write(
                    repo.path(rel),
                    "pub static KERNEL_SYMBOLS: &[crate::ksym::KernelSymbol] = &[];\n",
                )?;
            }
            repo.path(rel).to_string_lossy().into_owned()
        }
    };

    let mut report = Report::new();
    let mut stats = Stats::default();
    let mut expanded = 0;
    for p in &crates {
        if EXEMPT.contains(&p.dir.as_str()) || under(&p.dir, "vendor") {
            continue;
        }
        for flags in configs(&p.dir) {
            let label = if flags.is_empty() {
                p.dir.clone()
            } else {
                format!("{} [{}]", p.dir, flags.join(" "))
            };
            let out = toolchain::cargo(&channel)
                .current_dir(&repo.root)
                .args([
                    "rustc",
                    "-Zbuild-std=core,alloc",
                    "-Zbuild-std-features=compiler-builtins-mem",
                    "-Zunstable-options",
                    "--target",
                    &target,
                    "-p",
                    &p.name,
                ])
                .args(*flags)
                .args(["--", "-Zunpretty=expanded"])
                .env("CARGO_TARGET_DIR", &target_dir)
                .env("SLOPOS_KSYMS_RS", &ksyms)
                .output()?;
            let text = String::from_utf8(out.stdout)?;
            if !out.status.success() || text.trim().is_empty() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let why = stderr
                    .lines()
                    .find(|l| l.starts_with("error"))
                    .unwrap_or("no compiler error captured");
                report.finding(
                    label,
                    format!(
                        "expansion failed — {why}; a crate that does not build cannot be held to \
                         the constant, fix the build first"
                    ),
                );
                continue;
            }
            expanded += 1;
            check(&label, &text, &mut report, &mut stats)?;
        }
    }
    if expanded == 0 {
        return Err("no crate expanded — refusing to report OK".into());
    }
    if stats.trivial_clone == 0 {
        return Err(format!(
            "no `unsafe impl {TRIVIAL_CLONE}` in any of {expanded} expansions: the toolchain \
             changed what derives emit, or -Zunpretty=expanded's format moved. Re-derive the \
             compiler-shape filters in tools/xtask/src/lint/unsafe_expansion.rs before trusting \
             this gate again"
        )
        .into());
    }
    report.summary(format!(
        "expanded {expanded} crate configurations of {} kernel crates in {:.1}s: no unsafe beyond \
         the allowlisted traits, sections and symbols ({} derive-emitted shapes recognised)",
        crates.len(),
        started.elapsed().as_secs_f64(),
        stats.trivial_clone + stats.derived_unreachable
    ));
    Ok(report)
}

/// Counts of the compiler-emitted shapes recognised, kept so a run that sees
/// none of them can refuse to pass.
#[derive(Default, Debug)]
struct Stats {
    trivial_clone: usize,
    derived_unreachable: usize,
}

/// Parse one expansion and report every unsafe it carries beyond the
/// constant.
fn check(label: &str, text: &str, report: &mut Report, stats: &mut Stats) -> Result<()> {
    let file = syn::parse_file(text).map_err(|e| {
        let line = e.span().start().line;
        format!(
            "{label}: expansion line {line} does not parse: {e}: `{}`",
            text.lines()
                .nth(line.saturating_sub(1))
                .unwrap_or("")
                .trim()
        )
    })?;
    let mut v = Checker {
        lines: text.lines().collect(),
        label,
        report,
        stats,
        derived: 0,
        allowed_methods: &[],
        handled: HashSet::new(),
    };
    v.visit_file(&file);
    Ok(())
}

struct Checker<'a> {
    lines: Vec<&'a str>,
    label: &'a str,
    report: &'a mut Report,
    stats: &'a mut Stats,
    /// Depth of `#[automatically_derived]` impls enclosing the visit.
    derived: usize,
    /// Methods the enclosing allowlisted `unsafe impl` may declare `unsafe`.
    allowed_methods: &'static [&'static str],
    /// `#[unsafe(no_mangle)]` attributes already judged with their item, by
    /// (line, column).
    handled: HashSet<(usize, usize)>,
}

impl Checker<'_> {
    fn flag(&mut self, span: Span, rule: &str) {
        let line = rust::line(span);
        let text = self
            .lines
            .get(line.wrapping_sub(1))
            .map_or("", |l| l.trim());
        self.report.finding(
            format!("{}: expansion line {line}", self.label),
            format!(
                "{rule}: `{text}` — this is not in the crate's source and forbid(unsafe_code) \
                 cannot see it; route the operation through a safe slopos-ostd API, or, if the \
                 obligation belongs to the invoking crate, add the trait, section or symbol to \
                 the allowlist in tools/xtask/src/lint/unsafe_expansion.rs with a reason"
            ),
        );
    }

    fn tokens(&mut self, tokens: &TokenStream) {
        for span in unsafe_keywords(tokens) {
            self.flag(span, "executable unsafe in macro tokens");
        }
    }

    /// Judge `#[unsafe(no_mangle)]` on a named item.
    fn exported(&mut self, attrs: &[Attribute], name: &syn::Ident) {
        for a in attrs {
            if unsafe_attr_name(a).as_deref() == Some("no_mangle") {
                let start = a.pound_token.span.start();
                self.handled.insert((start.line, start.column));
                if !SYMBOL_ALLOWLIST.contains(&name.to_string().as_str()) {
                    self.flag(
                        a.pound_token.span,
                        &format!("#[unsafe(no_mangle)] on `{name}`, not in SYMBOL_ALLOWLIST"),
                    );
                }
            }
        }
    }
}

/// The inner attribute name of `#[unsafe(name ...)]`.
fn unsafe_attr_name(a: &Attribute) -> Option<String> {
    let Meta::List(l) = &a.meta else { return None };
    if !l.path.is_ident("unsafe") {
        return None;
    }
    let inner: Meta = l.parse_args().ok()?;
    rust::last_ident(inner.path())
}

/// The trait path as written, segments only: `::core::clone::TrivialClone`.
fn path_text(p: &syn::Path) -> String {
    let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
    format!(
        "{}{}",
        if p.leading_colon.is_some() { "::" } else { "" },
        segs.join("::")
    )
}

fn trait_allowed(p: &syn::Path) -> bool {
    let full = path_text(p);
    let last = rust::last_ident(p).unwrap_or_default();
    TRAIT_ALLOWLIST.iter().any(|e| {
        if e.contains("::") {
            *e == full
        } else {
            *e == last
        }
    })
}

/// `unsafe { ::core::intrinsics::unreachable() }`, as `#[derive(PartialEq)]`
/// emits it.
fn is_derived_unreachable(e: &syn::ExprUnsafe) -> bool {
    let [Stmt::Expr(Expr::Call(call), None)] = e.block.stmts.as_slice() else {
        return false;
    };
    call.args.is_empty()
        && matches!(&*call.func, Expr::Path(p)
            if p.qself.is_none() && path_text(&p.path) == "::core::intrinsics::unreachable")
}

impl<'ast> Visit<'ast> for Checker<'_> {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let derived = rust::has_attr(&i.attrs, "automatically_derived");
        let saved = self.allowed_methods;
        self.allowed_methods = &[];
        if let (Some(u), Some((_, path, _))) = (&i.unsafety, &i.trait_) {
            let text = path_text(path);
            if text == TRIVIAL_CLONE && derived {
                self.stats.trivial_clone += 1;
            } else if trait_allowed(path) {
                let last = rust::last_ident(path).unwrap_or_default();
                if let Some((_, m)) = METHOD_ALLOWLIST.iter().find(|(t, _)| *t == last) {
                    self.allowed_methods = std::slice::from_ref(m);
                }
            } else {
                self.flag(
                    u.span,
                    &format!("unsafe impl of `{text}`, not in TRAIT_ALLOWLIST"),
                );
            }
        } else if let Some(u) = &i.unsafety {
            self.flag(u.span, "unsafe inherent impl");
        }
        self.derived += usize::from(derived);
        visit::visit_item_impl(self, i);
        self.derived -= usize::from(derived);
        self.allowed_methods = saved;
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.exported(&f.attrs, &f.sig.ident);
        if let Some(u) = &f.sig.unsafety {
            if !self
                .allowed_methods
                .contains(&f.sig.ident.to_string().as_str())
            {
                self.flag(u.span, "executable unsafe: unsafe fn");
            }
        }
        let saved = self.allowed_methods;
        self.allowed_methods = &[];
        for a in &f.attrs {
            self.visit_attribute(a);
        }
        self.visit_visibility(&f.vis);
        for input in &f.sig.inputs {
            self.visit_fn_arg(input);
        }
        self.visit_return_type(&f.sig.output);
        self.visit_generics(&f.sig.generics);
        self.visit_block(&f.block);
        self.allowed_methods = saved;
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.exported(&f.attrs, &f.sig.ident);
        visit::visit_item_fn(self, f);
    }

    fn visit_item_static(&mut self, s: &'ast syn::ItemStatic) {
        self.exported(&s.attrs, &s.ident);
        visit::visit_item_static(self, s);
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        if let Some(u) = &s.unsafety {
            self.flag(u.span, "executable unsafe: unsafe fn");
        }
        visit::visit_signature(self, s);
    }

    fn visit_expr_unsafe(&mut self, e: &'ast syn::ExprUnsafe) {
        if self.derived > 0 && is_derived_unreachable(e) {
            self.stats.derived_unreachable += 1;
        } else {
            self.flag(e.unsafe_token.span, "executable unsafe: unsafe block");
        }
        visit::visit_expr_unsafe(self, e);
    }

    fn visit_item_foreign_mod(&mut self, i: &'ast syn::ItemForeignMod) {
        if let Some(u) = &i.unsafety {
            self.flag(u.span, "executable unsafe: unsafe extern block");
        }
        visit::visit_item_foreign_mod(self, i);
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        if let Some(u) = &i.unsafety {
            self.flag(u.span, "executable unsafe: unsafe trait");
        }
        visit::visit_item_trait(self, i);
    }

    fn visit_type_bare_fn(&mut self, t: &'ast syn::TypeBareFn) {
        if let Some(u) = &t.unsafety {
            self.flag(u.span, "executable unsafe: unsafe fn pointer type");
        }
        visit::visit_type_bare_fn(self, t);
    }

    fn visit_attribute(&mut self, a: &'ast Attribute) {
        let Meta::List(l) = &a.meta else { return };
        if !l.path.is_ident("unsafe") {
            return;
        }
        let span = a.pound_token.span;
        let Ok(inner) = l.parse_args::<Meta>() else {
            self.flag(span, "unsafe attribute");
            return;
        };
        match rust::last_ident(inner.path()).as_deref() {
            Some("link_section") => {
                let section = match &inner {
                    Meta::NameValue(nv) => match &nv.value {
                        Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => Some(s.value()),
                        _ => None,
                    },
                    _ => None,
                };
                if !section.is_some_and(|s| SECTION_ALLOWLIST.contains(&s.as_str())) {
                    self.flag(
                        span,
                        "#[unsafe(link_section)] of a section not in SECTION_ALLOWLIST",
                    );
                }
            }
            Some("no_mangle") => {
                let start = span.start();
                if !self.handled.contains(&(start.line, start.column)) {
                    self.flag(
                        span,
                        "#[unsafe(no_mangle)] on an item that names no allowlisted symbol",
                    );
                }
            }
            Some("export_name") => {
                let symbol = match &inner {
                    Meta::NameValue(nv) => match &nv.value {
                        Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => Some(s.value()),
                        _ => None,
                    },
                    _ => None,
                };
                if !symbol.is_some_and(|s| SYMBOL_ALLOWLIST.contains(&s.as_str())) {
                    self.flag(
                        span,
                        "#[unsafe(export_name)] of a symbol not in SYMBOL_ALLOWLIST",
                    );
                }
            }
            _ => self.flag(span, "unsafe attribute"),
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(&m.tokens);
        visit::visit_macro(self, m);
    }

    fn visit_item(&mut self, i: &'ast syn::Item) {
        if let syn::Item::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_item(self, i);
    }

    fn visit_impl_item(&mut self, i: &'ast syn::ImplItem) {
        if let syn::ImplItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_impl_item(self, i);
    }

    fn visit_trait_item(&mut self, i: &'ast syn::TraitItem) {
        if let syn::TraitItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_trait_item(self, i);
    }

    fn visit_foreign_item(&mut self, i: &'ast syn::ForeignItem) {
        if let syn::ForeignItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_foreign_item(self, i);
    }

    fn visit_expr(&mut self, e: &'ast Expr) {
        if let Expr::Verbatim(t) = e {
            self.tokens(t);
        }
        visit::visit_expr(self, e);
    }

    fn visit_type(&mut self, t: &'ast syn::Type) {
        if let syn::Type::Verbatim(ts) = t {
            self.tokens(ts);
        }
        visit::visit_type(self, t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings(text: &str) -> (Vec<String>, Stats) {
        let mut report = Report::new();
        let mut stats = Stats::default();
        check("mm", text, &mut report, &mut stats).unwrap();
        (
            report.findings.iter().map(|f| f.location.clone()).collect(),
            stats,
        )
    }

    fn lines(hits: &[String]) -> Vec<usize> {
        hits.iter()
            .map(|h| h.rsplit(' ').next().unwrap().parse().unwrap())
            .collect()
    }

    /// The golden fixture: the pinned toolchain's expansion of the two
    /// derives that emit `unsafe`. Fails if `-Zunpretty=expanded` stops
    /// emitting exactly the shapes the filters recognise.
    #[test]
    fn the_compiler_emitted_shapes_have_not_moved() {
        let repo = Repo::at(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        let channel = toolchain::channel(&repo).unwrap();
        let mut child = toolchain::rustc(&channel)
            .current_dir(&repo.root)
            .args([
                "--edition",
                "2024",
                "--crate-type",
                "lib",
                "--crate-name",
                "golden",
                "-Zunpretty=expanded",
                "-",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            child.stdin.as_mut().unwrap(),
            b"#![no_std]\n\
              #[derive(Clone, Copy)]\npub struct P { a: u8 }\n\
              #[derive(PartialEq)]\npub enum E { A(u8), B(u16) }\n",
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        let (hits, stats) = findings(&text);
        assert!(hits.is_empty(), "{hits:?}\n{text}");
        assert_eq!(
            (stats.trivial_clone, stats.derived_unreachable),
            (1, 1),
            "the derive expansion moved:\n{text}"
        );
    }

    #[test]
    fn allowlisted_impls_sections_and_symbols_pass() {
        let text = concat!(
            "unsafe impl ::slopos_ostd::Zeroable for A where u8: Copy {}\n",
            "unsafe impl crate::Pod for A {}\n",
            "unsafe impl ::slopos_ostd::test_support::hermetic::HermeticState for H {\n",
            "    unsafe fn restore(s: Self::Snapshot) {}\n",
            "}\n",
            "unsafe impl ::slopos_ostd::task::PcrStackTy for S {}\n",
            "#[unsafe(link_section = \".test_registry\")]\n",
            "static T: u8 = 0;\n",
            "#[unsafe(no_mangle)]\n",
            "pub extern \"C\" fn kernel_main() {}\n",
            "#[unsafe(export_name = \"common_exception_handler\")]\n",
            "fn h() {}\n",
            "/// unsafe { in a doc comment }\n",
            "fn s() { let _ = \"unsafe { x }\"; }\n",
            "#[automatically_derived]\n",
            "unsafe impl ::core::clone::TrivialClone for A { }\n",
        );
        let (hits, stats) = findings(text);
        assert!(hits.is_empty(), "{hits:?}");
        assert_eq!(stats.trivial_clone, 1);
    }

    #[test]
    fn everything_else_unsafe_is_flagged() {
        let text = concat!(
            "fn a() { unsafe { x() } }\n",
            "unsafe fn b() {}\n",
            "unsafe extern \"C\" { fn c(); }\n",
            "unsafe trait D {}\n",
            "unsafe impl Send for E {}\n",
            "unsafe impl ::other::Pod for E {}\n",
            "#[unsafe(link_section = \".text.evil\")]\n",
            "static F: u8 = 0;\n",
            "#[unsafe(no_mangle)]\n",
            "fn not_listed() {}\n",
            "impl G {\n",
            "    unsafe fn restore(&self) {}\n",
            "}\n",
            "fn h() { let _ = unsafe { ::core::intrinsics::unreachable() }; }\n",
            "fn i() { format_args!(\"{0}\", unsafe { y() }); }\n",
            "static J: Option<unsafe fn()> = None;\n",
            "#[unsafe(naked)]\n",
            "fn k() {}\n",
        );
        let (hits, _) = findings(text);
        assert_eq!(lines(&hits), [1, 2, 3, 4, 5, 6, 7, 9, 12, 14, 15, 16, 17]);
    }
}
