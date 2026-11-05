//! No kernel crate other than slopos-ostd writes `unsafe`.
//!
//! slopos-ostd is the kernel's trusted domain: it owns every line of
//! first-party `unsafe`, and every other crate the kernel links compiles under
//! `#![forbid(unsafe_code)]`. `forbid` already rejects new `unsafe` at rustc
//! time; this gate is the belt to its braces. It catches an
//! `#[allow(unsafe_code)]` exemption slipping in, `unsafe` written into a
//! macro body (which `forbid` cannot see at the call site), and a crate newly
//! linked into the kernel that never acquired the lint attribute.
//!
//! Two halves:
//! - no `unsafe` block, `unsafe fn`, `unsafe impl`, `unsafe trait`,
//!   `unsafe extern` block, `unsafe fn` pointer type, or `unsafe` keyword in
//!   macro tokens, outside slopos-ostd, the named TCB annexes
//!   (vendor/unwinding, vendor/gimli, a pinned unwinder and its DWARF reader
//!   reached only through OSTD) and the userland-side crates and build tools,
//!   which link into nothing the kernel does. The edition-2024 attribute form
//!   `#[unsafe(...)]` is not an unsafe block and is allowed. An item under
//!   `#[cfg(...)]` compiles out of the kernel build and is skipped whole.
//! - every crate in the `kernel` binary's dependency closure carries
//!   `#![forbid(unsafe_code)]` at its crate root, save the crates that
//!   legitimately author unsafe.

use std::collections::BTreeMap;

use proc_macro2::{Span, TokenStream};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ForeignItem, ImplItem, Item, Meta, Stmt, TraitItem};

use super::TCB_ANNEXES;
use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{
    self, Source, foreign_item_gated, impl_item_gated, item_gated, stmt_gated, trait_item_gated,
    unsafe_keywords,
};
use crate::workspace::{Package, Workspace};

/// Userland-side crates, the trusted core and its proc-macro support, and
/// the build tools. slibc-core's integration tests build slibc's memory
/// routines for the host and so drive raw pointers; `ktesting` is not here,
/// since it ships in kernel.elf.
const OUT_OF_SCOPE: &[&str] = &[
    "userland",
    "slibc",
    "slibc-core/tests",
    "cabi",
    "slop-protocol",
    "appkit",
    "editor-core",
    "http-core",
    "tls-core",
    "image",
    "slopos-rt",
    "slopos-ostd",
    "slopos-ostd-derive",
    "tools",
];

/// Files allowed to write `unsafe`, by exact repo-relative path. The kernel
/// binary's root declares the global allocator and alloc error handler and
/// the base-interface ELF note's `#[unsafe(link_section)]` static.
const SOURCE_ALLOWLIST: &[&str] = &["kernel/src/main.rs"];

/// Crate directories that legitimately author unsafe and so need not forbid
/// it: the OSTD itself, the proc macro emitting `unsafe impl` token text
/// consumed inside it, and the kernel binary (global allocator).
const LINT_EXEMPT: &[&str] = &["slopos-ostd", "slopos-ostd-derive", "kernel"];

pub fn run(repo: &Repo) -> Result<Report> {
    let mut report = Report::new();
    let all = repo.rs_files()?;
    if all.is_empty() {
        return Err(format!(
            "no Rust sources found under {} — the scan would be a no-op",
            repo.root.display()
        )
        .into());
    }
    let files: Vec<&String> = all.iter().filter(|f| in_scope(f)).collect();
    if files.is_empty() {
        return Err("every Rust source is exempt — the scan would be a no-op".into());
    }
    for f in &files {
        scan_source(&Source::load(repo, f)?, &mut report);
    }
    let ws = Workspace::load(repo)?;
    let crates = ws.kernel_crates()?;
    let checked = scan_crate_lints(repo, &crates, &mut report)?;
    report.summary(format!(
        "scanned {} files outside slopos-ostd and the TCB annexes; {checked} of {} kernel crates \
         checked for #![forbid(unsafe_code)] (the rest exempt)",
        files.len(),
        crates.len()
    ));
    Ok(report)
}

