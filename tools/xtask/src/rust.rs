//! Rust sources as syntax trees: what the source gates match against instead
//! of lines, so a brace in a string, a `//` in a URL or a header split across
//! lines reads as the compiler reads it.

use proc_macro2::{Delimiter, Group, Span, TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit::Visit;
use syn::{
    Attribute, Block, Expr, ForeignItem, ImplItem, Item, Macro, Meta, Path, Stmt, Token, TraitItem,
};

use crate::Result;
use crate::repo::Repo;

/// A parsed source file, with its repo-relative path for findings.
pub struct Source {
    pub path: String,
    pub text: String,
    pub file: syn::File,
}

impl Source {
    pub fn parse(path: &str, text: String) -> Result<Self> {
        let file =
            syn::parse_file(&text).map_err(|e| format!("{path}:{}: {e}", e.span().start().line))?;
        Ok(Self {
            path: path.to_string(),
            text,
            file,
        })
    }

    pub fn load(repo: &Repo, path: &str) -> Result<Self> {
        Self::parse(path, repo.read(path)?)
    }
}

/// The 1-based line a span starts on.
pub fn line(span: Span) -> usize {
    span.start().line
}

/// Whether the attributes carry a `#[cfg(...)]` — the item exists only in some
/// configurations, so a gate about the kernel build may leave it to the
/// configuration that compiles it.
pub fn is_cfg_gated(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| a.path().is_ident("cfg"))
}

/// Whether the attributes carry `#[cfg(test)]`, alone or inside `all(...)`.
pub fn is_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && match &a.meta {
                Meta::List(list) => {
                    list.tokens.to_string().split_whitespace().any(|t| {
                        t.trim_matches(|c: char| !c.is_alphanumeric() && c != '_') == "test"
                    })
                }
                _ => false,
            }
    })
}

/// Whether an attribute list names `#[<name>...]` (outer path only).
pub fn has_attr(attrs: &[Attribute], name: &str) -> bool {
    attrs.iter().any(|a| a.path().is_ident(name))
}

/// Whether `path`'s trailing segments are exactly `tail`: `ends_with(p,
/// &["mem", "forget"])` holds for `mem::forget` and `core::mem::forget`.
pub fn path_ends_with(path: &Path, tail: &[&str]) -> bool {
    let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
    segs.len() >= tail.len()
        && segs[segs.len() - tail.len()..]
            .iter()
            .zip(tail)
            .all(|(a, b)| a == b)
}

/// The last segment of a path, as text.
pub fn last_ident(path: &Path) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

/// Whether an item, statement or match arm sits under `#[cfg(...)]`: it
/// exists only in some configurations, and the source gates leave it — the
/// whole subtree — to the build that compiles it (rustc's own lints, the
/// expansion gate). Visitors return early on these.
pub fn item_gated(i: &Item) -> bool {
    is_cfg_gated(item_attrs(i))
}

pub fn impl_item_gated(i: &ImplItem) -> bool {
    is_cfg_gated(match i {
        ImplItem::Const(x) => &x.attrs,
        ImplItem::Fn(x) => &x.attrs,
        ImplItem::Type(x) => &x.attrs,
        ImplItem::Macro(x) => &x.attrs,
        _ => &[],
    })
}

pub fn trait_item_gated(i: &TraitItem) -> bool {
    is_cfg_gated(match i {
        TraitItem::Const(x) => &x.attrs,
        TraitItem::Fn(x) => &x.attrs,
        TraitItem::Type(x) => &x.attrs,
        TraitItem::Macro(x) => &x.attrs,
        _ => &[],
    })
}

pub fn foreign_item_gated(i: &ForeignItem) -> bool {
    is_cfg_gated(match i {
        ForeignItem::Fn(x) => &x.attrs,
        ForeignItem::Static(x) => &x.attrs,
        ForeignItem::Type(x) => &x.attrs,
        ForeignItem::Macro(x) => &x.attrs,
        _ => &[],
    })
}

pub fn stmt_gated(s: &Stmt) -> bool {
    is_cfg_gated(match s {
        Stmt::Local(l) => &l.attrs,
        Stmt::Item(i) => item_attrs(i),
        Stmt::Expr(e, _) => expr_attrs(e),
        Stmt::Macro(m) => &m.attrs,
    })
}

pub fn item_attrs(i: &Item) -> &[Attribute] {
    match i {
        Item::Const(x) => &x.attrs,
        Item::Enum(x) => &x.attrs,
        Item::ExternCrate(x) => &x.attrs,
        Item::Fn(x) => &x.attrs,
        Item::ForeignMod(x) => &x.attrs,
        Item::Impl(x) => &x.attrs,
        Item::Macro(x) => &x.attrs,
        Item::Mod(x) => &x.attrs,
        Item::Static(x) => &x.attrs,
        Item::Struct(x) => &x.attrs,
        Item::Trait(x) => &x.attrs,
        Item::TraitAlias(x) => &x.attrs,
        Item::Type(x) => &x.attrs,
        Item::Union(x) => &x.attrs,
        Item::Use(x) => &x.attrs,
        _ => &[],
    }
}

