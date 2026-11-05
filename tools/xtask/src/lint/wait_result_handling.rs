//! No kernel code collapses a wait's result with a one-token silencer.
//!
//! Every blocking `wait_event*` entry point returns `WaitResult<R>`, and a
//! `Result` is `#[must_use]` under `warnings = "deny"`, so discarding one as a
//! bare statement is already a build error. The compiler cannot see the
//! silencers, though: `let _ =`, `.ok()` and `.unwrap_or*()` each turn the
//! distinct ways a wait can end into one, and the one they erase is
//! `WaitAbort::Killed`. A site that erases it is a site where a dying task
//! keeps waiting.
//!
//! Three checks, in the kernel directories, test sources aside for the first
//! two:
//! - a wait result bound to `_` (`let _ = q.wait_event(..)`, `_ = ..`);
//! - `.ok()`, `.unwrap_or(..)`, `.unwrap_or_default()` or
//!   `.unwrap_or_else(..)` chained onto a wait result (a silencer inside the
//!   predicate is the predicate's own default, not the wait's);
//! - an `allow`/`expect` of `unused_must_use`, which would switch the
//!   compiler's half of this off.
//!
//! Stated so it is not mistaken for more: a written-out `match` that folds
//! the abort variants together is not flagged. Some sites genuinely collapse
//! them (a driver ring quarantines its chain the same way on a timeout as on a
//! kill), and a gate that forbade it would only be routed around.
//!
//! The entry points are the `wait_event*` functions slopos-ostd declares
//! returning `WaitResult`, read from its sources on every run.

use syn::Expr;
use syn::visit::{self, Visit};

use super::wait_api::EntryPoints;
use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source, macro_exprs};

/// The kernel directories this gate scans. `userland/` has no kernel wait
/// queues.
const KERNEL_DIRS: &[&str] = &[
    "abi",
    "acpi",
    "boot",
    "core",
    "drivers",
    "font",
    "fs",
    "gfx",
    "hermetic",
    "keymap-core",
    "kernel-services",
    "ktesting",
    "mm",
    "net",
    "pidfd",
    "ring",
    "sched",
    "service-core",
    "signalfd",
    "slopos-ostd",
    "slopos-ostd-derive",
    "video",
    "vt",
];

const SILENCERS: &[&str] = &["ok", "unwrap_or", "unwrap_or_default", "unwrap_or_else"];

/// Test sources are out of scope for the discard and silencer checks: a
/// test asserting on a wait's outcome is not a dying task.
fn is_test_source(path: &str) -> bool {
    path.ends_with("_tests.rs") || path.contains("/tests/") || path.ends_with("/tests.rs")
}

pub fn run(repo: &Repo) -> Result<Report> {
    let files = repo.rs_files_under(KERNEL_DIRS)?;
    if files.is_empty() {
        return Err(format!(
            "no Rust sources under any of the kernel directories ({}) in {} — the scan would be \
             a no-op",
            KERNEL_DIRS.join(" "),
            repo.root.display()
        )
        .into());
    }
    let entries = EntryPoints::load(repo)?;
    let mut report = Report::new();
    let mut waits = 0;
    for path in &files {
        let src = Source::load(repo, path)?;
        let mut v = Scan {
            entries: &entries,
            path,
            silencers: !is_test_source(path),
            report: &mut report,
            waits: 0,
        };
        v.visit_file(&src.file);
        waits += v.waits;
    }
    let dirs = KERNEL_DIRS.iter().filter(|d| repo.exists(d)).count();
    report.summary(format!(
        "scanned {} files in {dirs} kernel directories: {waits} calls to {} wait entry points, \
         none discarded or collapsed",
        files.len(),
        entries.len()
    ));
    Ok(report)
}

struct Scan<'a> {
    entries: &'a EntryPoints,
    path: &'a str,
    silencers: bool,
    report: &'a mut Report,
    waits: usize,
}

