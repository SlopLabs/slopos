//! Wait predicates only observe; epoch read sections never block.
//!
//! A predicate handed to a `wait_event*` entry point is re-run by the wait
//! loop to decide whether to keep sleeping. One that dispatches NAPI
//! (`napi::wake_napi`), forces a poll (`force_napi_poll`) or sleeps
//! (`sleep_current_task_ms`) papers over a scheduler or NAPI starvation bug by
//! side-effecting the world instead of exposing it. The fix belongs where the
//! starvation is, so the allowlist is empty and meant to stay that way.
//!
//! Inside the scope of an epoch guard (`let _g = NET_EPOCH.enter();`,
//! `Epoch::enter(&E)`) up to the end of its block or its `drop`, nothing
//! takes a lock (`.lock()`), sleeps (`sleep_current_task_ms`), yields
//! (`yield_with_deadline`) or waits (`.wait()`): holding a lock or yielding
//! across a read section delays every grace period and risks the
//! atomic-publish hazard that once broke SCM_RIGHTS in `unix_sendmsg`.
//! Runtime lockdep is the load-bearing enforcement; this catches the source
//! shape before it runs. `Epoch`'s own source is exempt.
//!
//! The predicate is the argument the wait API declares as a closure, read
//! from slopos-ostd's signatures; a predicate bound to a local closure first
//! is followed to that closure.

use std::collections::{BTreeMap, BTreeSet};

use syn::visit::{self, Visit};
use syn::{Expr, Pat, Stmt};

use super::wait_api::EntryPoints;
use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source, macro_exprs};

/// The kernel directories this gate scans; userland calls `force_napi_poll`
/// legitimately, as a synchronous poll rather than inside a predicate.
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
    "windowing",
];

/// Files allowed a side effect inside a wait predicate. Empty: an entry says
/// a predicate drains the NIC itself, which only helps when the netpoll
/// kthread is not running, a marker for a bug elsewhere.
const ALLOWLIST: &[&str] = &[];

/// Calls a predicate must not make, by last path segment, with the name the
/// finding gives them.
const PREDICATE_BANNED: &[(&str, &str)] = &[
    ("wake_napi", "napi::wake_napi"),
    ("force_napi_poll", "force_napi_poll"),
    ("sleep_current_task_ms", "sleep_current_task_ms"),
];

/// Calls an epoch read section must not make, by path or method name.
const EPOCH_BANNED_CALLS: &[&str] = &["sleep_current_task_ms", "yield_with_deadline"];

/// Zero-argument methods an epoch read section must not call.
const EPOCH_BANNED_METHODS: &[&str] = &["lock", "wait"];

const EPOCH_SOURCE: &str = "slopos-ostd/src/sync/epoch.rs";

pub fn run(repo: &Repo) -> Result<Report> {
    scan(repo, ALLOWLIST)
}

fn scan(repo: &Repo, allowlist: &[&str]) -> Result<Report> {
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
    let (mut predicates, mut sections) = (0, 0);
    for path in &files {
        let src = Source::load(repo, path)?;
        let mut v = Scan {
            entries: &entries,
            predicates_checked: !allowlist.contains(&path.as_str()),
            epochs_checked: path != EPOCH_SOURCE,
            closures: Vec::new(),
            found: BTreeSet::new(),
            predicates: 0,
            sections: 0,
        };
        v.visit_file(&src.file);
        predicates += v.predicates;
        sections += v.sections;
        for (line, msg) in v.found {
            report.at(path, line, msg);
        }
    }
    let dirs = KERNEL_DIRS.iter().filter(|d| repo.exists(d)).count();
    report.summary(format!(
        "scanned {} files in {dirs} kernel directories: {predicates} wait predicates observe \
         only, {sections} epoch read sections never block",
        files.len()
    ));
    Ok(report)
}

