//! A `Charge` lives in exactly one place for exactly the lifetime of the
//! thing it accounts for.
//!
//! `Charge<A>` refunds in its `Drop`, is not `Clone`, and is minted only by
//! consuming a `Reservation`: unforgeable, never refunded twice. It is not
//! linear: Rust is affine, so safe code can skip a destructor (the drivers'
//! IRQ claims `core::mem::forget` inside `#![forbid(unsafe_code)]` crates).
//! A skipped refund is a row permanently short and a principal permanently
//! denied, so "a missing refund is unrepresentable" is false and must not be
//! claimed; this gate is what keeps the narrower invariant true.
//!
//! A value is charge-bearing when its declared type holds a `Charge<_>` or a
//! `Reservation<_>` by value, when its name says it holds one (`*_charge`,
//! `charge_*`: the binding that matters is often a local whose type is
//! inferred), or when it is `self` in an `impl` of `Charge` or `Reservation`.
//! Five checks, each a way to separate the token from its object:
//! - forget: `mem::forget`, `ManuallyDrop` or `.leak()` of a charge-bearing
//!   value;
//! - optional: an `Option<Charge<_>>` or `Option<Reservation<_>>` field, which
//!   `Option::take` empties safely, so the empty state must be its own type
//!   (an `Option<Reservation<_>>` return is the ordinary fallible mint);
//! - take: `.take()` on a field or binding whose name or declared type says
//!   it holds a charge (a `ChargeSlot` is the charge's single home for a kind
//!   whose refund point is not its holder's `Drop`, and its `take` refunds);
//! - escape: a `fn` returning `Charge<_>` by value outside the mint;
//! - clone: `#[derive(Clone)]` or `Copy` on a type with a charge-bearing
//!   field, since a cloned charge refunds twice.

use std::collections::BTreeSet;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, GenericArgument, Member, Pat, PathArguments, ReturnType, Type};

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source, macro_exprs};

/// Path prefixes of the userland-side crates and vendored code, which hold
/// no kernel accounting.
const OUT_OF_SCOPE: &[&str] = &[
    "userland",
    "terminal-core",
    "editor-core",
    "http-core",
    "tls-core",
    "slibc",
    "slop-protocol",
    "appkit",
    "image",
    "slopos-rt",
    "vendor/",
];

