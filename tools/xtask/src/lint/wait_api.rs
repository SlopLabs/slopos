//! slopos-ostd's blocking wait API as the wait gates see it: each
//! `wait_event*` function declared returning `WaitResult`, and where it takes
//! its predicate. Read from the sources on every run, so a new entry point is
//! covered the day it is declared.

use std::collections::{BTreeMap, BTreeSet};

use syn::visit::{self, Visit};
use syn::{
    FnArg, GenericParam, Generics, ReturnType, Signature, Type, TypeParamBound, WherePredicate,
};

use crate::Result;
use crate::repo::Repo;
use crate::rust::{self, Source};

struct DeclVisitor<'a> {
    entries: &'a mut EntryPoints,
    path: &'a str,
    error: Option<String>,
}

impl DeclVisitor<'_> {
    fn declare(&mut self, sig: &Signature) {
        let name = sig.ident.to_string();
        if !name.starts_with("wait_event") || !returns_wait_result(&sig.output) {
            return;
        }
        let predicate = predicate_index(sig);
        match self.entries.0.insert(name.clone(), predicate) {
            Some(prev) if prev != predicate && self.error.is_none() => {
                self.error = Some(format!(
                    "{}:{}: `{name}` takes its predicate at argument {predicate:?} here and \
                     {prev:?} elsewhere; one name, one predicate position",
                    self.path,
                    rust::line(sig.ident.span())
                ));
            }
            _ => {}
        }
    }
}

impl<'ast> Visit<'ast> for DeclVisitor<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.declare(&f.sig);
        visit::visit_item_fn(self, f);
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.declare(&f.sig);
        visit::visit_impl_item_fn(self, f);
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        self.declare(&f.sig);
        visit::visit_trait_item_fn(self, f);
    }
}

fn returns_wait_result(output: &ReturnType) -> bool {
    match output {
        ReturnType::Type(_, ty) => match ty.as_ref() {
            Type::Path(p) => rust::last_ident(&p.path).as_deref() == Some("WaitResult"),
            _ => false,
        },
        ReturnType::Default => false,
    }
}

/// The index, receiver excluded, of the first parameter typed as a closure: a
/// generic bounded by `Fn`/`FnMut`/`FnOnce`, or an `impl Fn*`.
fn predicate_index(sig: &Signature) -> Option<usize> {
    let closures = closure_generics(&sig.generics);
    sig.inputs
        .iter()
        .filter_map(|arg| match arg {
            FnArg::Typed(t) => Some(&t.ty),
            FnArg::Receiver(_) => None,
        })
        .position(|ty| match ty.as_ref() {
            Type::Path(p) => {
                p.qself.is_none()
                    && p.path
                        .get_ident()
                        .is_some_and(|i| closures.contains(&i.to_string()))
            }
            Type::ImplTrait(t) => t.bounds.iter().any(is_fn_bound),
            _ => false,
        })
}

fn closure_generics(generics: &Generics) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in &generics.params {
        if let GenericParam::Type(t) = p
            && t.bounds.iter().any(is_fn_bound)
        {
            out.insert(t.ident.to_string());
        }
    }
    if let Some(w) = &generics.where_clause {
        for p in &w.predicates {
            if let WherePredicate::Type(t) = p
                && let Type::Path(ty) = &t.bounded_ty
                && let Some(ident) = ty.path.get_ident()
                && t.bounds.iter().any(is_fn_bound)
            {
                out.insert(ident.to_string());
            }
        }
    }
    out
}

fn is_fn_bound(b: &TypeParamBound) -> bool {
    matches!(b, TypeParamBound::Trait(t)
        if matches!(rust::last_ident(&t.path).as_deref(), Some("Fn" | "FnMut" | "FnOnce")))
}

/// Where the wait API is declared.
const API_DIR: &str = "slopos-ostd/src";

