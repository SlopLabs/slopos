//! No kernel crate contains `async`.
//!
//! SlopOS is a sync-core, async-edge framekernel: the kernel — OSTD and every
//! service crate, the io_uring-style `ring/` crate included — is synchronous,
//! and async lives entirely in userspace on top of the ring surface (AD-8 /
//! AD-9). There is no `async`-forbidding lint to pair with
//! `forbid(unsafe_code)`, so this gate holds the line: no `async fn`, `async`
//! block, `async move` block, `async` closure or `async gen` construct
//! outside the userland-side crates, the verification proofs and the named
//! TCB annexes. An item under `#[cfg(...)]` compiles out of the kernel build
//! and is skipped whole.

use std::collections::BTreeMap;

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Expr, ForeignItem, ImplItem, Item, Stmt, TraitItem};

use super::TCB_ANNEXES;
use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{
    self, Source, foreign_item_gated, impl_item_gated, item_gated, stmt_gated, trait_item_gated,
};

/// Userland-side crates, whose job is to host async, and the proofs.
const OUT_OF_SCOPE: &[&str] = &[
    "userland",
    "slibc",
    "slop-protocol",
    "appkit",
    "editor-core",
    "http-core",
    "tls-core",
    "image",
    "slopos-rt",
    "verification",
];

pub fn run(repo: &Repo) -> Result<Report> {
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
    let mut report = Report::new();
    for f in &files {
        scan_source(&Source::load(repo, f)?, &mut report);
    }
    report.summary(format!(
        "scanned {} files outside the userland-side crates and the TCB annexes",
        files.len()
    ));
    Ok(report)
}

fn in_scope(path: &str) -> bool {
    !OUT_OF_SCOPE.iter().any(|d| under(path, d)) && !TCB_ANNEXES.iter().any(|d| under(path, d))
}

fn scan_source(src: &Source, report: &mut Report) {
    let mut v = AsyncVisitor::default();
    v.visit_file(&src.file);
    for (line, what) in v.hits {
        report.at(
            &src.path,
            line,
            format!(
                "{what} in a kernel crate — SlopOS is sync core, async edge (AD-8 / AD-9): no \
                 kernel crate may contain async; move it to userland on top of the ring surface"
            ),
        );
    }
}

/// The async constructs of a file, one per line.
#[derive(Default)]
struct AsyncVisitor {
    hits: BTreeMap<usize, &'static str>,
}

impl AsyncVisitor {
    fn hit(&mut self, span: Span, what: &'static str) {
        self.hits.entry(rust::line(span)).or_insert(what);
    }

    fn tokens(&mut self, tokens: &TokenStream) {
        for span in async_keywords(tokens) {
            self.hit(span, "`async` in macro tokens");
        }
    }
}

impl<'ast> Visit<'ast> for AsyncVisitor {
    fn visit_signature(&mut self, s: &'ast syn::Signature) {
        if let Some(a) = &s.asyncness {
            self.hit(a.span, "`async fn`");
        }
        visit::visit_signature(self, s);
    }

    fn visit_expr_async(&mut self, e: &'ast syn::ExprAsync) {
        self.hit(e.async_token.span, "`async` block");
        visit::visit_expr_async(self, e);
    }

    fn visit_expr_closure(&mut self, e: &'ast syn::ExprClosure) {
        if let Some(a) = &e.asyncness {
            self.hit(a.span, "`async` closure");
        }
        visit::visit_expr_closure(self, e);
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

/// The `async` keywords in a token stream that open an async construct:
/// followed by `fn`, `move`, `gen`, a brace-delimited block, or a closure's
/// `|`. A bare `async` (a raw identifier, a word in a path) is not one.
fn async_keywords(tokens: &TokenStream) -> Vec<Span> {
    let mut out = Vec::new();
    collect_async(tokens, &mut out);
    out
}

fn collect_async(tokens: &TokenStream, out: &mut Vec<Span>) {
    let tts: Vec<TokenTree> = tokens.clone().into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        match tt {
            TokenTree::Ident(id) if id == "async" => {
                let opens = match tts.get(i + 1) {
                    Some(TokenTree::Ident(next)) => next == "fn" || next == "move" || next == "gen",
                    Some(TokenTree::Group(g)) => g.delimiter() == Delimiter::Brace,
                    Some(TokenTree::Punct(p)) => p.as_char() == '|',
                    _ => false,
                };
                if opens {
                    out.push(id.span());
                }
            }
            TokenTree::Group(g) => collect_async(&g.stream(), out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn every_async_construct_is_flagged() {
        let positives = concat!(
            "async fn a() {}\n",
            "pub async fn b() {}\n",
            "fn c() { let _f = async move { 1 }; }\n",
            "fn d() { let _g = async { 1 }; }\n",
            "fn e() { let _h = async || {}; }\n",
        );
        let hits = findings(&[
            ("sched/src/positives.rs", positives),
            ("vendor/othercrate/src/lib.rs", "async fn vendored() {}\n"),
        ]);
        assert_eq!(
            hits,
            [
                "sched/src/positives.rs:1",
                "sched/src/positives.rs:2",
                "sched/src/positives.rs:3",
                "sched/src/positives.rs:4",
                "sched/src/positives.rs:5",
                "vendor/othercrate/src/lib.rs:1",
            ]
        );
    }

    /// `async gen fn` is not syntax syn parses (gen blocks are unstable), so
    /// the file fails to parse and the gate errors rather than passes; inside
    /// macro tokens it is flagged like the other forms.
    #[test]
    fn async_gen_fails_closed() {
        assert!(Source::parse("sched/src/gen.rs", "async gen fn f() {}\n".into()).is_err());
        assert_eq!(
            findings(&[("sched/src/gen.rs", "m! { async gen fn f() {} }\n")]),
            ["sched/src/gen.rs:1"]
        );
    }

    #[test]
    fn accepted_forms_and_exempt_paths_stay_silent() {
        let negatives = concat!(
            "// async fn commented() {}\n",
            "/* async fn block_commented() {}\n",
            " * async fn continuation() {} */\n",
            "fn asyncness_probe() { if sig.asyncness.is_some() { } }\n",
            "fn message() { klog_info!(\"no `async` here\"); }\n",
            "fn ident() { let asynchronous = 1; let _ = asynchronous; }\n",
            "#[cfg(feature = \"host\")]\n",
            "async fn cfg_gated() {}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    async fn inside_gated_mod() {}\n",
            "    fn deeper() { let _ = async { 1 }; }\n",
            "}\n",
            "fn s() { let _ = \"async fn in a string\"; let _ = r#async; }\n",
        );
        let hits = findings(&[
            ("sched/src/negatives.rs", negatives),
            (
                "userland/src/lib.rs",
                "async fn userland_is_the_point() {}\n",
            ),
            ("verification/proofs/x.rs", "async fn proof_harness() {}\n"),
            ("vendor/gimli/src/x.rs", "async fn annex() {}\n"),
        ]);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn async_in_impls_traits_and_macros_is_flagged() {
        let src = concat!(
            "impl X {\n",
            "    async fn m(&self) {}\n",
            "}\n",
            "trait T {\n",
            "    async fn t(&self);\n",
            "}\n",
            "macro_rules! spawn { () => { async move { 1 } }; }\n",
            "fn f() { g(async move || {}); }\n",
        );
        assert_eq!(
            findings(&[("ring/src/a.rs", src)]),
            [
                "ring/src/a.rs:2",
                "ring/src/a.rs:5",
                "ring/src/a.rs:7",
                "ring/src/a.rs:8"
            ]
        );
    }
}
