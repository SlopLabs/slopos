//! `KArc<Task>` is the only owning task handle, and `TaskRef` the only way to
//! hold one outside slopos-ostd (AGENTS.md's task-ownership discipline).
//!
//! A `*mut Task` says nothing about whether the task is still alive, whether
//! anyone else is mutating it, or who tears it down; `KArc<Task>` says all
//! three. Eight checks keep the raw form and its disguises out of the kernel:
//!
//! 1. No raw task pointer (`*mut`/`*const` `Task` or `TaskInner`) in binding
//!    position: fn and closure arguments, struct fields, typed `let`s,
//!    statics and consts, at any depth of the bound type (`Option<*mut
//!    Task>` too) save inside `KernelSync<..>` (check 2), `PhantomData<..>`
//!    and a callable's return type. Return position is deliberately not
//!    checked: the sanctioned surfaces hand pointers out by design. A
//!    `*mut *mut Task` double indirection is a finding in any position.
//! 2. No `KernelSync<*mut Task>`: a task handle laundered through a blanket
//!    Send/Sync wrapper, with the compiler's objection silenced.
//! 3. No task handle laundered through `c_void`, matched on the cast, never
//!    the type (`TaskEntry`'s `*mut c_void` payload is not a task handle):
//!    3a, a cast to `c_void` whose operand names a `*mut Task`; 3b, a cast
//!    back to `Task` inside a function taking a `*mut c_void` parameter,
//!    without which 3a could be met by keeping only the receiving half.
//! 4. No `task_borrow` / `task_borrow_mut`: an accessor turning a raw task
//!    pointer back into a reference undoes every check above.
//! 5. No `unsafe impl Send`/`Sync` whose safety argument (the comments and
//!    attributes directly above it, up to eight lines) leans on a task
//!    refcount. A refcount buys existence, never exclusivity.
//! 6. No `refcnt` / `task_inc_ref` / `task_dec_ref` / `inc_ref` / `dec_ref`:
//!    manual refcounting is what `KArc` exists to make unnecessary. The
//!    page-table and frame refcount domain (`mm/src/paging/`,
//!    `slopos-ostd/src/mm/`) counts mappings, a different owner, and is exempt.
//! 7. The fault and panic paths (`boot/src/exception.rs`, `boot/src/idt.rs`,
//!    every `slopos-ostd/src/**/panic*.rs`) never mention a lookup that takes
//!    the `TASK_MANAGER` cli-spinlock or a `KArc` upgrade: a fault arriving
//!    under that lock would deadlock the dump, and a destructor run from an IST
//!    stack has neither the stack nor the allocator locks it needs.
//! 8. No function whose return type names a lifetime declared on its own
//!    generic list that no argument supplies: the caller picks it, so two calls
//!    hand out two live references to one place. An argument mentioning the
//!    lifetime only inside `PhantomData<..>`, a `fn(..)` or `Fn*(..)` parameter
//!    list, or a `+ 'a` outlives bound supplies nothing, since the callee can
//!    still mint the reference under a lifetime the caller chose; the same
//!    spellings in the return type mint nothing either. Known limits: a
//!    lifetime declared on an enclosing `impl<'a>` is not seen; a shared
//!    receiver minting `&mut` is beside the point of a lifetime rule; a
//!    consumed-by-value input (`Box::leak`'s shape) or an associated-type
//!    equality bound tying `'a` to the caller's data is still reported, since
//!    telling those apart needs type resolution.
//!
//! Checks 1 and 3 exempt the sanctioned surfaces, which convert between the
//! raw form and the owned one: the placement state machine, the intrusive
//! link roles, the ABI-frozen PCR `current_task` slot, and the pre-heap
//! `.bss` bootstrap stubs. A sanctioned surface or fault-path file that no
//! longer exists is itself a finding: a stale entry is a hole nobody watches.
//!
//! Scope is a deny-list, so a new kernel crate is scanned the day it lands:
//! every `.rs` file outside the userland-side crates, plans, proofs and
//! vendored code. Items under `#[cfg(test)]` never reach the kernel and are
//! skipped by every check but 5 and 7.
//!
//! Checks 4, 6 and 7 match identifiers on the file's token stream rather than
//! its syntax tree, so an identifier inside any macro body counts. The
//! syntax-tree checks see into a macro body by re-parsing its tokens, with each
//! `$var` read as a plain identifier and each `$(..)` repetition as one copy of
//! its contents, as statements or else as a comma-separated expression list; a
//! body that parses as neither (a custom DSL) is left to the identifier checks.

use std::collections::BTreeSet;
use std::str::FromStr;

use proc_macro2::{LineColumn, TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprCast, ExprMethodCall, Field, FnArg, GenericArgument, ImplItem, Item,
    ItemImpl, Macro, PatType, PathArguments, PathSegment, ReturnType, Signature, Stmt, TraitItem,
    Type, TypeParamBound,
};

use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{self, Source, item_attrs, visit_expansion};

/// Userland-side crates, planning docs, proofs, vendored code and build output.
const OUT_OF_SCOPE: &[&str] = &[
    "userland",
    "slibc",
    "slop-protocol",
    "appkit",
    "slopos-rt",
    "image",
    "terminal-core",
    "keymap-core",
    "shell-core",
    "editor-core",
    "http-core",
    "tls-core",
    "plans",
    "verification",
    "vendor",
    "third_party",
    "builddir",
    "target",
];

/// Exempt from checks 1 and 3: each converts between the raw form and the
/// owned one, or predates the heap.
const SANCTIONED_SURFACES: &[&str] = &[
    "slopos-ostd/src/task/placement.rs",
    "slopos-ostd/src/task/link_roles.rs",
    "slopos-ostd/src/cpu/x86_64/pcr.rs",
    "slopos-ostd/src/task/bootstrap.rs",
];