/// `(check, path prefix)` exemptions; each must match a finding, since a dead
/// exemption is how a scan quietly stops covering a file. `token.rs` is the
/// mint: its `into_parts` helpers are the only sanctioned forget of a token
/// (they carry a debit forward from a `Reservation` into a `Charge`, or out of
/// a `Charge` into an explicit refund), and `commit`, `try_extend` and
/// `shrink` are the minters, so their return types are expected.
const ALLOWLIST: &[(Check, &str)] = &[
    (Check::Forget, "slopos-ostd/src/process/quota/token.rs"),
    (Check::Escape, "slopos-ostd/src/process/quota/token.rs"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    Forget,
    Optional,
    Take,
    Escape,
    Clone,
}

impl Check {
    fn name(self) -> &'static str {
        match self {
            Check::Forget => "forget",
            Check::Optional => "optional",
            Check::Take => "take",
            Check::Escape => "escape",
            Check::Clone => "clone",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    check: Check,
    path: String,
    line: usize,
    message: String,
}

pub fn run(repo: &Repo) -> Result<Report> {
    let (hits, files) = scan(repo)?;
    let mut report = Report::new();
    let mut used = vec![false; ALLOWLIST.len()];
    let mut exempt = 0;
    for hit in hits {
        match ALLOWLIST
            .iter()
            .position(|(check, prefix)| *check == hit.check && hit.path.starts_with(prefix))
        {
            Some(i) => {
                used[i] = true;
                exempt += 1;
            }
            None => report.at(&hit.path, hit.line, hit.message),
        }
    }
    for ((check, prefix), used) in ALLOWLIST.iter().zip(used) {
        if !used {
            report.finding(
                format!("allowlist `{}:{prefix}`", check.name()),
                "the exemption matches nothing — remove it, since a dead exemption is how a \
                 scan quietly stops covering a file",
            );
        }
    }
    report.summary(format!(
        "scanned {files} files outside the userland-side crates for the five ways to separate a \
         charge from its object; {exempt} sanctioned in the mint, none elsewhere"
    ));
    Ok(report)
}

fn in_scope(path: &str) -> bool {
    !OUT_OF_SCOPE.iter().any(|p| path.starts_with(p))
}

fn scan(repo: &Repo) -> Result<(Vec<Hit>, usize)> {
    let files: Vec<String> = repo
        .rs_files()?
        .into_iter()
        .filter(|f| in_scope(f))
        .collect();
    if files.is_empty() {
        return Err(format!(
            "no Rust sources outside the userland-side crates under {} — the scan would be a \
             no-op",
            repo.root.display()
        )
        .into());
    }
    let mut hits = BTreeSet::new();
    for path in &files {
        let src = Source::load(repo, path)?;
        let mut names = Names::default();
        names.visit_file(&src.file);
        let mut v = Checks {
            path,
            names: &names,
            impl_of: Vec::new(),
            hits: &mut hits,
        };
        v.visit_file(&src.file);
    }
    Ok((hits.into_iter().collect(), files.len()))
}

fn is_token(ident: &str) -> bool {
    ident == "Charge" || ident == "Reservation"
}

/// Whether a name says it holds a charge.
fn charge_named(name: &str) -> bool {
    name.contains("_charge") || name.contains("charge_")
}

/// Whether `ty` holds a `Charge` or a `Reservation` by value, `Self` counting
/// as one when `self_is_token`.
fn bears(ty: &Type, self_is_token: bool) -> bool {
    match ty {
        Type::Path(p) => {
            p.qself
                .as_ref()
                .is_some_and(|q| bears(&q.ty, self_is_token))
                || p.path.segments.iter().any(|s| {
                    let ident = s.ident.to_string();
                    is_token(&ident)
                        || (self_is_token && ident == "Self")
                        || generic_types(&s.arguments).any(|t| bears(t, self_is_token))
                })
        }
        Type::Tuple(t) => t.elems.iter().any(|t| bears(t, self_is_token)),
        Type::Array(a) => bears(&a.elem, self_is_token),
        Type::Slice(s) => bears(&s.elem, self_is_token),
        Type::Paren(p) => bears(&p.elem, self_is_token),
        Type::Group(g) => bears(&g.elem, self_is_token),
        _ => false,
    }
}

fn generic_types(args: &PathArguments) -> impl Iterator<Item = &Type> {
    let args = match args {
        PathArguments::AngleBracketed(a) => Some(a.args.iter()),
        _ => None,
    };
    args.into_iter().flatten().filter_map(|a| match a {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

/// The `Option<Charge<_>>` / `Option<Reservation<_>>` inside `ty`, if any.
fn optional_token(ty: &Type) -> Option<&syn::PathSegment> {
    match ty {
        Type::Path(p) => p.path.segments.iter().find_map(|s| {
            let direct = s.ident == "Option"
                && generic_types(&s.arguments).any(|t| match t {
                    Type::Path(inner) => {
                        rust::last_ident(&inner.path).is_some_and(|i| is_token(&i))
                    }
                    _ => false,
                });
            if direct {
                Some(s)
            } else {
                generic_types(&s.arguments).find_map(optional_token)
            }
        }),
        Type::Tuple(t) => t.elems.iter().find_map(optional_token),
        Type::Array(a) => optional_token(&a.elem),
        Type::Slice(s) => optional_token(&s.elem),
        Type::Paren(p) => optional_token(&p.elem),
        Type::Group(g) => optional_token(&g.elem),
        _ => None,
    }
}

fn top_ident(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => rust::last_ident(&p.path),
        _ => None,
    }
}

/// The names one file declares, by what their declared type holds. Name
/// based, file wide, like the bindings the checks are about: a field and the
/// local it is moved into are both reached by name.
#[derive(Default)]
struct Names {
    charged: BTreeSet<String>,
    optional: BTreeSet<String>,
    slots: BTreeSet<String>,
}

impl Names {
    fn declare(&mut self, name: String, ty: &Type) {
        if top_ident(ty).as_deref() == Some("ChargeSlot") {
            self.slots.insert(name);
            return;
        }
        if optional_token(ty).is_some() {
            self.optional.insert(name.clone());
        }
        if bears(ty, false) {
            self.charged.insert(name);
        }
    }
}

impl<'ast> Visit<'ast> for Names {
    fn visit_pat_type(&mut self, p: &'ast syn::PatType) {
        if let Pat::Ident(i) = p.pat.as_ref() {
            self.declare(i.ident.to_string(), &p.ty);
        }
        visit::visit_pat_type(self, p);
    }

    fn visit_field(&mut self, f: &'ast syn::Field) {
        if let Some(ident) = &f.ident {
            self.declare(ident.to_string(), &f.ty);
        }
        visit::visit_field(self, f);
    }
}

struct Checks<'a> {
    path: &'a str,
    names: &'a Names,
    /// The last path segment of each enclosing `impl`'s self type.
    impl_of: Vec<Option<String>>,
    hits: &'a mut BTreeSet<Hit>,
}

impl Checks<'_> {
    fn hit(&mut self, check: Check, line: usize, message: String) {
        self.hits.insert(Hit {
            check,
            path: self.path.to_string(),
            line,
            message,
        });
    }

    fn self_is_token(&self) -> bool {
        self.impl_of
            .last()
            .and_then(Option::as_deref)
            .is_some_and(is_token)
    }

    fn self_is_charge(&self) -> bool {
        self.impl_of.last().and_then(Option::as_deref) == Some("Charge")
    }

    fn bearing(&self, e: &Expr) -> bool {
        let mut v = Bearing {
            names: self.names,
            self_is_token: self.self_is_token(),
            found: false,
        };
        v.visit_expr(e);
        v.found
    }

    fn forget(&mut self, line: usize, what: &str) {
        self.hit(
            Check::Forget,
            line,
            format!(
                "{what} of a charge-bearing value skips its refund — let it drop where it \
                 accounts for its object, or move it to the one place that does"
            ),
        );
    }

    fn signature(&mut self, sig: &syn::Signature) {
        let ReturnType::Type(_, ty) = &sig.output else {
            return;
        };
        let escapes = match top_ident(ty).as_deref() {
            Some("Charge") => true,
            Some("Self") => self.self_is_charge(),
            _ => false,
        };
        if escapes {
            self.hit(
                Check::Escape,
                rust::line(sig.ident.span()),
                format!(
                    "`{}` hands a `Charge` back by value outside the mint, separating it from \
                     what it accounts for — keep it in its object and lend `&Charge` instead",
                    sig.ident
                ),
            );
        }
    }

    fn derives(&mut self, attrs: &[syn::Attribute], ident: &syn::Ident, fields: Vec<&syn::Field>) {
        let mut derived = Vec::new();
        for attr in attrs.iter().filter(|a| a.path().is_ident("derive")) {
            let _ = attr.parse_nested_meta(|m| {
                if let Some(name) = m
                    .path
                    .get_ident()
                    .map(ToString::to_string)
                    .or_else(|| rust::last_ident(&m.path))
                    && (name == "Clone" || name == "Copy")
                {
                    derived.push(name);
                }
                Ok(())
            });
        }
        if derived.is_empty() {
            return;
        }
        if let Some(field) = fields.into_iter().find(|f| bears(&f.ty, false)) {
            self.hit(
                Check::Clone,
                rust::line(field.ty.span()),
                format!(
                    "`#[derive({})]` on `{ident}`, which holds a charge — a cloned charge \
                     refunds twice; drop the derive",
                    derived.join(", ")
                ),
            );
        }
    }
}

impl<'ast> Visit<'ast> for Checks<'_> {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        self.impl_of.push(top_ident(&i.self_ty));
        visit::visit_item_impl(self, i);
        self.impl_of.pop();
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.impl_of.push(None);
        self.signature(&f.sig);
        visit::visit_item_fn(self, f);
        self.impl_of.pop();
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.signature(&f.sig);
        visit::visit_impl_item_fn(self, f);
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        self.signature(&f.sig);
        visit::visit_trait_item_fn(self, f);
    }

    fn visit_item_struct(&mut self, s: &'ast syn::ItemStruct) {
        self.derives(&s.attrs, &s.ident, s.fields.iter().collect());
        visit::visit_item_struct(self, s);
    }

    fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
        let fields = e.variants.iter().flat_map(|v| v.fields.iter()).collect();
        self.derives(&e.attrs, &e.ident, fields);
        visit::visit_item_enum(self, e);
    }

    fn visit_item_union(&mut self, u: &'ast syn::ItemUnion) {
        self.derives(&u.attrs, &u.ident, u.fields.named.iter().collect());
        visit::visit_item_union(self, u);
    }

    fn visit_field(&mut self, f: &'ast syn::Field) {
        if let Some(seg) = optional_token(&f.ty) {
            self.hit(
                Check::Optional,
                rust::line(seg.ident.span()),
                "an `Option<Charge<_>>` field lets `Option::take` separate the charge from what \
                 it accounts for — give the empty state its own uncharged type"
                    .to_string(),
            );
        }
        visit::visit_field(self, f);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(p) = call.func.as_ref() {
            let path = &p.path;
            let what = if rust::path_ends_with(path, &["mem", "forget"]) {
                Some("`mem::forget`")
            } else if rust::path_ends_with(path, &["ManuallyDrop", "new"]) {
                Some("`ManuallyDrop::new`")
            } else if path.segments.len() >= 2 && rust::last_ident(path).as_deref() == Some("leak")
            {
                Some("`leak`")
            } else {
                None
            };
            let turbofish = path
                .segments
                .iter()
                .any(|s| generic_types(&s.arguments).any(|t| bears(t, self.self_is_token())));
            if let Some(what) = what
                && (turbofish || call.args.first().is_some_and(|a| self.bearing(a)))
            {
                let line = rust::line(path.segments.last().map_or(p.span(), |s| s.ident.span()));
                self.forget(line, what);
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        if mc.args.is_empty() {
            let line = rust::line(mc.method.span());
            if mc.method == "leak" && self.bearing(&mc.receiver) {
                self.forget(line, "`.leak()`");
            }
            if mc.method == "take"
                && let Some(name) = final_name(&mc.receiver)
                && !self.names.slots.contains(&name)
                && (charge_named(&name)
                    || name.contains("Charge")
                    || name.contains("Reservation")
                    || self.names.optional.contains(&name))
            {
                self.hit(
                    Check::Take,
                    line,
                    format!(
                        "`.take()` on `{name}` separates a charge from what it accounts for — \
                         keep it in place, or make its home a `ChargeSlot`, whose `take` refunds"
                    ),
                );
            }
        }
        visit::visit_expr_method_call(self, mc);
    }

    fn visit_type_path(&mut self, t: &'ast syn::TypePath) {
        if let Some(seg) = t.path.segments.last()
            && seg.ident == "ManuallyDrop"
            && generic_types(&seg.arguments).any(|ty| bears(ty, self.self_is_token()))
        {
            self.forget(rust::line(seg.ident.span()), "`ManuallyDrop`");
        }
        visit::visit_type_path(self, t);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        for e in macro_exprs(mac) {
            self.visit_expr(&e);
        }
    }
}

/// The name a receiver is reached by: the field, or the binding.
fn final_name(e: &Expr) -> Option<String> {
    match e {
        Expr::Field(f) => match &f.member {
            Member::Named(i) => Some(i.to_string()),
            Member::Unnamed(_) => None,
        },
        Expr::Path(p) => rust::last_ident(&p.path),
        Expr::Paren(p) => final_name(&p.expr),
        Expr::Reference(r) => final_name(&r.expr),
        _ => None,
    }
}

/// Whether an expression mentions a charge-bearing value.
struct Bearing<'a> {
    names: &'a Names,
    self_is_token: bool,
    found: bool,
}