/// The blocking wait entry points: each `wait_event*` name slopos-ostd
/// declares returning `WaitResult`, with the index (among the arguments a
/// method call passes) of its predicate, when it takes one.
#[derive(Debug, Default)]
pub(super) struct EntryPoints(BTreeMap<String, Option<usize>>);

impl EntryPoints {
    pub(super) fn load(repo: &Repo) -> Result<Self> {
        let mut entries = Self::default();
        for path in repo.rs_files_under(&[API_DIR])? {
            let src = Source::load(repo, &path)?;
            let mut v = DeclVisitor {
                entries: &mut entries,
                path: &path,
                error: None,
            };
            v.visit_file(&src.file);
            if let Some(e) = v.error {
                return Err(e.into());
            }
        }
        if entries.0.is_empty() {
            return Err(format!(
                "no `wait_event*` function returning `WaitResult` is declared under {API_DIR} \
                 — without the wait API there is nothing to tell a wait call from any other"
            )
            .into());
        }
        Ok(entries)
    }

    pub(super) fn contains(&self, name: &str) -> bool {
        self.0.contains_key(name)
    }

    /// The predicate's index among a method call's arguments.
    pub(super) fn predicate(&self, name: &str) -> Option<usize> {
        self.0.get(name).copied().flatten()
    }

    #[cfg(test)]
    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::fixture::Tree;

    /// Every name the line-based gate matched:
    /// `wait_event(_interruptible)?(_timeout)?(_until)?`.
    const SCRIPT_ENTRY_POINTS: &[&str] = &[
        "wait_event",
        "wait_event_until",
        "wait_event_timeout",
        "wait_event_timeout_until",
        "wait_event_interruptible",
        "wait_event_interruptible_until",
        "wait_event_interruptible_timeout",
        "wait_event_interruptible_timeout_until",
    ];

    /// A stand-in for slopos-ostd's wait API, so a fixture tree knows which
    /// calls are waits. Every name takes its predicate first.
    pub(in crate::lint) fn api() -> (&'static str, String) {
        let mut text =
            String::from("pub type WaitResult<R> = Result<R, WaitAbort>;\nimpl WaitQueue {\n");
        for name in SCRIPT_ENTRY_POINTS
            .iter()
            .chain(&["wait_event_uninterruptible_timeout_until"])
        {
            text.push_str(&format!(
                "    pub fn {name}<F, R>(&self, condition: F, timeout_ms: u64) -> WaitResult<R> \
                 where F: FnMut() -> Option<R> {{ self.wait_core(condition) }}\n"
            ));
        }
        text.push_str("}\n");
        ("slopos-ostd/src/sync/wait_queue.rs", text)
    }

    fn real_repo() -> Repo {
        Repo::at(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap(),
        )
    }

    #[test]
    fn the_wait_api_takes_every_predicate_first_and_covers_the_script_list() {
        let entries = EntryPoints::load(&real_repo()).unwrap();
        for name in SCRIPT_ENTRY_POINTS {
            assert!(
                entries.contains(name),
                "{name} is no longer a WaitResult entry point"
            );
        }
        assert!(entries.contains("wait_event_uninterruptible_timeout_until"));
        for name in entries.names() {
            assert_eq!(
                entries.predicate(name),
                Some(0),
                "{name}: predicate is not the first argument"
            );
        }
    }

    #[test]
    fn a_name_declared_with_two_predicate_positions_is_an_error() {
        let tree = Tree::new(&[(
            "slopos-ostd/src/a.rs",
            "impl A { fn wait_event<F: FnMut() -> bool>(&self, f: F) -> WaitResult<()> {} }\n\
             impl B { fn wait_event<F: FnMut() -> bool>(&self, t: u64, f: F) -> WaitResult<()> {} }\n",
        )]);
        let err = EntryPoints::load(&tree.repo).err().unwrap().to_string();
        assert!(err.contains("one predicate position"), "{err}");
    }
}