/// Exempt from check 6: these count page-table mappings and frame refs.
const REFCOUNT_DOMAIN: &[&str] = &["mm/src/paging", "slopos-ostd/src/mm"];

/// Check 7's named files; every `panic*.rs` under `PANIC_ROOT` joins them.
const FAULT_PATH_FILES: &[&str] = &["boot/src/exception.rs", "boot/src/idt.rs"];
const PANIC_ROOT: &str = "slopos-ostd/src";
const FAULT_PATH_FORBIDDEN: &[&str] = &[
    "task_find_by_id",
    "task_find_by_cr3",
    "task_pointer_is_valid",
    "to_owned",
    "task_placement_clone",
];

const BORROW_ACCESSORS: &[&str] = &["task_borrow", "task_borrow_mut"];
const REFCOUNT_IDENTS: &[&str] = &[
    "refcnt",
    "task_inc_ref",
    "task_dec_ref",
    "inc_ref",
    "dec_ref",
];
const TASK_TYPES: &[&str] = &["Task", "TaskInner"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    RawBinding,
    KernelSync,
    LaunderOut,
    LaunderIn,
    BorrowAccessor,
    RefcountSendSync,
    ManualRefcount,
    FaultPath,
    FreeLifetime,
}

impl Check {
    fn message(self) -> &'static str {
        match self {
            Check::RawBinding => {
                "raw task pointer in binding position — hold the task as KArc<Task> or TaskRef, or \
                 route the transfer through slopos-ostd/src/task/placement.rs"
            }
            Check::KernelSync => {
                "KernelSync<*mut Task> is an owner-less task handle with Send/Sync silenced — hold \
                 a KArc<Task> or TaskRef instead"
            }
            Check::LaunderOut => {
                "task handle cast out to c_void — pass a KArc<Task> or TaskRef, or route the \
                 transfer through slopos-ostd/src/task/placement.rs"
            }
            Check::LaunderIn => {
                "task handle cast back in from a `*mut c_void` parameter — take a typed task \
                 handle instead of round-tripping it through c_void"
            }
            Check::BorrowAccessor => {
                "task_borrow / task_borrow_mut turns a raw task pointer into a reference — reach \
                 the task through a TaskRef or KArc<Task>"
            }
            Check::RefcountSendSync => {
                "unsafe impl Send/Sync justified by a task refcount — a refcount proves the \
                 allocation exists, not who may write it; justify the impl by what serialises \
                 the writes"
            }
            Check::ManualRefcount => {
                "manual task refcount manipulation (refcnt / inc_ref / dec_ref) — let KArc own \
                 the count"
            }
            Check::FaultPath => {
                "the panic/fault path takes the task-manager lock or upgrades a KArc (task \
                 lookup, to_owned, task_placement_clone) — read what the fault needs without a \
                 lock or an owning handle"
            }
            Check::FreeLifetime => {
                "return type names a lifetime no argument supplies, so the caller picks it and \
                 two calls alias — tie the lifetime to an argument or hand out an owned handle"
            }
        }
    }
}

type Hits = BTreeSet<(String, usize, Check)>;

pub fn run(repo: &Repo) -> Result<Report> {
    let files: Vec<String> = repo
        .rs_files()?
        .into_iter()
        .filter(|f| in_scope(f))
        .collect();
    if files.is_empty() {
        return Err("no Rust sources in scope — the scan would be a no-op".into());
    }
    let mut hits = Hits::new();
    for path in &files {
        scan_source(&Source::load(repo, path)?, &mut hits);
    }
    let fault_files = fault_path_files(repo)?;
    for path in &fault_files {
        scan_fault_path(path, &repo.read(path)?, &mut hits)?;
    }

    let mut report = Report::new();
    for surface in SANCTIONED_SURFACES {
        if !repo.exists(surface) {
            report.finding(
                *surface,
                "sanctioned surface no longer exists — fix the path in SANCTIONED_SURFACES in \
                 tools/xtask/src/lint/task_ownership.rs, or remove the stale exemption",
            );
        }
    }
    for named in FAULT_PATH_FILES {
        if !repo.exists(named) {
            report.finding(
                *named,
                "fault-path file no longer exists — fix the path in FAULT_PATH_FILES in \
                 tools/xtask/src/lint/task_ownership.rs",
            );
        }
    }
    for (path, line, check) in hits {
        report.at(&path, line, check.message());
    }
    report.summary(format!(
        "scanned {} files outside the userland-side crates ({} sanctioned surfaces exempt from \
         checks 1 and 3) and {} fault-path files",
        files.len(),
        SANCTIONED_SURFACES.len(),
        fault_files.len()
    ));
    Ok(report)
}

fn in_scope(path: &str) -> bool {
    !OUT_OF_SCOPE.iter().any(|d| under(path, d))
}

fn fault_path_files(repo: &Repo) -> Result<Vec<String>> {
    let mut files: Vec<String> = FAULT_PATH_FILES
        .iter()
        .filter(|f| repo.exists(f))
        .map(|f| f.to_string())
        .collect();
    files.extend(
        repo.rs_files_under(&[PANIC_ROOT])?
            .into_iter()
            .filter(|f| f.rsplit('/').next().is_some_and(|n| n.starts_with("panic"))),
    );
    files.sort();
    files.dedup();
    Ok(files)
}