fn in_scope(path: &str) -> bool {
    !OUT_OF_SCOPE.iter().any(|d| under(path, d))
        && !TCB_ANNEXES.iter().any(|d| under(path, d))
        && !SOURCE_ALLOWLIST.contains(&path)
}

fn scan_source(src: &Source, report: &mut Report) {
    let mut v = UnsafeVisitor::default();
    v.visit_file(&src.file);
    for (line, what) in v.hits {
        report.at(
            &src.path,
            line,
            format!(
                "{what} outside slopos-ostd and the named TCB annexes — route the operation through \
                 a safe slopos-ostd API; only an audited file may be added to SOURCE_ALLOWLIST in \
                 tools/xtask/src/lint/unsafe_outside_ostd.rs"
            ),
        );
    }
}

/// Collects the `unsafe` keywords of a file, one per line.
#[derive(Default)]
struct UnsafeVisitor {
    hits: BTreeMap<usize, &'static str>,
}

impl UnsafeVisitor {
    fn hit(&mut self, span: Span, what: &'static str) {
        self.hits.entry(rust::line(span)).or_insert(what);
    }

    fn tokens(&mut self, tokens: &TokenStream) {
        for span in unsafe_keywords(tokens) {
            self.hit(span, "`unsafe` in macro tokens");
        }
    }
}

impl<'ast> Visit<'ast> for UnsafeVisitor {
    fn visit_expr_unsafe(&mut self, e: &'ast syn::ExprUnsafe) {
        self.hit(e.unsafe_token.span, "`unsafe` block");
        visit::visit_expr_unsafe(self, e);
    }

    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        if let Some(u) = &s.unsafety {
            self.hit(u.span, "`unsafe fn`");
        }
        visit::visit_signature(self, s);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if let Some(u) = &i.unsafety {
            self.hit(u.span, "`unsafe impl`");
        }
        visit::visit_item_impl(self, i);
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        if let Some(u) = &i.unsafety {
            self.hit(u.span, "`unsafe trait`");
        }
        visit::visit_item_trait(self, i);
    }

    fn visit_item_foreign_mod(&mut self, i: &'ast syn::ItemForeignMod) {
        if let Some(u) = &i.unsafety {
            self.hit(u.span, "`unsafe extern` block");
        }
        visit::visit_item_foreign_mod(self, i);
    }

    fn visit_type_bare_fn(&mut self, t: &'ast syn::TypeBareFn) {
        if let Some(u) = &t.unsafety {
            self.hit(u.span, "`unsafe fn` pointer type");
        }
        visit::visit_type_bare_fn(self, t);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(&m.tokens);
        visit::visit_macro(self, m);
    }

    fn visit_item(&mut self, i: &'ast Item) {
        if item_gated(i) {
            return;
        }
        if let Item::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_item(self, i);
    }

    fn visit_impl_item(&mut self, i: &'ast ImplItem) {
        if impl_item_gated(i) {
            return;
        }
        if let ImplItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_impl_item(self, i);
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        if trait_item_gated(i) {
            return;
        }
        if let TraitItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_trait_item(self, i);
    }

    fn visit_foreign_item(&mut self, i: &'ast ForeignItem) {
        if foreign_item_gated(i) {
            return;
        }
        if let ForeignItem::Verbatim(t) = i {
            self.tokens(t);
        }
        visit::visit_foreign_item(self, i);
    }

    fn visit_stmt(&mut self, s: &'ast Stmt) {
        if !stmt_gated(s) {
            visit::visit_stmt(self, s);
        }
    }

    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        if !rust::is_cfg_gated(&a.attrs) {
            visit::visit_arm(self, a);
        }
    }

    fn visit_expr(&mut self, e: &'ast Expr) {
        if let Expr::Verbatim(t) = e {
            self.tokens(t);
        }
        visit::visit_expr(self, e);
    }
}