impl Bearing<'_> {
    fn named(&self, name: &str) -> bool {
        self.names.charged.contains(name) || charge_named(name)
    }
}

impl<'ast> Visit<'ast> for Bearing<'_> {
    fn visit_expr_path(&mut self, p: &'ast syn::ExprPath) {
        if let Some(ident) = p.path.get_ident() {
            let ident = ident.to_string();
            self.found |= self.named(&ident) || (ident == "self" && self.self_is_token);
        } else {
            self.found |= p
                .path
                .segments
                .iter()
                .any(|s| is_token(&s.ident.to_string()));
        }
        visit::visit_expr_path(self, p);
    }

    fn visit_member(&mut self, m: &'ast Member) {
        if let Member::Named(i) = m {
            self.found |= self.named(&i.to_string());
        }
    }

    fn visit_type(&mut self, t: &'ast Type) {
        self.found |= bears(t, self.self_is_token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    fn hits(files: &[(&str, &str)]) -> Vec<Hit> {
        let tree = Tree::new(files);
        scan(&tree.repo).unwrap().0
    }

    fn count(hits: &[Hit], check: Check) -> usize {
        hits.iter().filter(|h| h.check == check).count()
    }

    fn lines(hits: &[Hit], check: Check) -> Vec<usize> {
        hits.iter()
            .filter(|h| h.check == check)
            .map(|h| h.line)
            .collect()
    }

    const POSITIVES: &str = "\
struct Leaky {
    slot_charge: Charge<FdSlot>,
}

fn escapes(c: Charge<FdSlot>) -> Charge<FdSlot> { c }

struct Optional {
    slot_charge: Option<Charge<FdSlot>>,
}

fn drops_it(c: Charge<FdSlot>) {
    core::mem::forget(c);
}

fn steals(e: &mut Optional) {
    let _ = e.slot_charge.take();
}

#[derive(Clone)]
struct Cloneable {
    slot_charge: Charge<FdSlot>,
}
";

    const NEGATIVES: &str = "\
// A forget of something that is not a charge: the live escape this gate cites,
// which must stay accepted or the gate cries wolf on real driver code.
fn irq_claim(handle: Handle, line: Line) {
    core::mem::forget(handle);
    core::mem::forget(line);
}

// A charge stored by value with no Clone in sight.
struct Proper {
    slot_charge: Charge<FdSlot>,
}

// Borrowing a charge is fine: it cannot be separated from its owner.
impl Proper {
    fn object_charge(&self) -> &Charge<ObjectRow> { &self.other }
}

// An Option of something else entirely.
struct Unrelated {
    backing: Option<KArc<dyn FileBacking>>,
}

// A take on a field that is not a charge.
fn take_entry(slot: &mut Slot) {
    let _ = slot.descriptors.take();
}

// `ChargeSlot::take` IS the sanctioned release for a kind whose refund point
// is not its holder's Drop — the slot is the charge's single home, and taking
// from it refunds.
struct Reaped {
    proc_charge: quota::ChargeSlot<ProcCount>,
}

impl Reaped {
    fn release(&self) {
        self.proc_charge.take();
    }
}

// The ordinary fallible mint: `None` means no debit was taken, not a charge
// that went missing.
fn reserve(account: AccountId) -> Option<Reservation<TaskCount>> {
    try_charge::<TaskCount>(account, 1).ok()
}

// Clone on a struct with no charge field.
#[derive(Clone, Copy)]
struct Flags {
    cloexec: bool,
}
";

    #[test]
    fn script_fixtures_fire_at_the_same_counts() {
        let found = hits(&[
            ("src/positives.rs", POSITIVES),
            ("src/negatives.rs", NEGATIVES),
        ]);
        assert_eq!(count(&found, Check::Forget), 1, "{found:#?}");
        assert_eq!(count(&found, Check::Optional), 1, "{found:#?}");
        assert_eq!(count(&found, Check::Take), 1, "{found:#?}");
        assert_eq!(count(&found, Check::Escape), 1, "{found:#?}");
        assert_eq!(count(&found, Check::Clone), 1, "{found:#?}");
        assert!(
            found.iter().all(|h| h.path == "src/positives.rs"),
            "{found:#?}"
        );
        assert_eq!(
            [
                Check::Forget,
                Check::Optional,
                Check::Take,
                Check::Escape,
                Check::Clone
            ]
            .map(|c| lines(&found, c)),
            [[12], [8], [16], [5], [21]]
        );
    }

    #[test]
    fn the_userland_side_is_out_of_scope_and_an_empty_scan_fails() {
        let found = hits(&[
            ("src/negatives.rs", NEGATIVES),
            ("userland/src/a.rs", POSITIVES),
            ("vendor/x/src/a.rs", POSITIVES),
        ]);
        assert!(found.is_empty(), "{found:#?}");
        let tree = Tree::new(&[("slibc/src/a.rs", POSITIVES)]);
        assert!(scan(&tree.repo).is_err());
    }

    #[test]
    fn syntax_the_line_scan_misread() {
        let found = hits(&[(
            "src/a.rs",
            r#"
fn multi_line_signature(
    c: Reservation<FdSlot>,
) -> quota::Charge<FdSlot> {
    Charge::commit(c)
}
struct Wrapped {
    held: Vec<Option<quota::Charge<FdSlot>>>,
}
fn split_call(c: Charge<FdSlot>) {
    core::mem::forget(
        c,
    );
}
fn the_mint_forgets_itself() {}
impl<A> Charge<A> {
    fn into_parts(self) -> (u64, u32) { core::mem::forget(self); (0, 0) }
    fn commit(r: Reservation<A>) -> Self { todo!() }
}
impl Other {
    fn not_a_charge(self) -> Self { core::mem::forget(self); self }
}
fn in_strings_and_comments() {
    let s = "core::mem::forget(slot_charge); x.slot_charge.take()";
    /* core::mem::forget(c); */
}
fn leaked(r: Reservation<FdSlot>) -> &'static mut Reservation<FdSlot> {
    KBox::leak(KBox::new(r))
}
fn manually(c: Charge<FdSlot>) {
    let m = ManuallyDrop::new(c);
    let n: ManuallyDrop<Charge<FdSlot>> = todo!();
}
fn inside_a_macro(c: Charge<FdSlot>) {
    debug_assert!({ core::mem::forget(c); true });
}
#[derive(Debug, Copy)]
#[repr(C)]
enum Shared {
    Owner(Charge<ObjectRow>),
}
fn named_by_declared_type(e: &mut E) {
    let _ = e.held.take();
}
"#,
        )]);
        assert_eq!(lines(&found, Check::Escape), [2, 18], "{found:#?}");
        assert_eq!(lines(&found, Check::Optional), [8], "{found:#?}");
        assert_eq!(
            lines(&found, Check::Forget),
            [11, 17, 28, 31, 32, 35],
            "{found:#?}"
        );
        assert_eq!(lines(&found, Check::Clone), [40], "{found:#?}");
        assert_eq!(lines(&found, Check::Take), [43], "{found:#?}");
    }

    #[test]
    fn a_dead_exemption_fails_the_real_run() {
        let tree = Tree::new(&[("src/a.rs", "fn f() {}\n")]);
        let report = run(&tree.repo).unwrap();
        assert_eq!(report.findings.len(), ALLOWLIST.len());
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.message.contains("matches nothing"))
        );
    }

    #[test]
    fn the_mint_is_exempt_for_forget_and_escape_only() {
        let tree = Tree::new(&[
            (
                "slopos-ostd/src/process/quota/token.rs",
                "impl<A> Charge<A> {\n    fn commit(r: Reservation<A>) -> Self { todo!() }\n    \
                 fn into_parts(self) { core::mem::forget(self); }\n}\n",
            ),
            (
                "slopos-ostd/src/process/quota/other.rs",
                "struct S { c: Option<Charge<X>> }\n",
            ),
        ]);
        let report = run(&tree.repo).unwrap();
        let locations: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        assert_eq!(
            locations,
            ["slopos-ostd/src/process/quota/other.rs:1"],
            "{report:#?}"
        );
    }
}
