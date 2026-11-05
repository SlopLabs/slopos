//! A page is never freed twice across a refused map.
//!
//! A `PhysAddr` is a number that says nothing about who owns the page. A
//! function that takes one, mints the owning `UFrame` and can then fail
//! performs a transfer it cannot reverse: the refusal drops the frame, which
//! frees the page, while the caller, holding only the number, frees it again.
//! Neither free is observable (`free_phys` returns 0 for a real release and for
//! an already-free page alike), and on a map path it is reachable from an
//! unprivileged demand fault whenever the buddy is empty. So each map entry
//! point says who owns the page: `map_4kb_user` takes a `UFrame` and hands it
//! back in the error, `map_4kb_user_fresh` allocates and frees iff refused,
//! `map_4kb_user_shared` aliases a paddr someone else owns.
//!
//! Three checks:
//!
//! 1. No fallible step (`?`, `return Err`) between `claim_user_paddr` and the
//!    `cursor.map`/`map_each`/`replace` or `map_4kb_user`/`map_each_4kb_user`/
//!    `replace_4kb_user` call that consumes the frame, in `user_mappings.rs`,
//!    `demand.rs` and `cow.rs`. Measured along the function's control flow
//!    from the claim: a sibling branch of the one that claimed, a closure, and
//!    the claim's own failure arm (`Err(..) =>`, `if let Err`, `let Ok(..) =
//!    .. else`, `claim(..)?`) are not on the path, since no frame exists there.
//!    It is stated over the claim rather than the parameter type, because a
//!    `PhysAddr` parameter is right for the alias-class entry points whose
//!    caller has no handle to give.
//! 2. No `free_page_frame` on a map path: anywhere in `process_vm.rs` and
//!    `user_mappings.rs`, and in the fault paths `demand.rs` and `cow.rs` on
//!    the path after a claim. Those release through the owning handle, so a
//!    raw free there is the second free or frees a page a PTE still holds.
//!    The free in a claim's own failure arm is the caller releasing a page no
//!    MetaSlot ref covers yet, and is accepted.
//! 3. No constructor deciding claim-versus-alias at run time: a `from_in_use`
//!    reached when a `from_unused` fails (a non-`Ok` arm of a match on it, a
//!    branch of an `if` testing it, its `let .. else`, or a combinator chained
//!    on it), in OSTD's frame surface and the mm mapping helpers. One call site
//!    meaning "claim a fresh page" or "alias a live one" depending on the slot
//!    would make the first two checks evadable by not naming the claim.
//!
//! Accepted: alias-class entry points taking a `PhysAddr`, `alias_user_paddr`
//! followed by a fallible step (dropping an alias releases only that ref), and
//! `from_unused`/`from_in_use` each used on its own. A scoped file that no
//! longer exists is a finding, so the gate cannot pass by scanning nothing.

use std::collections::BTreeSet;

use syn::visit::{self, Visit};
use syn::{Expr, ExprCall, ExprMethodCall, Macro, Pat};

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source, visit_expansion};

/// Files that mint an owning user-page handle, or sit on a map error path.
const CLAIM_PATHS: &[&str] = &[
    "mm/src/user_mappings.rs",
    "mm/src/demand.rs",
    "mm/src/cow.rs",
];
/// Map paths that release only through the owning handle.
const NOFREE_PATHS: &[&str] = &["mm/src/process_vm.rs", "mm/src/user_mappings.rs"];
/// Fault paths that free correctly before a claim and never after it.
const AFTER_CLAIM_NOFREE_PATHS: &[&str] = &["mm/src/demand.rs", "mm/src/cow.rs"];
/// OSTD's frame surface and the mm mapping helpers.
const FALLTHROUGH_PATHS: &[&str] = &[
    "slopos-ostd/src/mm/uframe.rs",
    "slopos-ostd/src/mm/frame.rs",
    "mm/src/user_mappings.rs",
];

const CLAIM: &str = "claim_user_paddr";
const FREE: &str = "free_page_frame";
const CONSUMERS: &[&str] = &["map_4kb_user", "map_each_4kb_user", "replace_4kb_user"];
const CURSOR_CONSUMERS: &[&str] = &["map", "map_each", "replace"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    ClaimWindow,
    RawFree,
    Fallthrough,
}