/// Check each non-exempt kernel crate's root for `#![forbid(unsafe_code)]`;
/// returns how many crates were checked.
fn scan_crate_lints(repo: &Repo, crates: &[&Package], report: &mut Report) -> Result<usize> {
    let mut checked = 0;
    for p in crates {
        if LINT_EXEMPT.contains(&p.dir.as_str()) || TCB_ANNEXES.iter().any(|a| p.dir == *a) {
            continue;
        }
        checked += 1;
        let Some(root) = p.crate_root().filter(|r| repo.exists(r)) else {
            report.finding(
                p.dir.as_str(),
                "kernel crate has no crate root (no lib or bin target source) — every crate the \
                 kernel binary links must carry #![forbid(unsafe_code)] at its crate root",
            );
            continue;
        };
        let src = Source::load(repo, root)?;
        if !forbids_unsafe_code(&src.file.attrs) {
            report.at(
                root,
                1,
                format!(
                    "kernel crate `{}` lacks #![forbid(unsafe_code)] — every crate the kernel \
                     binary links must carry it at its crate root",
                    p.name
                ),
            );
        }
    }
    Ok(checked)
}

fn forbids_unsafe_code(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        matches!(a.style, syn::AttrStyle::Inner(_))
            && a.path().is_ident("forbid")
            && matches!(&a.meta, Meta::List(l) if l
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
                )
                .is_ok_and(|lints| lints.iter().any(|l| l.is_ident("unsafe_code"))))
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::fixture::Tree;
    use crate::workspace::tests::metadata;

    fn findings(files: &[(&str, &str)]) -> Vec<String> {
        let mut report = Report::new();
        for (path, text) in files {
            if in_scope(path) {
                scan_source(&Source::parse(path, text.to_string()).unwrap(), &mut report);
            }
        }
        report.findings.iter().map(|f| f.location.clone()).collect()
    }

    #[test]
    fn every_unsafe_form_is_flagged() {
        let positives = "fn raw() { unsafe { core::ptr::read(p) } }\n\
                         unsafe fn danger() {}\n\
                         unsafe impl Send for Foo {}\n\
                         unsafe extern \"C\" { fn ext(); }\n\
                         fn block() { let x = unsafe { *p }; }\n\
                         fn deref_assign() {\n\
                         \x20   *out = unsafe { *src };\n\
                         }\n";
        let hits = findings(&[
            ("sched/src/positives.rs", positives),
            ("vendor/othercrate/src/lib.rs", "unsafe fn vendored() {}\n"),
        ]);
        assert_eq!(
            hits,
            [
                "sched/src/positives.rs:1",
                "sched/src/positives.rs:2",
                "sched/src/positives.rs:3",
                "sched/src/positives.rs:4",
                "sched/src/positives.rs:5",
                "sched/src/positives.rs:7",
                "vendor/othercrate/src/lib.rs:1",
            ]
        );
    }

    #[test]
    fn accepted_forms_and_exempt_paths_stay_silent() {
        let negatives = "// unsafe { never_executed() }\n\
                         /* unsafe fn block_commented() {} */\n\
                         fn ident() { let unsafe_count = 0; let _ = unsafe_count; }\n\
                         fn make_unsafe_handle() {}\n\
                         #[unsafe(link_section = \".driver_registry\")]\n\
                         static E: u8 = 0;\n\
                         fn trailing() { let v = 1; } // unsafe\n\
                         #[cfg(test)]\n\
                         unsafe fn cfg_gated() {}\n\
                         #[cfg(feature = \"host\")]\n\
                         mod gated {\n\
                         \x20   unsafe fn inside() {}\n\
                         }\n";
        let hits = findings(&[
            ("sched/src/negatives.rs", negatives),
            (
                "kernel/src/main.rs",
                "#[global_allocator]\nstatic A: KernelHeap = KernelHeap;\nunsafe fn allocator_support() {}\n",
            ),
            (
                "slopos-ostd/src/x.rs",
                "unsafe fn the_trusted_core_owns_this() {}\n",
            ),
            (
                "userland/src/main.rs",
                "unsafe fn userland_is_out_of_scope() {}\n",
            ),
            (
                "vendor/unwinding/src/lib.rs",
                "unsafe fn named_annex() {}\n",
            ),
            ("tools/xtask/src/x.rs", "unsafe fn build_tool() {}\n"),
        ]);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn the_source_allowlist_is_a_whole_path_match() {
        let hits = findings(&[
            ("sched/src/main.rs", "unsafe fn f() {}\n"),
            ("kernel/src/main.rs.bak/x.rs", "unsafe fn f() {}\n"),
        ]);
        assert_eq!(
            hits,
            ["sched/src/main.rs:1", "kernel/src/main.rs.bak/x.rs:1"]
        );
    }

    #[test]
    fn literals_comments_and_cfg_gated_subtrees_do_not_fool_it() {
        let src = "fn a() { let s = \"unsafe { x }\"; let c = '}'; }\n\
                   /* multi-line\n   unsafe fn hidden() {}\n*/\n\
                   /// unsafe in a doc comment\n\
                   fn b() {}\n\
                   #[cfg(test)]\n\
                   mod tests {\n\
                   \x20   fn deep() {\n\
                   \x20       unsafe { x() }\n\
                   \x20   }\n\
                   }\n\
                   fn c() {\n\
                   \x20   #[cfg(feature = \"x\")]\n\
                   \x20   let y = unsafe { z() };\n\
                   }\n";
        assert!(findings(&[("mm/src/a.rs", src)]).is_empty());
    }

    #[test]
    fn unsafe_written_into_macros_and_types_is_flagged() {
        let src = "macro_rules! m {\n\
                   \x20   ($x:expr) => {\n\
                   \x20       unsafe { $x }\n\
                   \x20   };\n\
                   }\n\
                   static F: Option<unsafe extern \"C\" fn()> = None;\n\
                   m! { #[unsafe(no_mangle)] fn ok() {} }\n\
                   impl X {\n\
                   \x20   unsafe fn method(&self) {}\n\
                   }\n";
        assert_eq!(
            findings(&[("mm/src/m.rs", src)]),
            ["mm/src/m.rs:3", "mm/src/m.rs:6", "mm/src/m.rs:9"]
        );
    }

    #[test]
    fn every_kernel_crate_must_forbid_unsafe_code() {
        let tree = Tree::new(&[
            ("mm/src/lib.rs", "#![forbid(unsafe_code)]\n"),
            ("sched/src/lib.rs", "pub fn f() {}\n"),
            ("slopos-ostd/src/lib.rs", ""),
            ("kernel/src/lib.rs", ""),
            ("vendor/unwinding/src/lib.rs", ""),
            (
                "fs/src/lib.rs",
                "#![no_std]\n#![forbid(missing_docs, unsafe_code)]\n",
            ),
            (
                "net/src/lib.rs",
                "#![cfg_attr(test, forbid(unsafe_code))]\n",
            ),
        ]);
        let json = metadata(&[
            ("mm", "mm", &[]),
            ("sched", "sched", &[]),
            ("newcrate", "newcrate", &[]),
            ("slopos-ostd", "slopos-ostd", &[]),
            ("kernel", "kernel", &[]),
            ("unwinding", "vendor/unwinding", &[]),
            ("fs", "fs", &[]),
            ("net", "net", &[]),
        ]);
        let ws = Workspace::from_metadata(&json, Path::new("/ws")).unwrap();
        let crates: Vec<&Package> = ws.packages.iter().collect();
        let mut report = Report::new();
        let checked = scan_crate_lints(&tree.repo, &crates, &mut report).unwrap();
        assert_eq!(checked, 5);
        let locs: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        assert_eq!(locs, ["net/src/lib.rs:1", "newcrate", "sched/src/lib.rs:1"]);
    }
}