impl Scan<'_> {
    /// The wait call whose result `e` evaluates to, through the method calls
    /// chained onto it. Stops at `?` (the result was propagated) and at a
    /// silencer (already reported on its own).
    fn waited<'e>(&self, e: &'e Expr) -> Option<&'e syn::ExprMethodCall> {
        match e {
            Expr::MethodCall(mc) if self.entries.contains(&mc.method.to_string()) => Some(mc),
            Expr::MethodCall(mc) if SILENCERS.contains(&mc.method.to_string().as_str()) => None,
            Expr::MethodCall(mc) => self.waited(&mc.receiver),
            Expr::Paren(p) => self.waited(&p.expr),
            Expr::Group(g) => self.waited(&g.expr),
            _ => None,
        }
    }

    fn discarded(&mut self, init: &Expr) {
        if !self.silencers {
            return;
        }
        if let Some(wait) = self.waited(init) {
            self.report.at(
                self.path,
                rust::line(wait.method.span()),
                format!(
                    "the result of `{}` is bound to `_`, which erases WaitAbort::Killed — branch \
                     on the variant (`match`, `?`) so a dying task stops waiting",
                    wait.method
                ),
            );
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_local(&mut self, local: &'ast syn::Local) {
        let wild = match &local.pat {
            syn::Pat::Wild(_) => true,
            syn::Pat::Type(t) => matches!(t.pat.as_ref(), syn::Pat::Wild(_)),
            _ => false,
        };
        if wild && let Some(init) = &local.init {
            self.discarded(&init.expr);
        }
        visit::visit_local(self, local);
    }

    fn visit_expr_assign(&mut self, a: &'ast syn::ExprAssign) {
        if matches!(a.left.as_ref(), Expr::Infer(_)) {
            self.discarded(&a.right);
        }
        visit::visit_expr_assign(self, a);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        let method = mc.method.to_string();
        if self.entries.contains(&method) {
            self.waits += 1;
        }
        if self.silencers
            && SILENCERS.contains(&method.as_str())
            && let Some(wait) = self.waited(&mc.receiver)
        {
            self.report.at(
                self.path,
                rust::line(wait.method.span()),
                format!(
                    "`.{method}()` collapses the result of `{}` and erases WaitAbort::Killed — \
                     branch on the variant (`match`, `?`) so a dying task stops waiting",
                    wait.method
                ),
            );
        }
        visit::visit_expr_method_call(self, mc);
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        let silences = |list: &syn::MetaList| {
            list.tokens
                .clone()
                .into_iter()
                .any(|t| matches!(t, proc_macro2::TokenTree::Ident(i) if i == "unused_must_use"))
        };
        let hit = match &attr.meta {
            syn::Meta::List(list)
                if list.path.is_ident("allow") || list.path.is_ident("expect") =>
            {
                silences(list)
            }
            syn::Meta::List(list) if list.path.is_ident("cfg_attr") => {
                let text = list.tokens.to_string();
                (text.contains("allow") || text.contains("expect")) && silences_nested(&list.tokens)
            }
            _ => false,
        };
        if hit {
            self.report.at(
                self.path,
                rust::line(attr.pound_token.span),
                "`unused_must_use` is silenced, switching off the compiler's half of \
                 wait-result handling — remove the attribute and handle the result",
            );
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for e in macro_exprs(mac) {
            self.visit_expr(&e);
        }
    }
}

fn silences_nested(tokens: &proc_macro2::TokenStream) -> bool {
    tokens.clone().into_iter().any(|t| match t {
        proc_macro2::TokenTree::Ident(i) => i == "unused_must_use",
        proc_macro2::TokenTree::Group(g) => silences_nested(&g.stream()),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;
    use crate::lint::wait_api::tests::api;

    fn findings(files: &[(&str, &str)]) -> Vec<String> {
        let (api_path, api_text) = api();
        let mut all = vec![(api_path, api_text.as_str())];
        all.extend_from_slice(files);
        let tree = Tree::new(&all);
        let report = run(&tree.repo).unwrap();
        let mut out: Vec<String> = report
            .findings
            .iter()
            .map(|f| format!("{}: {}", f.location, f.message))
            .collect();
        out.sort();
        out
    }

    fn count(found: &[String], needle: &str) -> usize {
        found.iter().filter(|f| f.contains(needle)).count()
    }

    #[test]
    fn script_fixtures_fire_at_the_same_counts() {
        let found = findings(&[
            (
                "net/src/discards.rs",
                "fn same_line() {\n    let _ = q.wait_event(|| true);\n}\n\
                 fn multi_line() {\n    let _ = BUS\n        .subscribe(ev)\n        .wait_event(|| true);\n}\n",
            ),
            (
                "net/src/silencers.rs",
                "fn a() { let x = q.wait_event(|| true).ok(); }\n\
                 fn b() { let x = q.wait_event_timeout(|| true, 1).unwrap_or(()); }\n\
                 fn c() { let x = q.wait_event_until(|| None).unwrap_or_default(); }\n\
                 fn d() { let x = q.wait_event_interruptible(|| true).unwrap_or_else(|_| ()); }\n",
            ),
            // An inner attribute must open the file, so the script's
            // mid-file `#![expect]` moves to the top.
            (
                "net/src/escape.rs",
                "#![expect(unused_must_use)]\n#[allow(unused_must_use)]\nfn a() {}\n",
            ),
            ("net/src/negatives.rs", NEGATIVES),
            (
                "net/src/socket_tests.rs",
                "fn t() { let _ = q.wait_event(|| true).ok(); }\n",
            ),
            (
                "core/src/tests/bus_tests.rs",
                "fn t() { let _ = q.wait_event(|| true).ok(); }\n",
            ),
            (
                "userland/src/x.rs",
                "fn outside_the_kernel_dirs() { let _ = q.wait_event(|| true).ok(); }\n",
            ),
        ]);
        assert_eq!(count(&found, "bound to `_`"), 2, "{found:#?}");
        assert!(
            found
                .iter()
                .any(|f| f.starts_with("net/src/discards.rs:2:"))
        );
        assert!(
            found
                .iter()
                .any(|f| f.starts_with("net/src/discards.rs:7:"))
        );
        assert_eq!(count(&found, "collapses the result"), 4, "{found:#?}");
        assert_eq!(
            count(&found, "`unused_must_use` is silenced"),
            2,
            "{found:#?}"
        );
        assert!(
            !found.iter().any(|f| f.starts_with("net/src/negatives.rs")
                || f.contains("_tests.rs")
                || f.contains("/tests/")
                || f.starts_with("userland/")),
            "{found:#?}"
        );
        assert_eq!(found.len(), 8, "{found:#?}");
    }

    const NEGATIVES: &str = r#"
fn branches_on_the_variant() {
    match q.wait_event(|| true) {
        Ok(()) => {}
        Err(WaitAbort::Killed) => return,
        Err(_) => {}
    }
}
fn propagates() -> WaitResult<()> {
    q.wait_event(|| true)?;
    Ok(())
}
fn tests_the_outcome() {
    if q.wait_event(|| true).is_err() {
        return;
    }
}
fn a_terminated_discard_far_above_is_not_ours() {
    let _ = unrelated();
    // 1
    // 2
    // 3
    // 4
    // 5
    // 6
    q.wait_event(|| true).map_err(|e| e)?;
}
fn userland_poll_helper_is_not_a_wait_event() {
    let _ = handle.wait_events(timeout_ms);
}
fn a_predicates_own_default_is_not_the_waits() {
    let queued = || {
        table.get(i).map(|s| !s.is_empty()).unwrap_or(true)
    };
    match sub.wait_event_interruptible(queued) {
        Ok(()) => {}
        Err(_) => {}
    }
}
"#;

    #[test]
    fn syntax_the_line_scan_misread() {
        let found = findings(&[(
            "net/src/a.rs",
            r#"
fn discard_near_a_wait_is_not_a_discard_of_it() {
    let _ = "let _ = q.wait_event(|| true)";
    let r = q.wait_event(|| true);
    match r { Ok(()) => {} Err(_) => {} }
}
fn a_closure_default_on_the_same_line() { match q.wait_event(|| m.get().unwrap_or(true)) { _ => {} } }
fn a_propagated_value_may_be_defaulted() { let v = q.wait_event_until(|| x.take())?.unwrap_or(0); }
fn underscore_assign() { _ = q.wait_event(|| true); }
fn inside_a_macro() { log!("{:?}", q.wait_event(|| true).ok()); }
fn a_typed_discard() { let _: WaitResult<()> = q.wait_event(|| true); }
fn the_variant_api_names_too() { let _ = q.wait_event_uninterruptible_timeout_until(|| None, 5); }
fn chained_twice_reports_once() { let x = q.wait_event(|| true).ok().unwrap_or(()); }
/* let _ = q.wait_event(|| true).ok(); */
#[cfg_attr(test, allow(unused_must_use))]
fn f() {}
"#,
        )]);
        let lines: Vec<&str> = found
            .iter()
            .map(|f| f.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                "net/src/a.rs:10",
                "net/src/a.rs:11",
                "net/src/a.rs:12",
                "net/src/a.rs:13",
                "net/src/a.rs:15",
                "net/src/a.rs:9"
            ],
            "{found:#?}"
        );
    }

    #[test]
    fn a_tree_with_no_kernel_directories_fails_closed() {
        let tree = Tree::new(&[("userland/src/x.rs", "fn f() {}\n")]);
        assert!(run(&tree.repo).is_err());
    }

    #[test]
    fn a_tree_without_the_wait_api_fails_closed() {
        let tree = Tree::new(&[(
            "net/src/x.rs",
            "fn f() { let _ = q.wait_event(|| true); }\n",
        )]);
        let err = run(&tree.repo).err().unwrap().to_string();
        assert!(err.contains("WaitResult"), "{err}");
    }
}