impl Check {
    fn message(self) -> &'static str {
        match self {
            Check::ClaimWindow => {
                "fallible step between claim_user_paddr and the PTE store that consumes the \
                 frame — the refusal drops the frame (freeing the page) while the caller still \
                 holds the paddr and frees it again; move the fallible step ahead of the claim, \
                 or take the owning UFrame as a parameter so the refusal can hand it back"
            }
            Check::RawFree => {
                "raw free_page_frame on a map path — release through the owning handle (the \
                 leaf PTE on success, the returned frame's Drop on refusal); a raw free here is \
                 the second free or frees a page the page table still holds"
            }
            Check::Fallthrough => {
                "from_in_use reached when from_unused fails — one call site must not mean \
                 claim-a-fresh-page or alias-a-live-one depending on the slot; call \
                 claim_user_paddr or alias_user_paddr, and let the caller say which"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FreeRule {
    Never,
    Everywhere,
    AfterClaim,
}

pub fn run(repo: &Repo) -> Result<Report> {
    let mut report = Report::new();
    let mut scoped: Vec<&str> = CLAIM_PATHS
        .iter()
        .chain(NOFREE_PATHS)
        .chain(AFTER_CLAIM_NOFREE_PATHS)
        .chain(FALLTHROUGH_PATHS)
        .copied()
        .collect();
    scoped.sort();
    scoped.dedup();
    let mut scanned = 0;
    for path in &scoped {
        if !repo.exists(path) {
            report.finding(
                *path,
                "scoped file no longer exists — fix the path list in \
                 tools/xtask/src/lint/frame_ownership.rs",
            );
            continue;
        }
        scanned += 1;
        for (line, check) in scan(&Source::load(repo, path)?) {
            report.at(path, line, check.message());
        }
    }
    if scanned == 0 {
        return Err("none of the scoped files exist — the scan would be a no-op".into());
    }
    report.summary(format!(
        "scanned {scanned} of {} map, fault and frame-constructor files",
        scoped.len()
    ));
    Ok(report)
}

fn scan(src: &Source) -> BTreeSet<(usize, Check)> {
    let path = src.path.as_str();
    let frees = if NOFREE_PATHS.contains(&path) {
        FreeRule::Everywhere
    } else if AFTER_CLAIM_NOFREE_PATHS.contains(&path) {
        FreeRule::AfterClaim
    } else {
        FreeRule::Never
    };
    let mut walk = ClaimWalk {
        window: CLAIM_PATHS.contains(&path),
        frees,
        state: Flow::default(),
        claims: 0,
        hits: BTreeSet::new(),
    };
    walk.visit_file(&src.file);
    let mut hits = walk.hits;
    if FALLTHROUGH_PATHS.contains(&path) {
        let mut v = Fallthrough(BTreeSet::new());
        v.visit_file(&src.file);
        hits.extend(v.0.into_iter().map(|l| (l, Check::Fallthrough)));
    }
    hits
}

/// What holds on the path being walked: whether a claimed frame awaits its
/// consuming call, and whether a claim has happened at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Flow {
    open: bool,
    claimed: bool,
}

impl Flow {
    fn join(paths: impl IntoIterator<Item = Flow>) -> Flow {
        paths.into_iter().fold(Flow::default(), |a, b| Flow {
            open: a.open || b.open,
            claimed: a.claimed || b.claimed,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Branch {
    Then,
    Else,
}

/// Walks each function body in evaluation order, carrying a [`Flow`] along
/// each path and joining it where paths meet.
struct ClaimWalk {
    window: bool,
    frees: FreeRule,
    state: Flow,
    /// Claims seen so far, so a construct can tell whether one happened inside
    /// a sub-expression it just walked.
    claims: usize,
    hits: BTreeSet<(usize, Check)>,
}

impl ClaimWalk {
    fn scope(&mut self, walk: impl FnOnce(&mut Self)) {
        let saved = self.state;
        self.state = Flow::default();
        walk(self);
        self.state = saved;
    }

    /// Walk `e` and report whether it made a claim.
    fn claims_in(&mut self, walk: impl FnOnce(&mut Self)) -> bool {
        let before = self.claims;
        walk(self);
        self.claims > before
    }

    fn branch(&mut self, from: Flow, walk: impl FnOnce(&mut Self)) -> Flow {
        self.state = from;
        walk(self);
        self.state
    }

    fn call(&mut self, name: &str, line: usize, consumes: bool) {
        if name == CLAIM {
            self.claims += 1;
            self.state = Flow {
                open: true,
                claimed: true,
            };
        } else if consumes {
            self.state.open = false;
        } else if name == FREE {
            let raw = match self.frees {
                FreeRule::Never => false,
                FreeRule::Everywhere => true,
                FreeRule::AfterClaim => self.state.claimed,
            };
            if raw {
                self.hits.insert((line, Check::RawFree));
            }
        }
    }

    fn fallible(&mut self, line: usize) {
        if self.window && self.state.open {
            self.hits.insert((line, Check::ClaimWindow));
        }
        self.state.open = false;
    }
}

impl<'ast> Visit<'ast> for ClaimWalk {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.scope(|v| visit::visit_item_fn(v, i));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.scope(|v| visit::visit_impl_item_fn(v, i));
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.scope(|v| visit::visit_trait_item_fn(v, i));
    }

    fn visit_expr_closure(&mut self, i: &'ast syn::ExprClosure) {
        self.scope(|v| visit::visit_expr_closure(v, i));
    }

    fn visit_expr_call(&mut self, i: &'ast ExprCall) {
        visit::visit_expr_call(self, i);
        if let Expr::Path(p) = &*i.func
            && let Some(name) = rust::last_ident(&p.path)
        {
            let consumes = CONSUMERS.contains(&name.as_str());
            self.call(
                &name,
                rust::line(p.path.segments.last().unwrap().ident.span()),
                consumes,
            );
        }
    }

    fn visit_expr_method_call(&mut self, i: &'ast ExprMethodCall) {
        visit::visit_expr_method_call(self, i);
        let name = i.method.to_string();
        let consumes = CONSUMERS.contains(&name.as_str())
            || (CURSOR_CONSUMERS.contains(&name.as_str()) && is_cursor(&i.receiver));
        self.call(&name, rust::line(i.method.span()), consumes);
    }

    fn visit_expr_try(&mut self, i: &'ast syn::ExprTry) {
        let own_failure = self.claims_in(|v| v.visit_expr(&i.expr));
        if !own_failure {
            self.fallible(rust::line(i.question_token.span));
        }
    }

    fn visit_expr_return(&mut self, i: &'ast syn::ExprReturn) {
        visit::visit_expr_return(self, i);
        if i.expr.as_deref().is_some_and(is_err_call) {
            self.fallible(rust::line(i.return_token.span));
        }
        self.state = Flow::default();
    }

    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        let claimed = self.claims_in(|v| v.visit_expr(&i.cond));
        let failure = claimed.then(|| failure_branch(&i.cond)).flatten();
        let start = self.state;
        let mut ends = Vec::new();
        if failure != Some(Branch::Then) {
            ends.push(self.branch(start, |v| v.visit_block(&i.then_branch)));
        }
        if failure != Some(Branch::Else) {
            match &i.else_branch {
                Some((_, e)) => ends.push(self.branch(start, |v| v.visit_expr(e))),
                None => ends.push(start),
            }
        }
        self.state = Flow::join(ends);
    }

    fn visit_expr_match(&mut self, i: &'ast syn::ExprMatch) {
        let claimed = self.claims_in(|v| v.visit_expr(&i.expr));
        let start = self.state;
        let mut ends = Vec::new();
        for arm in &i.arms {
            if claimed && is_variant_pat(&arm.pat, "Err") {
                continue;
            }
            ends.push(self.branch(start, |v| v.visit_arm(arm)));
        }
        self.state = Flow::join(ends);
    }

    fn visit_local(&mut self, i: &'ast syn::Local) {
        self.visit_pat(&i.pat);
        let Some(init) = &i.init else {
            return;
        };
        let claimed = self.claims_in(|v| v.visit_expr(&init.expr));
        if let Some((_, diverge)) = &init.diverge
            && !claimed
        {
            let after = self.state;
            self.visit_expr(diverge);
            self.state = after;
        }
    }

    fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
        let start = self.state;
        visit::visit_expr_while(self, i);
        self.state = Flow::join([start, self.state]);
    }

    fn visit_expr_for_loop(&mut self, i: &'ast syn::ExprForLoop) {
        let start = self.state;
        visit::visit_expr_for_loop(self, i);
        self.state = Flow::join([start, self.state]);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
    }
}

/// Whether `pat` is `Name(..)`, e.g. `Err(e)`.
fn is_variant_pat(pat: &Pat, name: &str) -> bool {
    match pat {
        Pat::TupleStruct(t) => rust::last_ident(&t.path).is_some_and(|n| n == name),
        Pat::Paren(p) => is_variant_pat(&p.pat, name),
        _ => false,
    }
}

fn is_err_call(e: &Expr) -> bool {
    match e {
        Expr::Call(c) => matches!(&*c.func, Expr::Path(p) if p.path.is_ident("Err")),
        Expr::Paren(p) => is_err_call(&p.expr),
        _ => false,
    }
}

/// The branch of `if <cond>` taken when the claim in `cond` failed.
fn failure_branch(cond: &Expr) -> Option<Branch> {
    match cond {
        Expr::Let(l) if is_variant_pat(&l.pat, "Err") => Some(Branch::Then),
        Expr::Let(l) if is_variant_pat(&l.pat, "Ok") => Some(Branch::Else),
        Expr::MethodCall(m) if m.method == "is_err" => Some(Branch::Then),
        Expr::MethodCall(m) if m.method == "is_ok" => Some(Branch::Else),
        Expr::Paren(p) => failure_branch(&p.expr),
        _ => None,
    }
}

/// Whether a method receiver names a page-table cursor (`cursor`, `cur`,
/// `self.cursor`, `new_cursor`, ...).
fn is_cursor(e: &Expr) -> bool {
    let name = match e {
        Expr::Path(p) => rust::last_ident(&p.path),
        Expr::Field(f) => match &f.member {
            syn::Member::Named(id) => Some(id.to_string()),
            syn::Member::Unnamed(_) => None,
        },
        Expr::Reference(r) => return is_cursor(&r.expr),
        Expr::Paren(p) => return is_cursor(&p.expr),
        _ => None,
    };
    name.is_some_and(|n| n.ends_with("cursor") || n.ends_with("cur"))
}

/// The lines of every call to `.0` (free or method) the visit reaches.
struct Calls<'n>(&'n str, Vec<usize>);

impl<'ast> Visit<'ast> for Calls<'_> {
    fn visit_expr_call(&mut self, i: &'ast ExprCall) {
        if let Expr::Path(p) = &*i.func
            && let Some(last) = p.path.segments.last()
            && last.ident == self.0
        {
            self.1.push(rust::line(last.ident.span()));
        }
        visit::visit_expr_call(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast ExprMethodCall) {
        if i.method == self.0 {
            self.1.push(rust::line(i.method.span()));
        }
        visit::visit_expr_method_call(self, i);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
    }
}

fn calls_to(e: &Expr, name: &str) -> Vec<usize> {
    let mut v = Calls(name, Vec::new());
    v.visit_expr(e);
    v.1
}

/// Check 3: a `from_in_use` on the failure side of a `from_unused`.
struct Fallthrough(BTreeSet<usize>);

impl Fallthrough {
    fn aliases_in(&mut self, e: &Expr) {
        self.0.extend(calls_to(e, "from_in_use"));
    }
}

impl<'ast> Visit<'ast> for Fallthrough {
    fn visit_expr_match(&mut self, i: &'ast syn::ExprMatch) {
        if !calls_to(&i.expr, "from_unused").is_empty() {
            for arm in i.arms.iter().filter(|a| !is_variant_pat(&a.pat, "Ok")) {
                if let Some((_, guard)) = &arm.guard {
                    self.aliases_in(guard);
                }
                self.aliases_in(&arm.body);
            }
        }
        visit::visit_expr_match(self, i);
    }

    fn visit_expr_if(&mut self, i: &'ast syn::ExprIf) {
        if !calls_to(&i.cond, "from_unused").is_empty() {
            let mut then = Calls("from_in_use", Vec::new());
            then.visit_block(&i.then_branch);
            self.0.extend(then.1);
            if let Some((_, e)) = &i.else_branch {
                self.aliases_in(e);
            }
        }
        visit::visit_expr_if(self, i);
    }

    fn visit_local(&mut self, i: &'ast syn::Local) {
        if let Some(init) = &i.init
            && let Some((_, diverge)) = &init.diverge
            && !calls_to(&init.expr, "from_unused").is_empty()
        {
            self.aliases_in(diverge);
        }
        visit::visit_local(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast ExprMethodCall) {
        if !calls_to(&i.receiver, "from_unused").is_empty() {
            for arg in &i.args {
                self.aliases_in(arg);
            }
        }
        visit::visit_expr_method_call(self, i);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const USER_MAPPINGS: &str = r#"pub fn bad_map_question(vm: &mut KArc<VmSpace>, va: VirtAddr, pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::claim_user_paddr(pa)?;
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}

pub fn good_shared(vm: &mut KArc<VmSpace>, va: VirtAddr, pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::alias_user_paddr(pa)?;
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}
"#;

    const DEMAND: &str = r#"fn bad_map_return(vm: &mut KArc<VmSpace>) -> Result<(), MmError> {
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(pa) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(phys);
            return Err(MmError::MappingFailed);
        }
    };
    if !region.is_ok() {
        return Err(MmError::PermissionDenied);
    }
    map_4kb_user(vm, va, frame, flags)
}
"#;