struct Scan<'a> {
    entries: &'a EntryPoints,
    predicates_checked: bool,
    epochs_checked: bool,
    /// Closures bound by `let name = || ..` in each enclosing function body.
    closures: Vec<BTreeMap<String, syn::ExprClosure>>,
    found: BTreeSet<(usize, String)>,
    predicates: usize,
    sections: usize,
}

impl Scan<'_> {
    fn in_fn(&mut self, body: &syn::Block, walk: impl FnOnce(&mut Self)) {
        let mut locals = LocalClosures::default();
        locals.visit_block(body);
        self.closures.push(locals.0);
        walk(self);
        self.closures.pop();
    }

    fn closure_named(&self, name: &str) -> Option<syn::ExprClosure> {
        self.closures.last().and_then(|m| m.get(name)).cloned()
    }

    fn check_predicate(&mut self, wait: &syn::Ident, arg: &Expr) {
        let mut arg = arg;
        while let Expr::Reference(r) = arg {
            arg = &r.expr;
        }
        let closure = match arg {
            Expr::Closure(c) => Some(c.clone()),
            Expr::Path(p) => p
                .path
                .get_ident()
                .and_then(|i| self.closure_named(&i.to_string())),
            _ => None,
        };
        let Some(closure) = closure else { return };
        self.predicates += 1;
        let mut hits = Calls::new(|name, _| {
            PREDICATE_BANNED
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, shown)| *shown)
        });
        hits.visit_expr(&closure.body);
        for (line, shown) in hits.found {
            self.found.insert((
                line,
                format!(
                    "`{shown}` inside the predicate of `{wait}` — a wait predicate only observes \
                     state; move the side effect out, or fix the starvation it papers over"
                ),
            ));
        }
    }

    fn check_block(&mut self, block: &syn::Block) {
        for (i, stmt) in block.stmts.iter().enumerate() {
            let Some(guard) = epoch_guard(stmt) else {
                continue;
            };
            self.sections += 1;
            for later in &block.stmts[i + 1..] {
                if guard.as_deref().is_some_and(|g| drops(later, g)) {
                    break;
                }
                if matches!(later, Stmt::Item(_)) {
                    continue;
                }
                let mut hits = Calls::new(|name, nargs| {
                    let methods = if nargs == 0 {
                        EPOCH_BANNED_METHODS
                    } else {
                        &[]
                    };
                    EPOCH_BANNED_CALLS
                        .iter()
                        .chain(methods)
                        .find(|n| **n == name)
                        .copied()
                });
                hits.methods_only = EPOCH_BANNED_METHODS;
                hits.visit_stmt(later);
                for (line, name) in hits.found {
                    let shown = if EPOCH_BANNED_METHODS.contains(&name) {
                        format!(".{name}()")
                    } else {
                        name.to_string()
                    };
                    self.found.insert((
                        line,
                        format!(
                            "`{shown}` inside an epoch read section — end the guard's scope \
                             before the lock, sleep, yield or wait"
                        ),
                    ));
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.in_fn(&f.block, |s| visit::visit_item_fn(s, f));
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.in_fn(&f.block, |s| visit::visit_impl_item_fn(s, f));
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        match &f.default {
            Some(body) => self.in_fn(body, |s| visit::visit_trait_item_fn(s, f)),
            None => visit::visit_trait_item_fn(self, f),
        }
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        if self.predicates_checked
            && let Some(i) = self.entries.predicate(&mc.method.to_string())
            && let Some(arg) = mc.args.iter().nth(i)
        {
            self.check_predicate(&mc.method, arg);
        }
        visit::visit_expr_method_call(self, mc);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        if self.epochs_checked {
            self.check_block(block);
        }
        visit::visit_block(self, block);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for e in macro_exprs(mac) {
            self.visit_expr(&e);
        }
    }
}

/// `let name = || ..` anywhere in one function body, nested functions aside.
#[derive(Default)]
struct LocalClosures(BTreeMap<String, syn::ExprClosure>);

impl<'ast> Visit<'ast> for LocalClosures {
    fn visit_local(&mut self, local: &'ast syn::Local) {
        if let Pat::Ident(p) = &local.pat
            && let Some(init) = &local.init
            && let Expr::Closure(c) = init.expr.as_ref()
        {
            self.0.insert(p.ident.to_string(), c.clone());
        }
        visit::visit_local(self, local);
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

/// Calls in an expression that `pick` names, with the line of each.
struct Calls<F> {
    pick: F,
    /// Names matched only as zero-argument method calls.
    methods_only: &'static [&'static str],
    found: Vec<(usize, &'static str)>,
}

impl<F: Fn(&str, usize) -> Option<&'static str>> Calls<F> {
    fn new(pick: F) -> Self {
        Self {
            pick,
            methods_only: &[],
            found: Vec::new(),
        }
    }
}

impl<'ast, F: Fn(&str, usize) -> Option<&'static str>> Visit<'ast> for Calls<F> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(p) = call.func.as_ref()
            && let Some(last) = p.path.segments.last()
        {
            let name = last.ident.to_string();
            if !self.methods_only.contains(&name.as_str())
                && let Some(shown) = (self.pick)(&name, call.args.len())
            {
                self.found.push((rust::line(last.ident.span()), shown));
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        if let Some(shown) = (self.pick)(&mc.method.to_string(), mc.args.len()) {
            self.found.push((rust::line(mc.method.span()), shown));
        }
        visit::visit_expr_method_call(self, mc);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for e in macro_exprs(mac) {
            self.visit_expr(&e);
        }
    }

    fn visit_item(&mut self, _: &'ast syn::Item) {}
}

/// Whether `stmt` opens an epoch read section, and the guard's name when it
/// has one (`let _ = E.enter()` drops the guard at once and opens nothing).
fn epoch_guard(stmt: &Stmt) -> Option<Option<String>> {
    let Stmt::Local(local) = stmt else {
        return None;
    };
    let init = local.init.as_ref()?;
    if !is_epoch_enter(&init.expr) {
        return None;
    }
    let mut pat = &local.pat;
    if let Pat::Type(t) = pat {
        pat = &t.pat;
    }
    match pat {
        Pat::Wild(_) => None,
        Pat::Ident(p) => Some(Some(p.ident.to_string())),
        _ => Some(None),
    }
}

fn is_epoch_enter(e: &Expr) -> bool {
    match e {
        Expr::MethodCall(mc) if mc.method == "enter" && mc.args.is_empty() => {
            let receiver = match mc.receiver.as_ref() {
                Expr::Path(p) => rust::last_ident(&p.path),
                Expr::Field(f) => match &f.member {
                    syn::Member::Named(i) => Some(i.to_string()),
                    syn::Member::Unnamed(_) => None,
                },
                Expr::Reference(r) => match r.expr.as_ref() {
                    Expr::Path(p) => rust::last_ident(&p.path),
                    _ => None,
                },
                _ => None,
            };
            receiver.is_some_and(|r| r.to_ascii_lowercase().ends_with("epoch"))
        }
        Expr::Call(call) => matches!(call.func.as_ref(),
            Expr::Path(p) if rust::path_ends_with(&p.path, &["Epoch", "enter"])),
        Expr::Paren(p) => is_epoch_enter(&p.expr),
        _ => false,
    }
}

/// Whether `stmt` is `drop(guard);`.
fn drops(stmt: &Stmt, guard: &str) -> bool {
    let Stmt::Expr(Expr::Call(call), _) = stmt else {
        return false;
    };
    matches!(call.func.as_ref(), Expr::Path(p) if rust::last_ident(&p.path).as_deref() == Some("drop"))
        && matches!(call.args.first(), Some(Expr::Path(p)) if p.path.is_ident(guard))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;
    use crate::lint::wait_api::tests::api;

    fn findings_with(files: &[(&str, &str)], allowlist: &[&str]) -> Vec<String> {
        let (api_path, api_text) = api();
        let mut all = vec![(api_path, api_text.as_str())];
        all.extend_from_slice(files);
        let tree = Tree::new(&all);
        let report = scan(&tree.repo, allowlist).unwrap();
        let mut out: Vec<String> = report
            .findings
            .iter()
            .map(|f| format!("{}: {}", f.location, f.message))
            .collect();
        out.sort();
        out
    }

    fn findings(files: &[(&str, &str)]) -> Vec<String> {
        findings_with(files, &[])
    }

    fn count(found: &[String], needle: &str) -> usize {
        found.iter().filter(|f| f.contains(needle)).count()
    }

    const PREDICATE: &str = "inside the predicate";
    const EPOCH: &str = "inside an epoch read section";

    #[test]
    fn script_fixtures_fire_at_the_same_counts() {
        let found = findings_with(
            &[
                ("net/src/wait_positives.rs", WAIT_POSITIVES),
                ("net/src/epoch_positives.rs", EPOCH_POSITIVES),
                ("net/src/allowlisted_fixture.rs", ALLOWLISTED),
                ("net/src/negatives.rs", NEGATIVES),
                (
                    "slopos-ostd/src/sync/epoch.rs",
                    "fn own_source_is_exempt() {\n    let _guard = Epoch::enter();\n    let x = m.lock();\n}\n",
                ),
                (
                    "userland/src/x.rs",
                    "fn outside_the_kernel_dirs() { let _ = q.wait_event(|| { force_napi_poll(); true }); }\n",
                ),
            ],
            &["net/src/allowlisted_fixture.rs"],
        );
        assert_eq!(count(&found, PREDICATE), 4, "{found:#?}");
        assert_eq!(count(&found, EPOCH), 5, "{found:#?}");
        assert!(
            !found.iter().any(|f| f.starts_with("net/src/negatives.rs")
                || f.starts_with("slopos-ostd/src/sync/epoch.rs")
                || f.starts_with("userland/")),
            "{found:#?}"
        );
        assert_eq!(found.len(), 9, "{found:#?}");
    }

    const WAIT_POSITIVES: &str = "\
fn a() { let _ = q.wait_event(|| { crate::napi::wake_napi(); true }); }
fn b() { let _ = q.wait_event_timeout(|| { crate::napi::wake_napi(); true }, 10); }
fn c() { let _ = q.wait_event_until(|| { force_napi_poll(); true }); }
fn d() { let _ = q.wait_event(|| { sleep_current_task_ms(1); true }); }
";

    const EPOCH_POSITIVES: &str = "\
fn g() {
    let _guard = NET_EPOCH.enter();
    let x = m.lock();
    sleep_current_task_ms(1);
    yield_with_deadline(0);
    let y = w.wait();
}
";

    const ALLOWLISTED: &str = "\
fn allowlisted() { let _ = q.wait_event(|| { crate::napi::wake_napi(); true }); }
fn not_allowlisted_for_epoch() {
    let _guard = NET_EPOCH.enter();
    let x = m.lock();
}
";

    const NEGATIVES: &str = "\
fn bare_wake_with_no_wait_anywhere() {
    crate::napi::wake_napi();
}
fn far_below_a_wait_event() {
    let _ = q.wait_event(|| true);
    // 1
    // 2
    // 3
    // 4
    // 5
    // 6
    // 7
    // 8
    // 9
    // 10
    // 11
    // 12
    // 13
    // 14
    // 15
    // 16
    // 17
    // 18
    // 19
    // 20
    // 21
    crate::napi::wake_napi();
}
";

    #[test]
    fn the_allowlist_exempts_predicates_only() {
        let found = findings(&[("net/src/allowlisted_fixture.rs", ALLOWLISTED)]);
        assert_eq!(count(&found, PREDICATE), 1, "{found:#?}");
        assert_eq!(count(&found, EPOCH), 1, "{found:#?}");
        assert!(ALLOWLIST.is_empty());
    }

    #[test]
    fn predicates_are_the_closure_argument_not_the_neighbourhood() {
        let found = findings(&[(
            "net/src/a.rs",
            r#"
fn right_after_a_wait_is_outside_its_predicate() {
    let _ = q.wait_event(|| ready());
    crate::napi::wake_napi();
}
fn a_named_predicate_is_followed() {
    let pending = || { force_napi_poll(); ready() };
    let _ = q.wait_event_interruptible(pending);
}
fn every_variant_of_the_api() {
    let _ = q.wait_event_uninterruptible_timeout_until(|| { sleep_current_task_ms(1); None }, 5);
    let _ = q.wait_event_interruptible_timeout(&mut || { napi::wake_napi(); true }, 5);
}
fn the_timeout_is_not_the_predicate() {
    let _ = q.wait_event_timeout(|| true, { sleep_current_task_ms(1); 5 });
}
fn a_brace_in_a_literal_does_not_end_the_predicate() {
    let _ = q.wait_event(|| { let c = '}'; let s = "})"; force_napi_poll(); c == '{' });
}
fn inside_a_macro() {
    assert!(q.wait_event(|| { force_napi_poll(); true }).is_ok());
}
fn a_mention_in_a_string_is_not_a_call() {
    let _ = q.wait_event(|| { log("force_napi_poll()"); true });
}
// let _ = q.wait_event(|| { force_napi_poll(); true });
"#,
        )]);
        let lines: Vec<&str> = found
            .iter()
            .map(|f| f.split(": ").next().unwrap())
            .collect();
        assert_eq!(
            lines,
            [
                "net/src/a.rs:11",
                "net/src/a.rs:12",
                "net/src/a.rs:18",
                "net/src/a.rs:21",
                "net/src/a.rs:7",
            ],
            "{found:#?}"
        );
    }

    #[test]
    fn an_epoch_section_is_its_guards_scope() {
        let found = findings(&[(
            "net/src/a.rs",
            r#"
fn ends_with_its_block() {
    {
        let _g = NET_EPOCH.enter();
        lookup();
    }
    let x = m.lock();
}
fn ends_at_drop() {
    let g = NET_EPOCH.enter();
    lookup();
    drop(g);
    sleep_current_task_ms(1);
}
fn a_discarded_guard_opens_nothing() {
    let _ = NET_EPOCH.enter();
    let x = m.lock();
}
fn a_non_epoch_enter_opens_nothing() {
    current_backend().enter();
    let x = m.lock();
}
fn a_lock_with_arguments_is_not_a_lock() {
    let _g = Epoch::enter(&NET_EPOCH);
    let x = table.lock(key);
    let y = self.epoch.enter();
}
fn a_closure_body_in_the_section_counts() {
    let _g = self.epoch.enter();
    table.for_each(|e| e.state.lock().touch());
}
fn twenty_lines_on_is_still_inside() {
    let _g = NET_EPOCH.enter();
    // 1
    // 2
    // 3
    // 4
    // 5
    // 6
    // 7
    // 8
    // 9
    // 10
    // 11
    // 12
    // 13
    // 14
    // 15
    // 16
    // 17
    // 18
    // 19
    // 20
    // 21
    yield_with_deadline(0);
}
"#,
        )]);
        let lines: Vec<&str> = found
            .iter()
            .map(|f| f.split(": ").next().unwrap())
            .collect();
        assert_eq!(lines, ["net/src/a.rs:30", "net/src/a.rs:55"], "{found:#?}");
    }

    #[test]
    fn a_tree_with_no_kernel_directories_fails_closed() {
        let tree = Tree::new(&[("userland/src/x.rs", "fn f() {}\n")]);
        assert!(run(&tree.repo).is_err());
    }
}