pub fn expr_attrs(e: &Expr) -> &[Attribute] {
    match e {
        Expr::Array(x) => &x.attrs,
        Expr::Assign(x) => &x.attrs,
        Expr::Async(x) => &x.attrs,
        Expr::Await(x) => &x.attrs,
        Expr::Binary(x) => &x.attrs,
        Expr::Block(x) => &x.attrs,
        Expr::Break(x) => &x.attrs,
        Expr::Call(x) => &x.attrs,
        Expr::Cast(x) => &x.attrs,
        Expr::Closure(x) => &x.attrs,
        Expr::Const(x) => &x.attrs,
        Expr::Continue(x) => &x.attrs,
        Expr::Field(x) => &x.attrs,
        Expr::ForLoop(x) => &x.attrs,
        Expr::Group(x) => &x.attrs,
        Expr::If(x) => &x.attrs,
        Expr::Index(x) => &x.attrs,
        Expr::Infer(x) => &x.attrs,
        Expr::Let(x) => &x.attrs,
        Expr::Lit(x) => &x.attrs,
        Expr::Loop(x) => &x.attrs,
        Expr::Macro(x) => &x.attrs,
        Expr::Match(x) => &x.attrs,
        Expr::MethodCall(x) => &x.attrs,
        Expr::Paren(x) => &x.attrs,
        Expr::Path(x) => &x.attrs,
        Expr::Range(x) => &x.attrs,
        Expr::RawAddr(x) => &x.attrs,
        Expr::Reference(x) => &x.attrs,
        Expr::Repeat(x) => &x.attrs,
        Expr::Return(x) => &x.attrs,
        Expr::Struct(x) => &x.attrs,
        Expr::Try(x) => &x.attrs,
        Expr::TryBlock(x) => &x.attrs,
        Expr::Tuple(x) => &x.attrs,
        Expr::Unary(x) => &x.attrs,
        Expr::Unsafe(x) => &x.attrs,
        Expr::While(x) => &x.attrs,
        Expr::Yield(x) => &x.attrs,
        _ => &[],
    }
}

/// The `unsafe` keywords in a token stream, at any depth, leaving out the
/// attribute form `#[unsafe(...)]` / `#![unsafe(...)]`.
pub fn unsafe_keywords(tokens: &TokenStream) -> Vec<Span> {
    let mut out = Vec::new();
    collect_unsafe(tokens, &mut out);
    out
}

fn collect_unsafe(tokens: &TokenStream, out: &mut Vec<Span>) {
    let tts: Vec<TokenTree> = tokens.clone().into_iter().collect();
    for (i, tt) in tts.iter().enumerate() {
        match tt {
            TokenTree::Ident(id) if id == "unsafe" => out.push(id.span()),
            TokenTree::Group(g) => {
                let after_hash = match i.checked_sub(1).map(|j| &tts[j]) {
                    Some(TokenTree::Punct(p)) if p.as_char() == '#' => true,
                    Some(TokenTree::Punct(p)) if p.as_char() == '!' => {
                        matches!(i.checked_sub(2).map(|j| &tts[j]), Some(TokenTree::Punct(h)) if h.as_char() == '#')
                    }
                    _ => false,
                };
                if after_hash && g.delimiter() == Delimiter::Bracket && is_unsafe_attr(&g.stream())
                {
                    continue;
                }
                collect_unsafe(&g.stream(), out);
            }
            _ => {}
        }
    }
}

/// Whether bracketed attribute tokens read `unsafe(...)`.
fn is_unsafe_attr(tokens: &TokenStream) -> bool {
    let mut it = tokens.clone().into_iter();
    matches!(
        (it.next(), it.next(), it.next()),
        (Some(TokenTree::Ident(id)), Some(TokenTree::Group(g)), None)
            if id == "unsafe" && g.delimiter() == Delimiter::Parenthesis
    )
}

/// The comma-separated expressions a macro invocation's body holds, when it
/// holds any: `assert!(q.wait_event(..).is_ok())` passes through here.
pub fn macro_exprs(mac: &Macro) -> Vec<Expr> {
    mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        .map(|p| p.into_iter().collect())
        .unwrap_or_default()
}

/// A macro body re-read as Rust: statements, or else a comma-separated
/// expression list.
pub enum Expansion {
    Stmts(Vec<Stmt>),
    Exprs(Vec<Expr>),
}