fn scan_source(src: &Source, hits: &mut Hits) {
    let mut v = Scan {
        lines: src.text.lines().collect(),
        exempt_casts: SANCTIONED_SURFACES.contains(&src.path.as_str()),
        test_depth: 0,
        test_spans: Vec::new(),
        c_void_fns: Vec::new(),
        found: BTreeSet::new(),
    };
    v.visit_file(&src.file);
    let exempt_refcount = REFCOUNT_DOMAIN.iter().any(|d| under(&src.path, d));
    let mut found = v.found;
    let test_spans = v.test_spans;
    each_ident(src.file.to_token_stream(), &mut |name, at| {
        if test_spans.iter().any(|(lo, hi)| *lo <= at && at <= *hi) {
            return;
        }
        if BORROW_ACCESSORS.contains(&name) {
            found.insert((at.line, Check::BorrowAccessor));
        }
        if !exempt_refcount && REFCOUNT_IDENTS.contains(&name) {
            found.insert((at.line, Check::ManualRefcount));
        }
    });
    hits.extend(found.into_iter().map(|(l, c)| (src.path.clone(), l, c)));
}

fn scan_fault_path(path: &str, text: &str, hits: &mut Hits) -> Result<()> {
    let tokens = TokenStream::from_str(text).map_err(|e| format!("{path}: {e}"))?;
    each_ident(tokens, &mut |name, at| {
        if FAULT_PATH_FORBIDDEN.iter().any(|f| name.contains(f)) {
            hits.insert((path.to_string(), at.line, Check::FaultPath));
        }
    });
    Ok(())
}

/// Every identifier in `tokens`, at any depth, with where it starts.
fn each_ident(tokens: TokenStream, f: &mut impl FnMut(&str, LineColumn)) {
    for tt in tokens {
        match tt {
            TokenTree::Ident(id) => {
                let name = id.to_string();
                f(name.strip_prefix("r#").unwrap_or(&name), id.span().start());
            }
            TokenTree::Group(g) => each_ident(g.stream(), f),
            _ => {}
        }
    }
}

struct Scan<'t> {
    lines: Vec<&'t str>,
    exempt_casts: bool,
    test_depth: usize,
    test_spans: Vec<(LineColumn, LineColumn)>,
    /// Per enclosing `fn`, whether it takes a `*mut c_void` parameter.
    c_void_fns: Vec<bool>,
    found: BTreeSet<(usize, Check)>,
}