    const PROCESS_VM: &str = r#"fn map_user_range(vm: &mut KArc<VmSpace>) -> Result<u32, c_int> {
    if let Err(err) = map_4kb_user_fresh(vm, VirtAddr::new(current), map_flags) {
        free_page_frame(phys);
        return Err(-1);
    }
    Ok(1)
}
"#;

    const COW: &str = r#"fn ok_claim_failure_free(vm: &mut KArc<VmSpace>) -> Result<(), MmError> {
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(new_phys) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(new_phys);
            return Err(MmError::MappingFailed);
        }
    };
    replace_4kb_user(vm, va, frame, flags)
}
"#;

    const UFRAME: &str = r#"    pub fn claim_or_alias_user_paddr(paddr: PhysAddr) -> Result<Self, FrameError> {
        match Frame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default()) {
            Ok(frame) => Ok(Self(frame)),
            Err(FrameError::StateMismatch) => Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?)),
            Err(e) => Err(e),
        }
    }

    pub fn claim_user_paddr(paddr: PhysAddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default())?))
    }

    pub fn alias_user_paddr(paddr: PhysAddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?))
    }
"#;

    fn hits(path: &str, text: &str) -> Vec<(usize, Check)> {
        scan(&Source::parse(path, text.to_string()).unwrap())
            .into_iter()
            .collect()
    }

    #[test]
    fn self_test_fixtures_fire_at_their_counts() {
        let tree = Tree::new(&[
            ("mm/src/user_mappings.rs", USER_MAPPINGS),
            ("mm/src/demand.rs", DEMAND),
            ("mm/src/process_vm.rs", PROCESS_VM),
            ("mm/src/cow.rs", COW),
            ("slopos-ostd/src/mm/uframe.rs", UFRAME),
            ("slopos-ostd/src/mm/frame.rs", ""),
        ]);
        let report = run(&tree.repo).unwrap();
        let count = |check: Check| {
            report
                .findings
                .iter()
                .filter(|f| f.message == check.message())
                .count()
        };
        assert_eq!(count(Check::ClaimWindow), 2, "{:?}", report.findings);
        assert_eq!(count(Check::RawFree), 1, "{:?}", report.findings);
        assert_eq!(count(Check::Fallthrough), 1, "{:?}", report.findings);
        let mut locations: Vec<_> = report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        locations.sort();
        assert_eq!(
            locations,
            [
                "mm/src/demand.rs:10",
                "mm/src/process_vm.rs:3",
                "mm/src/user_mappings.rs:3",
                "slopos-ostd/src/mm/uframe.rs:4",
            ]
        );
        assert!(
            report.summary.contains("scanned 6 of 6"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn negatives_stay_silent() {
        assert_eq!(
            hits("mm/src/user_mappings.rs", USER_MAPPINGS),
            [(3, Check::ClaimWindow)]
        );
        assert!(hits("mm/src/cow.rs", COW).is_empty());
        assert_eq!(
            hits("slopos-ostd/src/mm/uframe.rs", UFRAME),
            [(4, Check::Fallthrough)]
        );
    }

    #[test]
    fn missing_scoped_file_is_a_finding_and_none_is_an_error() {
        let tree = Tree::new(&[("mm/src/cow.rs", COW)]);
        let report = run(&tree.repo).unwrap();
        assert_eq!(report.findings.len(), 5, "{:?}", report.findings);
        let empty = Tree::new(&[("README", "")]);
        assert!(run(&empty.repo).is_err());
    }

    #[test]
    fn window_follows_control_flow_not_lines() {
        let text = r#"
fn mid_line(pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::claim_user_paddr(pa).map_err(|_| MapError::PathCorrupt)?;
    let range = lookup(va)?.range; let x = 1;
    map_4kb_user(vm, va, frame, prop).map_err(|(_, e)| e)?;
    Ok(())
}
fn sibling_branch(pa: PhysAddr) -> Result<(), MapError> {
    if fresh {
        let frame = UFrame::<AnonymousMeta>::claim_user_paddr(pa)?;
        return map_4kb_user(vm, va, frame, prop).map_err(|(_, e)| e);
    } else {
        check()?;
    }
    Ok(())
}
fn let_else(pa: PhysAddr) -> Result<(), MapError> {
    let Ok(frame) = UFrame::<AnonymousMeta>::claim_user_paddr(pa) else {
        free_page_frame(pa);
        return Err(MapError::PathCorrupt);
    };
    let mut cur = vs.cursor_mut(range).map_err(|e| e)?;
    cur.map(frame, prop)
}
fn closure_is_its_own_scope(pa: PhysAddr) -> Result<(), MapError> {
    let pages = phys.iter().map_while(|pa| UFrame::<AnonymousMeta>::claim_user_paddr(*pa).ok());
    let n = count()?;
    map_each_4kb_user(vm, first, last, pages)
}
fn in_macro_args(pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::claim_user_paddr(pa)?;
    klog_info!("{}", probe(va)?);
    self.cursor.replace::<Size4Kb, AnonymousMeta>(frame, prop)
}
fn far_apart(pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::claim_user_paddr(pa)?;
    if a { b(); } else { c(); }
    while d() { e(); }
    for x in y { z(x); }
    if broken { return Err(MapError::PathCorrupt); }
    map_4kb_user(vm, va, frame, prop).map_err(|(_, e)| e)
}
"#;
        assert_eq!(
            hits("mm/src/demand.rs", text),
            [
                (4, Check::ClaimWindow),
                (22, Check::ClaimWindow),
                (32, Check::ClaimWindow),
                (40, Check::ClaimWindow),
            ]
        );
    }

    #[test]
    fn frees_after_a_claim_on_fault_paths() {
        let text = r#"
fn fault(pa: PhysAddr) -> Result<(), MmError> {
    if pa.is_null() {
        free_page_frame(pa);
        return Err(MmError::NoMemory);
    }
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(pa) {
        Ok(f) => f,
        Err(_) => {
            free_page_frame(pa);
            return Err(MmError::MappingFailed);
        }
    };
    if let Err((_, e)) = map_4kb_user(vm, va, frame, prop) {
        free_page_frame(pa);
    }
    Ok(())
}
fn literal_names_nothing() {
    log("free_page_frame(pa) // claim_user_paddr(pa)");
}
"#;
        assert_eq!(hits("mm/src/cow.rs", text), [(15, Check::RawFree)]);
        assert_eq!(
            hits("mm/src/process_vm.rs", text),
            [(4, Check::RawFree), (15, Check::RawFree)]
        );
    }

    #[test]
    fn fallthrough_spellings() {
        let text = r#"
fn by_wildcard(pa: PhysAddr) -> Result<Frame, FrameError> {
    match Frame::from_unused(pa, M::default()) {
        Ok(f) => Ok(f),
        _ => Frame::from_in_use(pa),
    }
}
fn by_combinator(pa: PhysAddr) -> Result<Frame, FrameError> {
    Frame::from_unused(pa, M::default())
        .or_else(|_| Frame::from_in_use(pa))
}
fn by_let_else(pa: PhysAddr) -> Result<Frame, FrameError> {
    let Ok(f) = Frame::from_unused(pa, M::default()) else {
        return Frame::from_in_use(pa);
    };
    Ok(f)
}
fn by_if(pa: PhysAddr) -> Result<Frame, FrameError> {
    if let Ok(f) = Frame::from_unused(pa, M::default()) { Ok(f) } else { Frame::from_in_use(pa) }
}
fn ok_arm_is_fine(pa: PhysAddr) -> Result<(), FrameError> {
    match Frame::from_unused(pa, M::default()) {
        Ok(f) => { let _alias = Frame::from_in_use(other)?; Ok(()) }
        Err(e) => Err(e),
    }
}
"#;
        assert_eq!(
            hits("slopos-ostd/src/mm/frame.rs", text),
            [
                (5, Check::Fallthrough),
                (10, Check::Fallthrough),
                (14, Check::Fallthrough),
                (19, Check::Fallthrough),
            ]
        );
    }
}