/// What `mac`'s tokens parse as with each metavariable read as a plain
/// identifier. A `macro_rules!` yields one expansion per transcriber.
pub fn expansions(mac: &Macro) -> Vec<Expansion> {
    let bodies = if mac.path.is_ident("macro_rules") {
        transcribers(mac.tokens.clone())
    } else {
        vec![mac.tokens.clone()]
    };
    bodies
        .into_iter()
        .filter_map(|body| {
            let body = unmeta(body);
            if let Ok(stmts) = Block::parse_within.parse2(body.clone()) {
                return Some(Expansion::Stmts(stmts));
            }
            Punctuated::<Expr, Token![,]>::parse_terminated
                .parse2(body)
                .ok()
                .map(|exprs| Expansion::Exprs(exprs.into_iter().collect()))
        })
        .collect()
}

/// Visit `mac`'s body with `v`, as far as it parses (see [`expansions`]).
pub fn visit_expansion<V: for<'a> Visit<'a>>(v: &mut V, mac: &Macro) {
    for expansion in expansions(mac) {
        match expansion {
            Expansion::Stmts(stmts) => stmts.iter().for_each(|s| v.visit_stmt(s)),
            Expansion::Exprs(exprs) => exprs.iter().for_each(|e| v.visit_expr(e)),
        }
    }
}

/// The right-hand side of each `matcher => transcriber` rule.
fn transcribers(tokens: TokenStream) -> Vec<TokenStream> {
    let tts: Vec<TokenTree> = tokens.into_iter().collect();
    let punct =
        |i: usize, c: char| matches!(tts.get(i), Some(TokenTree::Punct(p)) if p.as_char() == c);
    (2..tts.len())
        .filter_map(|i| match &tts[i] {
            TokenTree::Group(g) if punct(i - 2, '=') && punct(i - 1, '>') => Some(g.stream()),
            _ => None,
        })
        .collect()
}

/// `$name` and `$crate` become plain identifiers; `$(..) sep? op` becomes one
/// copy of its contents.
fn unmeta(tokens: TokenStream) -> TokenStream {
    let is_op = |tt: Option<&TokenTree>| matches!(tt, Some(TokenTree::Punct(p)) if matches!(p.as_char(), '*' | '+' | '?'));
    let mut out = Vec::new();
    let mut it = tokens.into_iter().peekable();
    while let Some(tt) = it.next() {
        match tt {
            TokenTree::Punct(p) if p.as_char() == '$' => match it.peek() {
                Some(TokenTree::Ident(_)) => out.extend(it.next()),
                Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis => {
                    let inner = g.stream();
                    it.next();
                    out.extend(unmeta(inner));
                    if is_op(it.peek()) {
                        it.next();
                    } else {
                        let mut ahead = it.clone();
                        ahead.next();
                        if is_op(ahead.peek()) {
                            it.next();
                            it.next();
                        }
                    }
                }
                _ => out.push(TokenTree::Punct(p)),
            },
            TokenTree::Group(g) => {
                let mut group = Group::new(g.delimiter(), unmeta(g.stream()));
                group.set_span(g.span());
                out.push(TokenTree::Group(group));
            }
            tt => out.push(tt),
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(src: &str) -> Vec<Attribute> {
        let item: syn::ItemFn = syn::parse_str(&format!("{src} fn f() {{}}")).unwrap();
        item.attrs
    }

    #[test]
    fn cfg_test_is_found_alone_and_inside_all() {
        assert!(is_cfg_test(&attrs("#[cfg(test)]")));
        assert!(is_cfg_test(&attrs("#[cfg(all(test, feature = \"x\"))]")));
        assert!(!is_cfg_test(&attrs("#[cfg(feature = \"test-hooks\")]")));
        assert!(is_cfg_gated(&attrs("#[cfg(feature = \"x\")]")));
        assert!(!is_cfg_gated(&attrs("#[inline]")));
    }

    #[test]
    fn a_parse_error_names_the_file_and_line() {
        let err = Source::parse("mm/src/x.rs", "fn f(\n{".to_string())
            .err()
            .unwrap()
            .to_string();
        assert!(err.starts_with("mm/src/x.rs:"), "{err}");
    }

    #[test]
    fn spans_carry_lines() {
        let src = Source::parse("a.rs", "\n\nfn g() {}\n".to_string()).unwrap();
        let syn::Item::Fn(f) = &src.file.items[0] else {
            panic!()
        };
        assert_eq!(line(f.sig.ident.span()), 3);
    }

    #[test]
    fn path_tails_match_whole_segments() {
        let p: Path = syn::parse_str("core::mem::forget").unwrap();
        assert!(path_ends_with(&p, &["mem", "forget"]));
        assert!(path_ends_with(&p, &["forget"]));
        assert!(!path_ends_with(&p, &["ptr", "forget"]));
        assert_eq!(last_ident(&p).as_deref(), Some("forget"));
    }
}