impl Scan<'_> {
    fn hit(&mut self, line: usize, check: Check) {
        let exempt = match check {
            Check::RefcountSendSync | Check::FaultPath => false,
            Check::RawBinding | Check::LaunderOut | Check::LaunderIn => {
                self.exempt_casts || self.test_depth > 0
            }
            _ => self.test_depth > 0,
        };
        if !exempt {
            self.found.insert((line, check));
        }
    }

    fn gated<T: Spanned>(&mut self, attrs: &[Attribute], node: &T, walk: impl FnOnce(&mut Self)) {
        let test = rust::is_cfg_test(attrs);
        if test {
            let span = node.span();
            self.test_spans.push((span.start(), span.end()));
            self.test_depth += 1;
        }
        walk(self);
        if test {
            self.test_depth -= 1;
        }
    }

    fn in_fn(&mut self, sig: &Signature, walk: impl FnOnce(&mut Self)) {
        let takes_c_void = sig.inputs.iter().any(|arg| match arg {
            FnArg::Typed(pt) => ptr_to(&pt.ty, &["c_void"]),
            FnArg::Receiver(_) => false,
        });
        self.c_void_fns.push(takes_c_void);
        walk(self);
        self.c_void_fns.pop();
    }

    fn binding(&mut self, ty: &Type) {
        if holds_raw_task(ty) {
            self.hit(rust::line(ty.span()), Check::RawBinding);
        }
    }

    /// Check 5: the comments and attributes directly above the impl, up to
    /// eight lines, plus the impl's own line, read as prose.
    fn send_sync_justification(&mut self, imp: &ItemImpl) {
        let Some(unsafety) = imp.unsafety else {
            return;
        };
        let Some((None, path, _)) = &imp.trait_ else {
            return;
        };
        if !matches!(rust::last_ident(path).as_deref(), Some("Send" | "Sync")) {
            return;
        }
        let line = rust::line(unsafety.span);
        let mut ctx = self.text_line(line).to_lowercase();
        for n in (line.saturating_sub(8).max(1)..line).rev() {
            let text = self.text_line(n).trim_start();
            let prose = text.is_empty()
                || text.starts_with("//")
                || text.starts_with("/*")
                || text.starts_with('*')
                || text.starts_with("#[");
            if !prose {
                break;
            }
            ctx.push(' ');
            ctx.push_str(&text.to_lowercase());
        }
        const REFCOUNT_WORDS: &[&str] = &[
            "refcount",
            "refcnt",
            "reference count",
            "ref count",
            "inc_ref",
            "dec_ref",
            "refcounted",
            "ref-count",
        ];
        if REFCOUNT_WORDS.iter().any(|w| ctx.contains(w)) && ctx.contains("task") {
            self.hit(line, Check::RefcountSendSync);
        }
    }

    fn text_line(&self, line: usize) -> &str {
        line.checked_sub(1)
            .and_then(|i| self.lines.get(i))
            .copied()
            .unwrap_or("")
    }
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item(&mut self, i: &'ast Item) {
        self.gated(item_attrs(i), i, |v| visit::visit_item(v, i));
    }

    fn visit_impl_item(&mut self, i: &'ast ImplItem) {
        let attrs: &[Attribute] = match i {
            ImplItem::Const(c) => &c.attrs,
            ImplItem::Fn(f) => &f.attrs,
            ImplItem::Type(t) => &t.attrs,
            ImplItem::Macro(m) => &m.attrs,
            _ => &[],
        };
        self.gated(attrs, i, |v| visit::visit_impl_item(v, i));
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        let attrs: &[Attribute] = match i {
            TraitItem::Const(c) => &c.attrs,
            TraitItem::Fn(f) => &f.attrs,
            TraitItem::Type(t) => &t.attrs,
            TraitItem::Macro(m) => &m.attrs,
            _ => &[],
        };
        self.gated(attrs, i, |v| visit::visit_trait_item(v, i));
    }

    fn visit_stmt(&mut self, i: &'ast Stmt) {
        match i {
            Stmt::Local(l) => self.gated(&l.attrs, i, |v| visit::visit_stmt(v, i)),
            Stmt::Macro(m) => self.gated(&m.attrs, i, |v| visit::visit_stmt(v, i)),
            _ => visit::visit_stmt(self, i),
        }
    }

    fn visit_field(&mut self, i: &'ast Field) {
        self.gated(&i.attrs, i, |v| {
            v.binding(&i.ty);
            visit::visit_field(v, i);
        });
    }

    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.in_fn(&i.sig, |v| visit::visit_item_fn(v, i));
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.in_fn(&i.sig, |v| visit::visit_impl_item_fn(v, i));
    }

    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.in_fn(&i.sig, |v| visit::visit_trait_item_fn(v, i));
    }

    fn visit_signature(&mut self, i: &'ast Signature) {
        if unconstrained_lifetime(i) {
            self.hit(rust::line(i.fn_token.span), Check::FreeLifetime);
        }
        visit::visit_signature(self, i);
    }

    fn visit_pat_type(&mut self, i: &'ast PatType) {
        self.binding(&i.ty);
        visit::visit_pat_type(self, i);
    }

    fn visit_receiver(&mut self, i: &'ast syn::Receiver) {
        if i.colon_token.is_some() {
            self.binding(&i.ty);
        }
        visit::visit_receiver(self, i);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.binding(&i.ty);
        visit::visit_item_static(self, i);
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.binding(&i.ty);
        visit::visit_item_const(self, i);
    }

    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.binding(&i.ty);
        visit::visit_impl_item_const(self, i);
    }

    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        self.binding(&i.ty);
        visit::visit_trait_item_const(self, i);
    }

    fn visit_foreign_item_static(&mut self, i: &'ast syn::ForeignItemStatic) {
        self.binding(&i.ty);
        visit::visit_foreign_item_static(self, i);
    }

    fn visit_type_ptr(&mut self, i: &'ast syn::TypePtr) {
        if matches!(bare(&i.elem), Type::Ptr(inner) if named(&inner.elem, TASK_TYPES)) {
            self.hit(rust::line(i.star_token.span), Check::RawBinding);
        }
        visit::visit_type_ptr(self, i);
    }

    fn visit_path_segment(&mut self, i: &'ast PathSegment) {
        if i.ident == "KernelSync" && type_args(i).any(|t| ptr_to(t, TASK_TYPES)) {
            self.hit(rust::line(i.ident.span()), Check::KernelSync);
        }
        visit::visit_path_segment(self, i);
    }

    fn visit_expr_cast(&mut self, i: &'ast ExprCast) {
        let line = rust::line(i.as_token.span);
        if ptr_to(&i.ty, &["c_void"]) && names_task_ptr(&i.expr) {
            self.hit(line, Check::LaunderOut);
        }
        if ptr_to(&i.ty, &["Task"]) && self.c_void_fns.last() == Some(&true) {
            self.hit(line, Check::LaunderIn);
        }
        visit::visit_expr_cast(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast ExprMethodCall) {
        if i.method == "cast"
            && let Some(turbofish) = &i.turbofish
        {
            let line = rust::line(i.method.span());
            let targets = || {
                turbofish.args.iter().filter_map(|a| match a {
                    GenericArgument::Type(t) => Some(t),
                    _ => None,
                })
            };
            if targets().any(mentions_c_void) && names_task_ptr(&i.receiver) {
                self.hit(line, Check::LaunderOut);
            }
            if targets().any(|t| named(t, &["Task"]) || ptr_to(t, &["Task"]))
                && self.c_void_fns.last() == Some(&true)
            {
                self.hit(line, Check::LaunderIn);
            }
        }
        visit::visit_expr_method_call(self, i);
    }

    fn visit_item_impl(&mut self, i: &'ast ItemImpl) {
        self.send_sync_justification(i);
        visit::visit_item_impl(self, i);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
        visit::visit_macro(self, i);
    }
}

/// `ty` with any invisible or parenthesised grouping taken off.
fn bare(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => bare(&g.elem),
        Type::Paren(p) => bare(&p.elem),
        _ => ty,
    }
}

/// Whether `ty` is a path whose last segment is one of `names`.
fn named(ty: &Type, names: &[&str]) -> bool {
    match bare(ty) {
        Type::Path(p) => rust::last_ident(&p.path).is_some_and(|n| names.contains(&n.as_str())),
        _ => false,
    }
}

/// Whether `ty` is a raw pointer to one of `names`.
fn ptr_to(ty: &Type, names: &[&str]) -> bool {
    matches!(bare(ty), Type::Ptr(p) if named(&p.elem, names))
}

fn type_args(seg: &PathSegment) -> impl Iterator<Item = &Type> {
    let args = match &seg.arguments {
        PathArguments::AngleBracketed(a) => Some(a.args.iter()),
        _ => None,
    };
    args.into_iter().flatten().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// Whether a bound type holds a raw task pointer, looking through every
/// wrapper but `KernelSync` (check 2's), `PhantomData` (holds nothing) and a
/// callable's return type (return position).
fn holds_raw_task(ty: &Type) -> bool {
    struct Holds(bool);
    impl<'ast> Visit<'ast> for Holds {
        fn visit_type_ptr(&mut self, i: &'ast syn::TypePtr) {
            if named(&i.elem, TASK_TYPES) {
                self.0 = true;
            }
            visit::visit_type_ptr(self, i);
        }
        fn visit_path_segment(&mut self, i: &'ast PathSegment) {
            if i.ident != "KernelSync" && i.ident != "PhantomData" {
                visit::visit_path_segment(self, i);
            }
        }
        fn visit_return_type(&mut self, _: &'ast ReturnType) {}
    }
    let mut v = Holds(false);
    v.visit_type(ty);
    v.0
}

/// Whether an expression names a `*mut Task`/`*const Task` type anywhere.
fn names_task_ptr(expr: &Expr) -> bool {
    struct Names(bool);
    impl<'ast> Visit<'ast> for Names {
        fn visit_type_ptr(&mut self, i: &'ast syn::TypePtr) {
            if named(&i.elem, &["Task"]) {
                self.0 = true;
            }
            visit::visit_type_ptr(self, i);
        }
        fn visit_macro(&mut self, i: &'ast Macro) {
            visit_expansion(self, i);
        }
    }
    let mut v = Names(false);
    v.visit_expr(expr);
    v.0
}

fn mentions_c_void(ty: &Type) -> bool {
    struct Mentions(bool);
    impl<'ast> Visit<'ast> for Mentions {
        fn visit_path_segment(&mut self, i: &'ast PathSegment) {
            self.0 |= i.ident == "c_void";
            visit::visit_path_segment(self, i);
        }
    }
    let mut v = Mentions(false);
    v.visit_type(ty);
    v.0
}

/// Check 8: a lifetime declared on the fn's own generic list that the return
/// type names and no argument supplies.
fn unconstrained_lifetime(sig: &Signature) -> bool {
    let ReturnType::Type(_, ret) = &sig.output else {
        return false;
    };
    let declared: Vec<String> = sig
        .generics
        .lifetimes()
        .map(|l| l.lifetime.ident.to_string())
        .filter(|l| l != "static")
        .collect();
    if declared.is_empty() {
        return false;
    }
    let mut named = Lifetimes {
        names: BTreeSet::new(),
        mask_outlives: false,
    };
    named.visit_type(ret);
    let mut supplied = Lifetimes {
        names: BTreeSet::new(),
        mask_outlives: true,
    };
    for arg in &sig.inputs {
        match arg {
            FnArg::Typed(pt) => supplied.visit_type(&pt.ty),
            FnArg::Receiver(r) => supplied.visit_type(&r.ty),
        }
    }
    declared
        .iter()
        .any(|l| named.names.contains(l) && !supplied.names.contains(l))
}

/// The lifetimes a type could tie to its referent: not those inside
/// `PhantomData<..>`, a `fn(..)`/`Fn*(..)` parameter list or a `for<..>`
/// binder, nor (with `mask_outlives`) a `+ 'a` bound.
struct Lifetimes {
    names: BTreeSet<String>,
    mask_outlives: bool,
}

impl<'ast> Visit<'ast> for Lifetimes {
    fn visit_lifetime(&mut self, i: &'ast syn::Lifetime) {
        self.names.insert(i.ident.to_string());
    }

    fn visit_path_segment(&mut self, i: &'ast PathSegment) {
        if i.ident != "PhantomData" {
            visit::visit_path_segment(self, i);
        }
    }

    fn visit_type_bare_fn(&mut self, i: &'ast syn::TypeBareFn) {
        self.visit_return_type(&i.output);
    }

    fn visit_parenthesized_generic_arguments(
        &mut self,
        i: &'ast syn::ParenthesizedGenericArguments,
    ) {
        self.visit_return_type(&i.output);
    }

    fn visit_bound_lifetimes(&mut self, _: &'ast syn::BoundLifetimes) {}

    fn visit_type_param_bound(&mut self, i: &'ast TypeParamBound) {
        if !(self.mask_outlives && matches!(i, TypeParamBound::Lifetime(_))) {
            visit::visit_type_param_bound(self, i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const POSITIVES: &str = r#"fn takes_raw(task: *mut Task) {}
fn takes_inner(inner: *const TaskInner) {}
fn double(slot: *mut *mut Task) {}
struct Holder { slot: KernelSync<*mut Task> }
fn outbound(task: &mut Task) -> u64 {
    (task as *mut Task).cast::<c_void>() as u64
}
extern "C" fn shim(task_arg: *mut c_void) {
    let task_ptr = task_arg as *mut Task;
}
fn borrows() {
    task_borrow(ptr);
    task_borrow_mut(ptr);
}
/// Safe because the task refcount keeps the allocation alive for as
/// long as this handle exists.
unsafe impl Send for Holder {}
fn counts() {
    task_inc_ref(ptr);
    task_dec_ref(ptr);
    let n = slot.refcnt;
}
"#;

    const LIFETIMES_BAD: &str = r#"pub fn borrow_mut<'a, K, U>(p: *mut Payload<K, U>) -> Option<&'a mut Payload<K, U>> {
    None
}
pub fn name_bytes<'a, K, U>(p: *const Payload<K, U>) -> Option<&'a [u8]> {
    None
}
pub fn fpu_mut<'a, K, U>(p: *mut Payload<K, U>) -> &'a mut FpuState {
    todo!()
}
pub fn plain_ref<'a>(p: *const Payload) -> &'a Payload {
    todo!()
}
pub fn spans_lines<'a, K, U>(
    p: *const Payload<K, U>,
) -> Option<&'a crate::sync::AtomicCell<ExitInfo>> {
    None
}
pub fn phantom_bypass<'a, P>(p: *const P, _w: PhantomData<&'a ()>) -> &'a P {
    todo!()
}
pub fn fnptr_bypass<'a, P>(p: *const P, _f: fn(&'a ())) -> &'a P {
    todo!()
}
pub fn closure_bypass<'a, P>(p: *const P, g: impl FnOnce(&'a P)) -> &'a P {
    todo!()
}
pub fn async_bypass<'a, P>(p: *const P, g: impl AsyncFnOnce(&'a P)) -> &'a P {
    todo!()
}
pub fn outlives_bypass<'a, P>(p: *const P, g: KBox<dyn Fn(&'a P) + 'a>) -> &'a P {
    todo!()
}
pub fn wherefn<'a, P, F>(p: *const P, g: F) -> &'a P
where
    F: FnOnce(&'a P),
{
    todo!()
}
pub fn boundfn<'a, P, F: FnOnce(&'a P)>(p: *const P, g: F) -> &'a P {
    todo!()
}
pub fn split_generics
<'a, P>(p: *const P) -> &'a P {
    todo!()
}
pub fn ret_callable_ref<'a, P>(p: *const P) -> impl Fn() -> &'a P {
    todo!()
}
"#;

    const LIFETIMES_OK: &str = r#"// The lifetime IS tied to an argument — the whole point of the check.
pub fn tied<'a>(p: &'a Payload) -> &'a Foo {
    &p.foo
}
pub fn tied_mut<'a>(p: &'a mut Payload) -> Option<&'a mut Foo> {
    None
}
// Elided: bound to the argument by the elision rules.
pub fn elided(p: &Payload) -> &Foo {
    &p.foo
}
// A lifetime declared and used only in an argument.
pub fn consumes<'a>(p: &'a Payload) -> u32 {
    0
}
// 'static is a fixed lifetime, not one the caller picks.
pub fn statics<K, U>(p: *const Payload<K, U>) -> Option<&'static [u8]> {
    None
}
// No generic list at all.
pub fn no_generics(p: *mut Payload) -> u32 {
    0
}
// An arrow inside a bound must not close the generic list, and `F` is not
// a lifetime.
pub fn with_bound<'a, F: Fn(u32) -> u32>(p: &'a Payload, f: F) -> &'a Foo {
    &p.foo
}
// Higher-ranked bound in argument position: 'x is not declared on the fn.
pub fn hrtb<'a>(p: &'a Payload, f: impl for<'x> Fn(&'x u32)) -> &'a Foo {
    &p.foo
}
// Multi-line, but argument-tied.
pub fn tied_multiline<'a, K, U>(
    p: &'a Payload<K, U>,
) -> Option<&'a crate::sync::AtomicCell<ExitInfo>> {
    None
}
// A brand token the caller cannot conjure: private fields, invariant in
// 'brand, minted only inside a higher-ranked scope.
pub fn from_token<'brand, P>(t: BspToken<'brand>, p: *const P) -> &'brand P {
    todo!()
}
// An argument head supplies even when the same argument carries an
// outlives bound the mask blanks.
pub fn head_outlives<'a, P>(x: &'a P, g: impl Fn(&P) + 'a) -> &'a P {
    x
}
// The `where` strip exists for this: no reference is returned at all.
pub fn where_no_ref<'a, F>(x: u32, f: F) -> u32
where
    F: Fn(&'a u8),
{
    0
}
// Returning a marker hands the caller no reference to anything.
pub fn phantom_out<'a, P>(p: *const P) -> PhantomData<&'a P> {
    PhantomData
}
// Returning a callable that *takes* the borrow mints nothing either.
pub fn fnptr_out<'a, P>(p: *const P) -> fn(&'a P) {
    todo!()
}
"#;

    const NEGATIVES: &str = r#"pub type TaskEntry = extern "C" fn(*mut c_void);
pub struct TaskInnerFields { pub entry_arg: *mut c_void }
fn refs(task: &KArc<Task>, r: TaskRef, a: TaskAddr, s: TaskStack, b: TaskAbi) {}
fn entry_types(e: TaskEntry, r: *mut TaskRef, a: *const TaskAddr) {}
// A comment mentioning task_borrow and refcnt and *mut Task must not fire.
/* Block comment naming task_borrow_mut and *const TaskInner. */
fn spelled_out(handle: TaskHandle) -> TaskEntry { todo!() }
#[cfg(test)]
fn cfg_gated_raw(task: *mut Task) {}
unsafe impl Sync for Unrelated {}
fn opaque_payload(arg: *mut c_void) -> *mut c_void { arg }
"#;

    const PLACEMENT: &str = r#"fn takes_raw(task: *mut Task) {}
fn outbound(task: &mut Task) -> u64 {
    (task as *mut Task).cast::<c_void>() as u64
}
fn still_flagged() { task_borrow(ptr); }
"#;

    const FRAMES: &str = "fn map() { entry.inc_ref(); entry.dec_ref(); let n = entry.refcnt; }\n";

    const EXCEPTION: &str = r#"fn dump(id: u32) {
    let t = task_find_by_id(id);
    let c = task_find_by_cr3(cr3);
    if task_pointer_is_valid(p) {}
    let owned = handle.to_owned();
    let cloned = task_placement_clone(slot);
}
"#;

    /// The planted files, plus the named files whose absence is a finding.
    fn tree(files: &[(&str, &str)]) -> Tree {
        let tree = Tree::new(files);
        for surface in SANCTIONED_SURFACES.iter().chain(FAULT_PATH_FILES) {
            if !tree.repo.exists(surface) {
                tree.write(surface, "");
            }
        }
        tree
    }

    fn hits(files: &[(&str, &str)]) -> Vec<(String, usize, Check)> {
        let tree = tree(files);
        let mut hits = Hits::new();
        for (path, text) in files {
            scan_source(&Source::parse(path, text.to_string()).unwrap(), &mut hits);
        }
        for path in fault_path_files(&tree.repo).unwrap() {
            let text = tree.repo.read(&path).unwrap();
            scan_fault_path(&path, &text, &mut hits).unwrap();
        }
        hits.into_iter().collect()
    }

    fn count(hits: &[(String, usize, Check)], check: Check) -> usize {
        hits.iter().filter(|h| h.2 == check).count()
    }

    fn only(check: Check, text: &str) -> Vec<usize> {
        hits(&[("sched/src/x.rs", text)])
            .into_iter()
            .filter(|h| h.2 == check)
            .map(|h| h.1)
            .collect()
    }

    fn all(text: &str) -> Vec<(usize, Check)> {
        hits(&[("sched/src/x.rs", text)])
            .into_iter()
            .map(|h| (h.1, h.2))
            .collect()
    }

    #[test]
    fn self_test_fixtures_fire_at_their_counts() {
        let hits = hits(&[
            ("sched/src/task/positives.rs", POSITIVES),
            ("sched/src/task/negatives.rs", NEGATIVES),
            ("sched/src/task/lifetimes_bad.rs", LIFETIMES_BAD),
            ("sched/src/task/lifetimes_ok.rs", LIFETIMES_OK),
            ("slopos-ostd/src/task/placement.rs", PLACEMENT),
            ("mm/src/paging/frames.rs", FRAMES),
            ("boot/src/exception.rs", EXCEPTION),
        ]);
        assert_eq!(count(&hits, Check::RawBinding), 3, "{hits:?}");
        assert_eq!(count(&hits, Check::KernelSync), 1, "{hits:?}");
        assert_eq!(count(&hits, Check::LaunderOut), 1, "{hits:?}");
        assert_eq!(count(&hits, Check::LaunderIn), 1, "{hits:?}");
        assert_eq!(count(&hits, Check::BorrowAccessor), 3, "{hits:?}");
        assert_eq!(count(&hits, Check::RefcountSendSync), 1, "{hits:?}");
        assert_eq!(count(&hits, Check::ManualRefcount), 3, "{hits:?}");
        assert_eq!(count(&hits, Check::FaultPath), 5, "{hits:?}");
        assert_eq!(count(&hits, Check::FreeLifetime), 14, "{hits:?}");
        let silent: Vec<_> = hits
            .iter()
            .filter(|h| h.0.ends_with("negatives.rs") || h.0.ends_with("lifetimes_ok.rs"))
            .collect();
        assert!(silent.is_empty(), "false positives: {silent:?}");
    }

    #[test]
    fn positives_fire_on_their_lines() {
        let hits = all(POSITIVES);
        let want = [
            (1, Check::RawBinding),
            (2, Check::RawBinding),
            (3, Check::RawBinding),
            (4, Check::KernelSync),
            (6, Check::LaunderOut),
            (9, Check::LaunderIn),
            (12, Check::BorrowAccessor),
            (13, Check::BorrowAccessor),
            (17, Check::RefcountSendSync),
            (19, Check::ManualRefcount),
            (20, Check::ManualRefcount),
            (21, Check::ManualRefcount),
        ];
        assert_eq!(hits, want);
    }

    #[test]
    fn check_8_reports_at_the_fn_line() {
        let lines = only(Check::FreeLifetime, LIFETIMES_BAD);
        assert_eq!(lines, [1, 4, 7, 10, 13, 18, 21, 24, 27, 30, 33, 39, 42, 46]);
    }

    #[test]
    fn sanctioned_surface_keeps_only_check_4() {
        let hits = hits(&[("slopos-ostd/src/task/placement.rs", PLACEMENT)]);
        let checks: Vec<_> = hits.iter().map(|h| (h.1, h.2)).collect();
        assert_eq!(checks, [(5, Check::BorrowAccessor)]);
    }

    #[test]
    fn refcount_domain_is_exempt_from_check_6_only() {
        let text = "fn map() { entry.refcnt; task_borrow(p); }\n";
        let hits = hits(&[("slopos-ostd/src/mm/frames.rs", text)]);
        let checks: Vec<_> = hits.iter().map(|h| h.2).collect();
        assert_eq!(checks, [Check::BorrowAccessor]);
    }

    #[test]
    fn literals_and_comments_never_fire() {
        let text = r#"
fn f() {
    let a = "fn bad(task: *mut Task) task_borrow refcnt";
    let b = '}'; let c = "http://x"; // task_borrow(p) *mut Task
    /* KernelSync<*mut Task>
       task_inc_ref(p) */
    let d = r"unsafe impl Send for X {} // task refcount";
}
"#;
        assert!(all(text).is_empty(), "{:?}", all(text));
    }

    #[test]
    fn multi_line_binding_and_qualified_paths_fire() {
        let text = r#"
struct Slots {
    a: Option<*mut Task>,
    b: [*const crate::task::TaskInner<K, U>; 4],
    c: PhantomData<*mut Task>,
    d: fn() -> *mut Task,
    e: fn(*mut Task),
}
fn f(
    t:
        *mut Task,
) -> *mut Task {
    let x = |p: *const Task| p;
    t
}
static CURRENT: *mut Task = core::ptr::null_mut();
"#;
        assert_eq!(only(Check::RawBinding, text), [3, 4, 7, 11, 13, 16]);
    }

    #[test]
    fn return_position_is_not_binding_but_double_indirection_is() {
        let text = "fn f() -> *mut Task { g() }\nfn h() -> *mut *mut Task { g() }\n";
        assert_eq!(only(Check::RawBinding, text), [2]);
    }

    #[test]
    fn check_2_sees_qualified_and_turbofish_spellings() {
        let text = r#"
static S: sync::KernelSync<
    *const TaskInner<K, U>,
> = KernelSync::<*mut Task>::new(core::ptr::null_mut());
"#;
        assert_eq!(only(Check::KernelSync, text), [2, 4]);
    }

    #[test]
    fn launder_pair_across_lines_and_in_closures() {
        let text = r#"
fn out(task: &mut Task) -> *mut c_void {
    (task
        as *mut Task)
        .cast::<core::ffi::c_void>()
}
fn out2(t: &Task) -> u64 { t as *const Task as *const c_void as u64 }
extern "C" fn shim(
    arg: *mut core::ffi::c_void,
) {
    let run = || {
        let t = arg.cast::<Task>();
    };
}
fn not_c_void(arg: u64) -> *mut c_void {
    let t = arg as *mut Task;
}
"#;
        assert_eq!(only(Check::LaunderOut, text), [5, 7]);
        assert_eq!(only(Check::LaunderIn, text), [12]);
    }

    #[test]
    fn check_5_needs_both_words_directly_above() {
        let text = r#"
// SAFETY: the Task's reference count keeps it alive.
#[allow(unused)]
unsafe impl<T: Send> Sync
    for Holder<T> {}

// The task refcount is unrelated to this impl.
fn spacer() {}
unsafe impl Sync for Unrelated {}

/* ref-count of the task */
unsafe impl Send for Other {}
unsafe impl Sync for Plain {} // task refcnt
"#;
        assert_eq!(only(Check::RefcountSendSync, text), [4, 12, 13]);
    }

    #[test]
    fn cfg_test_items_are_skipped_whole() {
        let text = r#"
#[cfg(test)]
mod tests {
    fn helper(task: *mut Task) { task_borrow(task); x.refcnt; }
    fn free<'a>(p: *const u8) -> &'a u8 { todo!() }
}
#[cfg(feature = "x")]
fn gated(task: *mut Task) {}
"#;
        assert_eq!(all(text), [(8, Check::RawBinding)]);
    }

    #[test]
    fn macro_bodies_are_checked() {
        let text = r#"
macro_rules! define {
    ($name:ident, $($field:ident),*) => {
        pub fn $name<'a>(p: *const $crate::Payload) -> &'a u8 { todo!() }
        struct Fields { $($field: *mut Task,)* }
    };
}
define_things! {
    fn launder(task: &Task) -> *mut c_void { task as *const Task as *mut c_void }
}
fn f() {
    log!("{}", x.refcnt);
}
"#;
        assert_eq!(
            all(text),
            [
                (4, Check::FreeLifetime),
                (5, Check::RawBinding),
                (9, Check::LaunderOut),
                (12, Check::ManualRefcount),
            ]
        );
    }

    #[test]
    fn check_8_lookalikes() {
        let text = r#"
impl<'a> Holder<'a> {
    fn get(&self) -> &'a u8 { todo!() }
    fn slot<'b>(&'b self) -> &'b mut u8 { todo!() }
}
trait Mint {
    fn mint<'a>(addr: usize) -> &'a mut u8;
}
extern "C" {
    fn ext<'a>(p: *const u8) -> &'a u8;
}
fn ret_bound<'a>(x: &u8) -> impl Iterator<Item = u8> + 'a { todo!() }
fn arg_assoc<'a>(it: impl Iterator<Item = &'a u8>) -> &'a u8 { todo!() }
"#;
        assert_eq!(only(Check::FreeLifetime, text), [7, 10, 12]);
    }

    #[test]
    fn fault_path_covers_panic_modules_and_reads_identifiers() {
        let hits = hits(&[
            (
                "slopos-ostd/src/panic_dump.rs",
                "fn d() { s.into_owned(); }\n",
            ),
            (
                "slopos-ostd/src/arch/panic.rs",
                "fn d() { let s = \"to_owned\"; // task_find_by_id\n}\n",
            ),
            (
                "boot/src/idt.rs",
                "fn h() {\n    task_find_by_cr3_locked(c);\n}\n",
            ),
        ]);
        let fault: Vec<_> = hits
            .iter()
            .filter(|h| h.2 == Check::FaultPath)
            .map(|h| (h.0.as_str(), h.1))
            .collect();
        assert_eq!(
            fault,
            [("boot/src/idt.rs", 2), ("slopos-ostd/src/panic_dump.rs", 1)]
        );
    }

    #[test]
    fn run_reports_stale_entries_and_scope() {
        let tree = Tree::new(&[
            ("boot/src/exception.rs", "fn ok() {}\n"),
            ("userland/src/lib.rs", "fn takes_raw(task: *mut Task) {}\n"),
            ("sched/src/lib.rs", "fn takes_raw(task: *mut Task) {}\n"),
        ]);
        let report = run(&tree.repo).unwrap();
        let mut locations: Vec<_> = report.findings.iter().map(|f| f.location.clone()).collect();
        locations.sort();
        let mut want: Vec<String> = SANCTIONED_SURFACES.iter().map(|s| s.to_string()).collect();
        want.push("boot/src/idt.rs".into());
        want.push("sched/src/lib.rs:1".into());
        want.sort();
        assert_eq!(locations, want);
    }

    #[test]
    fn run_passes_a_clean_tree_and_says_what_it_scanned() {
        let tree = tree(&[("sched/src/lib.rs", "fn ok(t: TaskRef) {}\n")]);
        let report = run(&tree.repo).unwrap();
        assert!(report.passed(), "{:?}", report.findings);
        assert!(
            report.summary.contains("scanned 7 files"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn run_errors_on_an_empty_scope() {
        let tree = Tree::new(&[("userland/src/lib.rs", "fn f() {}\n")]);
        assert!(run(&tree.repo).is_err());
    }
}
