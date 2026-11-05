//! Apps draw with the theme's roles and tokens, never a colour, a spacing or
//! an inset of their own, so a theme change reaches every app and no app
//! drifts from the rest.
//!
//! Apps are every file under `apps/`, the Settings panels in `settings-app/`
//! outside its tests, and each module of `userland/src/apps/` that implements
//! appkit's `App` or calls `run_app`. An app builds no colour (`Color32::rgb`,
//! `Color32::new`, `Color32::WHITE`, `Color32::BLACK`, `Color32(bits)`,
//! `EncodedPixel(bits)`, a rename or alias of `Color32`), derives none
//! (`with_alpha`, `mix`, `contrast::`), and gives no `spacing:`, `size:
//! Length::Px(..)` or `EdgeInsets` a non-zero number. appkit, outside its
//! tests, is held to the colour constructors alone.
//!
//! A file whose colours are its content takes a `<check> <path> <why>` line in
//! `scripts/gates/ui-literals.txt`, and a line that exempts nothing fails.

use std::collections::BTreeSet;

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Expr, Member, Path, Token};

use crate::Result;
use crate::repo::{self, Repo};
use crate::report::Report;
use crate::rust::{self, Source};

const ALLOWLIST: &str = "scripts/gates/ui-literals.txt";
const USERLAND_APPS: &str = "userland/src/apps";

pub fn run(repo: &Repo) -> Result<Report> {
    let files = repo.rs_files()?;
    let allowlist = repo.read(ALLOWLIST)?;
    check(repo, &files, ALLOWLIST, &allowlist)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    Colour,
    Spacing,
    Inset,
}

impl Check {
    fn name(self) -> &'static str {
        match self {
            Check::Colour => "colour",
            Check::Spacing => "spacing",
            Check::Inset => "inset",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    App,
    Kit,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Hit {
    path: String,
    line: usize,
    check: Check,
    what: String,
}

/// The module of `userland/src/apps/` a path belongs to: `foo` for both
/// `foo.rs` and `foo/**`.
fn userland_module(path: &str) -> Option<&str> {
    let rest = path.strip_prefix(USERLAND_APPS)?.strip_prefix('/')?;
    let first = rest.split('/').next()?;
    Some(first.strip_suffix(".rs").unwrap_or(first))
}

fn scope(path: &str, app_modules: &BTreeSet<String>) -> Option<Scope> {
    if repo::under(path, "apps") {
        Some(Scope::App)
    } else if repo::under(path, "settings-app/src") {
        (path != "settings-app/src/tests.rs").then_some(Scope::App)
    } else if repo::under(path, "appkit/src") {
        (path != "appkit/src/tests.rs" && !repo::under(path, "appkit/src/tests"))
            .then_some(Scope::Kit)
    } else {
        userland_module(path)
            .filter(|m| app_modules.contains(*m))
            .map(|_| Scope::App)
    }
}

/// A macro's body as the expressions or statements it most likely holds, so
/// a colour built inside `vec![..]` or `format!(..)` is still seen.
fn visit_macro_body<V: for<'a> Visit<'a>>(v: &mut V, mac: &syn::Macro) {
    if mac.path.is_ident("macro_rules") {
        return;
    }
    if let Ok(exprs) = Punctuated::<Expr, Token![,]>::parse_terminated.parse2(mac.tokens.clone()) {
        for e in &exprs {
            v.visit_expr(e);
        }
    } else if let Ok(stmts) = syn::Block::parse_within.parse2(mac.tokens.clone()) {
        for s in &stmts {
            v.visit_stmt(s);
        }
    }
}

/// Whether a module implements appkit's `App` or calls `run_app`.
#[derive(Default)]
struct AppMark(bool);

impl<'ast> Visit<'ast> for AppMark {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if let Some((_, path, _)) = &i.trait_
            && rust::last_ident(path).as_deref() == Some("App")
        {
            self.0 = true;
        }
        visit::visit_item_impl(self, i);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if let Expr::Path(p) = &*e.func
            && rust::last_ident(&p.path).as_deref() == Some("run_app")
        {
            self.0 = true;
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if e.method == "run_app" {
            self.0 = true;
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit_macro_body(self, mac);
    }
}

/// Whether a node holds an integer or float literal that is not zero.
#[derive(Default)]
struct NonZero(bool);

impl<'ast> Visit<'ast> for NonZero {
    fn visit_lit_int(&mut self, i: &'ast syn::LitInt) {
        if i.base10_parse::<u128>().map_or(true, |n| n != 0) {
            self.0 = true;
        }
    }

    fn visit_lit_float(&mut self, f: &'ast syn::LitFloat) {
        if f.base10_parse::<f64>().map_or(true, |n| n != 0.0) {
            self.0 = true;
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit_macro_body(self, mac);
    }
}

fn nonzero_expr(e: &Expr) -> bool {
    let mut v = NonZero::default();
    v.visit_expr(e);
    v.0
}

fn segments(path: &Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

fn ends_in(path: &Path, names: &[&str]) -> bool {
    rust::last_ident(path).is_some_and(|l| names.contains(&l.as_str()))
}

/// `[.., Length, Px]`-style path tails: the last two segments.
fn tail_is(path: &Path, owner: &str, names: &[&str]) -> bool {
    let segs = segments(path);
    segs.len() >= 2
        && segs[segs.len() - 2] == owner
        && names.contains(&segs[segs.len() - 1].as_str())
}

const COLOUR_CONSTRUCTORS: &[&str] = &["rgb", "new", "WHITE", "BLACK"];
const COLOUR_TYPES: &[&str] = &["Color32", "EncodedPixel"];
const COLOUR_DERIVATIONS: &[&str] = &["with_alpha", "mix"];
const INSET_CONSTRUCTORS: &[&str] = &["all", "symmetric", "new"];

struct Scan {
    scope: Scope,
    hits: Vec<(usize, Check, String)>,
}

impl Scan {
    fn hit(&mut self, span: Span, check: Check, what: impl Into<String>) {
        self.hits.push((rust::line(span), check, what.into()));
    }

    fn app(&self) -> bool {
        self.scope == Scope::App
    }

    /// Paths the parser leaves as tokens (unparsed macro bodies, attribute
    /// arguments) still name `Color32::rgb`.
    fn scan_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, t) in tokens.iter().enumerate() {
            match t {
                TokenTree::Group(g) => self.scan_tokens(g.stream()),
                TokenTree::Ident(id) if id == "Color32" => {
                    let colon = |j: usize| matches!(tokens.get(j), Some(TokenTree::Punct(p)) if p.as_char() == ':');
                    if colon(i + 1)
                        && colon(i + 2)
                        && let Some(TokenTree::Ident(c)) = tokens.get(i + 3)
                        && COLOUR_CONSTRUCTORS.contains(&c.to_string().as_str())
                    {
                        self.hit(
                            id.span(),
                            Check::Colour,
                            format!("builds a colour with `Color32::{c}`"),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_path(&mut self, p: &'ast Path) {
        let segs: Vec<_> = p.segments.iter().collect();
        for w in segs.windows(2) {
            if w[0].ident == "Color32"
                && COLOUR_CONSTRUCTORS.contains(&w[1].ident.to_string().as_str())
            {
                self.hit(
                    w[0].ident.span(),
                    Check::Colour,
                    format!("builds a colour with `Color32::{}`", w[1].ident),
                );
            }
            if self.app() && w[0].ident == "contrast" {
                self.hit(
                    w[0].ident.span(),
                    Check::Colour,
                    "derives a colour through `contrast::`",
                );
            }
        }
        visit::visit_path(self, p);
    }

    fn visit_use_path(&mut self, u: &'ast syn::UsePath) {
        if self.app() && u.ident == "contrast" {
            self.hit(
                u.ident.span(),
                Check::Colour,
                "derives a colour through `contrast::`",
            );
        }
        visit::visit_use_path(self, u);
    }

    fn visit_use_rename(&mut self, u: &'ast syn::UseRename) {
        if u.ident == "Color32" {
            self.hit(
                u.ident.span(),
                Check::Colour,
                format!("renames `Color32` to `{}`", u.rename),
            );
        }
        visit::visit_use_rename(self, u);
    }

    fn visit_item_type(&mut self, t: &'ast syn::ItemType) {
        self.alias(&t.ty);
        visit::visit_item_type(self, t);
    }

    fn visit_impl_item_type(&mut self, t: &'ast syn::ImplItemType) {
        self.alias(&t.ty);
        visit::visit_impl_item_type(self, t);
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        if e.qself.is_none() && ends_in(&e.path, COLOUR_TYPES) {
            let last = e.path.segments.last().expect("a path has a segment");
            self.hit(
                last.ident.span(),
                Check::Colour,
                format!("builds a `{}` from its bits", last.ident),
            );
        }
        visit::visit_expr_path(self, e);
    }

    fn visit_pat_tuple_struct(&mut self, p: &'ast syn::PatTupleStruct) {
        if ends_in(&p.path, COLOUR_TYPES) {
            let last = p.path.segments.last().expect("a path has a segment");
            self.hit(
                last.ident.span(),
                Check::Colour,
                format!("takes a `{}` by its bits", last.ident),
            );
        }
        visit::visit_pat_tuple_struct(self, p);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if self.app()
            && let Expr::Path(f) = &*e.func
        {
            let name = rust::last_ident(&f.path).unwrap_or_default();
            if COLOUR_DERIVATIONS.contains(&name.as_str()) {
                self.hit(
                    f.path
                        .segments
                        .last()
                        .expect("a path has a segment")
                        .ident
                        .span(),
                    Check::Colour,
                    format!("derives a colour with `{name}`"),
                );
            }
            if tail_is(&f.path, "EdgeInsets", INSET_CONSTRUCTORS) && e.args.iter().any(nonzero_expr)
            {
                let owner = &f.path.segments[f.path.segments.len() - 2];
                self.hit(
                    owner.ident.span(),
                    Check::Inset,
                    format!("gives `EdgeInsets::{name}` a number"),
                );
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if self.app() && COLOUR_DERIVATIONS.contains(&e.method.to_string().as_str()) {
            self.hit(
                e.method.span(),
                Check::Colour,
                format!("derives a colour with `.{}`", e.method),
            );
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        if self.app() {
            if e.qself.is_none() && ends_in(&e.path, &["EdgeInsets"]) {
                let last = e.path.segments.last().expect("a path has a segment");
                self.hit(
                    last.ident.span(),
                    Check::Inset,
                    "spells out an `EdgeInsets { .. }`",
                );
            }
            for f in &e.fields {
                let Member::Named(member) = &f.member else {
                    continue;
                };
                if member == "spacing" && nonzero_expr(&f.expr) {
                    self.hit(member.span(), Check::Spacing, "gives `spacing:` a number");
                }
                if member == "size"
                    && let Expr::Call(call) = &f.expr
                    && let Expr::Path(func) = &*call.func
                    && tail_is(&func.path, "Length", &["Px"])
                    && call.args.iter().any(nonzero_expr)
                {
                    self.hit(
                        member.span(),
                        Check::Spacing,
                        "gives `size: Length::Px(..)` a number",
                    );
                }
            }
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit::visit_macro(self, mac);
        let before = self.hits.len();
        visit_macro_body(self, mac);
        if self.hits.len() == before {
            self.scan_tokens(mac.tokens.clone());
        }
    }
}

impl Scan {
    fn alias(&mut self, ty: &syn::Type) {
        if let syn::Type::Path(t) = ty
            && t.qself.is_none()
            && rust::last_ident(&t.path).as_deref() == Some("Color32")
            && t.path.segments.iter().all(|s| s.arguments.is_none())
        {
            let last = t.path.segments.last().expect("a path has a segment");
            self.hit(last.ident.span(), Check::Colour, "aliases `Color32`");
        }
    }
}

fn scan_file(source: &Source, scope: Scope) -> Vec<Hit> {
    let mut scan = Scan {
        scope,
        hits: Vec::new(),
    };
    scan.visit_file(&source.file);
    let mut seen = BTreeSet::new();
    let mut hits: Vec<Hit> = scan
        .hits
        .into_iter()
        .filter(|(line, check, _)| seen.insert((*line, *check)))
        .map(|(line, check, what)| Hit {
            path: source.path.clone(),
            line,
            check,
            what,
        })
        .collect();
    hits.sort();
    hits
}

struct Entry<'a> {
    line: usize,
    check: &'a str,
    path: &'a str,
    text: &'a str,
}

fn entries(text: &str) -> Vec<Entry<'_>> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let t = l.trim_start();
            !t.is_empty() && !t.starts_with('#')
        })
        .map(|(i, l)| {
            let mut f = l.split_whitespace();
            Entry {
                line: i + 1,
                check: f.next().unwrap_or_default(),
                path: f.next().unwrap_or_default(),
                text: l.trim(),
            }
        })
        .collect()
}

struct Outcome {
    hits: Vec<Hit>,
    dead: Vec<(usize, String)>,
    apps: usize,
    kit: usize,
    modules: usize,
}

fn scan(repo: &Repo, files: &[String], allowlist: &str) -> Result<Outcome> {
    let mut app_modules = BTreeSet::new();
    for path in files.iter().filter(|p| userland_module(p).is_some()) {
        if !repo.path(path).is_file() {
            continue;
        }
        let mut mark = AppMark::default();
        mark.visit_file(&Source::load(repo, path)?.file);
        if mark.0 {
            app_modules.insert(userland_module(path).unwrap_or_default().to_string());
        }
    }
    if app_modules.is_empty() {
        return Err(format!(
            "no appkit app found under {USERLAND_APPS}/ — the scope rule found nothing to scan"
        )
        .into());
    }

    let entries = entries(allowlist);
    let mut used = vec![false; entries.len()];
    let mut hits = Vec::new();
    let (mut apps, mut kit) = (0, 0);
    for path in files {
        let Some(scope) = scope(path, &app_modules) else {
            continue;
        };
        if !repo.path(path).is_file() {
            continue;
        }
        match scope {
            Scope::App => apps += 1,
            Scope::Kit => kit += 1,
        }
        for hit in scan_file(&Source::load(repo, path)?, scope) {
            match entries
                .iter()
                .position(|e| e.check == hit.check.name() && e.path == hit.path)
            {
                Some(i) => used[i] = true,
                None => hits.push(hit),
            }
        }
    }
    let dead = entries
        .iter()
        .zip(&used)
        .filter(|(_, u)| !**u)
        .map(|(e, _)| (e.line, e.text.to_string()))
        .collect();
    Ok(Outcome {
        hits,
        dead,
        apps,
        kit,
        modules: app_modules.len(),
    })
}

fn check(repo: &Repo, files: &[String], allowlist_path: &str, allowlist: &str) -> Result<Report> {
    let out = scan(repo, files, allowlist)?;
    let mut report = Report::new();
    for hit in &out.hits {
        let instead = match hit.check {
            Check::Colour => format!(
                "name a `Tone` or a `Surface` instead; a file whose colours are its content \
                 takes `colour {} <why>` in {allowlist_path}",
                hit.path
            ),
            Check::Spacing => "name a `theme::space` token instead".to_string(),
            Check::Inset => "name `theme::space` or `theme::inset` tokens instead".to_string(),
        };
        let who = if scope_is_kit(&hit.path) {
            "appkit, outside its tests,"
        } else {
            "an app"
        };
        report.at(
            &hit.path,
            hit.line,
            format!("{who} {} — {instead}", hit.what),
        );
    }
    for (line, text) in &out.dead {
        report.at(
            allowlist_path,
            *line,
            format!("`{text}` exempts nothing — delete the entry"),
        );
    }
    report.summary(format!(
        "scanned {} app files ({} app modules under {USERLAND_APPS}/, apps/, settings-app/) and \
         {} appkit files",
        out.apps, out.modules, out.kit
    ));
    Ok(report)
}

fn scope_is_kit(path: &str) -> bool {
    repo::under(path, "appkit/src")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const PAINTED: &str = r##"const A: Color32 = Color32::rgb(1, 2, 3);
const B: Color32 = slopos_abi::draw::Color32::new(1, 2, 3, 4);
const C: Color32 = Color32::WHITE;
fn d() -> Color32 { Color32(0xff00_0000) }
fn e(c: &mut Color32) {
    *c = Color32::rgb(1, 2, 3);
}
use slopos_abi::draw::Color32 as Rgb;
const F: EncodedPixel = EncodedPixel(0xff12_3456);
const G: Color32 = with_alpha(Color32::TRANSPARENT, 0xff);
fn m() { Node::VStack { spacing: 8, children, align }; }
fn n() { Node::VStack { spacing: space::SM + 2, children, align }; }
fn o() { Node::VStack { spacing: 0x8, children, align }; }
fn p() { Node::Spacer { size: Length::Px(8) }; }
fn q() { let p = EdgeInsets::all(16); }
fn r() { let p = EdgeInsets::new(4, space::SM, 0, 0); }
fn s() { let p = EdgeInsets::all(0x10); }
fn t() {
    let p = EdgeInsets::symmetric(
        24,
        space::MD,
    );
}
fn u() {
    let p = EdgeInsets::new(
        pad(0),
        12,
        0,
        0,
    );
}
fn v() { let p = EdgeInsets { top: 1, right: 0, bottom: 0, left: 0 }; }
fn w() { let g = "/usr/share/*"; }
const X: Color32 = Color32::rgb(1, 2, 3);
fn y(h: &str) { let u = format!("http://{h}"); let c = Color32::rgb(1, 2, 3); }
fn z() { let q = '"'; let c = Color32::WHITE; }
fn aa<'a>(s: &'a str) -> Color32 { Color32::BLACK }
fn ab() { let p = (EdgeInsets::all(space::SM), EdgeInsets::all(4)); }
fn ac(a: Color32, b: Color32) -> f64 { contrast::ratio(a, b) }
const AD: &str = r#"a"b"#;
const AE: Color32 = Color32::rgb(1, 2, 3);
fn af() { let a = Node::VStack { spacing: 0, children }; let b = Node::HStack { spacing: 6, children }; }
type Ag = Color32;
fn ai(compact: bool) {
    Node::VStack {
        spacing: if compact {
            4
        } else {
            space::SM
        },
        children,
    };
}
"##;

    const THEMED: &str = r#"// const A: Color32 = Color32::rgb(1, 2, 3);
/* spacing: 8 */ fn after_a_block_comment() {}
/*
 * Color32::rgb(1, 2, 3)
 * EdgeInsets::all(16)
 */
/// Doc: `Color32::WHITE`, spacing: 8.
fn a(theme: &Theme) -> Color32 { let c: Color32 = theme.text; c }
fn b(c: Color32) -> Color32 { theme::current().accent }
const C: Color32 = Color32::TRANSPARENT;
fn d() { Node::VStack { spacing: 0, children, align }; }
fn e() { Node::HStack { spacing: space::SM, children, align }; }
fn f() { let p = EdgeInsets::new(space::SM, 0, 0, space::SM); }
fn g() { let p = EdgeInsets::default(); let q = inset::PAGE; }
fn h() { Node::Spacer { size: Length::Px(space::SM) }; }
fn i() { Node::SizedBox { width: Some(Length::Px(140)), height: None, child }; }
fn j() { TableColumnWidth::Fixed(80); let darkness = 2; }
fn k() {
    let p = EdgeInsets::symmetric(
        space::XL,
        0,
    );
    let rows = vec![
        1,
        2,
    ];
}
fn o() -> &'static str { "Color32::rgb(1, 2, 3) spacing: 8 EdgeInsets::all(16)" }
const P: &str = "a string that runs on: \
    spacing: 8, Color32::WHITE";
fn q() -> char { '\'' }
fn r(compact: bool) -> i32 {
    Node::VStack {
        spacing: if compact {
            space::XS
        } else {
            space::SM
        },
        children,
    };
    let spacing: i32 = space::SM;
    spacing
}
type Px = i32;
"#;

    const ALLOWLIST: &str = "\
# A comment, and a blank line, are no entries.

colour apps/src/palette.rs a syntax palette
colour apps/src/gone.rs a file that no longer exists
";

    fn fixture() -> Tree {
        Tree::new(&[
            ("apps/src/painted.rs", PAINTED),
            ("apps/src/themed.rs", THEMED),
            ("userland/src/apps/demo/mod.rs", "impl App for Demo {}\n"),
            (
                "userland/src/apps/demo/view.rs",
                "const A: Color32 = Color32::rgb(1, 2, 3);\n",
            ),
            (
                "userland/src/apps/generic/mod.rs",
                "impl<T: Clone> slopos_appkit::App for Generic<T> {}\nconst A: Color32 = Color32::rgb(1, 2, 3);\n",
            ),
            (
                "userland/src/apps/gallery.rs",
                "pub fn main() -> ! { slopos_appkit::run_app(Gallery) }\nfn a() { Node::VStack { spacing: 4, children, align }; }\n",
            ),
            (
                "userland/src/apps/turbo/mod.rs",
                "pub fn main() -> ! { slopos_appkit::run_app::<Turbo>(Turbo) }\n",
            ),
            (
                "userland/src/apps/turbo/view.rs",
                "const A: Color32 = Color32::rgb(1, 2, 3);\n",
            ),
            (
                "userland/src/apps/compositor/frame.rs",
                "const A: Color32 = Color32::rgb(1, 2, 3);\nfn b() { Node::VStack { spacing: 8, children, align }; }\n",
            ),
            (
                "appkit/src/widgets/painter.rs",
                "const A: Color32 = Color32::new(0, 0, 0, 40);\nfn b(theme: &Theme) { let p = EdgeInsets::all(16); with_alpha(theme.text, 0x40); }\n",
            ),
            (
                "appkit/src/tests/fixture.rs",
                "const A: Color32 = Color32::rgb(1, 2, 3);\n",
            ),
            (
                "apps/src/palette.rs",
                "const KEYWORD: Color32 = Color32::rgb(1, 2, 3);\nfn a() { Node::VStack { spacing: 8, children, align }; }\n",
            ),
        ])
    }

    fn outcome(tree: &Tree) -> Outcome {
        let files = tree.repo.rs_files().unwrap();
        scan(&tree.repo, &files, ALLOWLIST).unwrap()
    }

    fn count(out: &Outcome, check: Check) -> usize {
        out.hits.iter().filter(|h| h.check == check).count()
    }

    fn at(out: &Outcome, path: &str) -> Vec<(usize, Check)> {
        out.hits
            .iter()
            .filter(|h| h.path == path)
            .map(|h| (h.line, h.check))
            .collect()
    }

    #[test]
    fn colours_spacings_and_insets_apps_pick_fail() {
        let tree = fixture();
        let out = outcome(&tree);
        assert_eq!(count(&out, Check::Colour), 19, "{:#?}", out.hits);
        assert_eq!(count(&out, Check::Spacing), 8, "{:#?}", out.hits);
        assert_eq!(count(&out, Check::Inset), 7, "{:#?}", out.hits);
        assert_eq!(out.dead.len(), 1);
        assert_eq!(out.dead[0].0, 4);
        let colour: Vec<usize> = at(&out, "apps/src/painted.rs")
            .into_iter()
            .filter(|(_, c)| *c == Check::Colour)
            .map(|(l, _)| l)
            .collect();
        assert_eq!(
            colour,
            [1, 2, 3, 4, 6, 8, 9, 10, 34, 35, 36, 37, 39, 41, 43]
        );
        let spacing: Vec<usize> = at(&out, "apps/src/painted.rs")
            .into_iter()
            .filter(|(_, c)| *c == Check::Spacing)
            .map(|(l, _)| l)
            .collect();
        assert_eq!(spacing, [11, 12, 13, 14, 42, 46]);
        let inset: Vec<usize> = at(&out, "apps/src/painted.rs")
            .into_iter()
            .filter(|(_, c)| *c == Check::Inset)
            .map(|(l, _)| l)
            .collect();
        assert_eq!(inset, [15, 16, 17, 19, 25, 32, 38]);
    }

    #[test]
    fn accepted_forms_stay_silent() {
        let tree = fixture();
        let out = outcome(&tree);
        for path in [
            "apps/src/themed.rs",
            "userland/src/apps/compositor/frame.rs",
            "appkit/src/tests/fixture.rs",
        ] {
            assert!(at(&out, path).is_empty(), "{path}: {:?}", at(&out, path));
        }
        assert_eq!(
            at(&out, "appkit/src/widgets/painter.rs"),
            [(1, Check::Colour)]
        );
        assert_eq!(at(&out, "apps/src/palette.rs"), [(2, Check::Spacing)]);
    }

    #[test]
    fn findings_and_dead_entries_render() {
        let tree = fixture();
        let files = tree.repo.rs_files().unwrap();
        let report = check(&tree.repo, &files, "allow.txt", ALLOWLIST).unwrap();
        assert_eq!(report.findings.len(), 19 + 8 + 7 + 1);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.location == "allow.txt:4"
                    && f.message.contains("colour apps/src/gone.rs"))
        );
        assert!(report.findings.iter().any(|f| {
            f.location == "appkit/src/widgets/painter.rs:1"
                && f.message
                    .starts_with("appkit, outside its tests, builds a colour with `Color32::new`")
        }));
    }

    #[test]
    fn no_userland_app_is_an_error() {
        let tree = Tree::new(&[
            ("apps/src/a.rs", "fn a() {}\n"),
            (
                "userland/src/apps/tool.rs",
                "// impl App for Tool\nfn main() {}\n",
            ),
        ]);
        let files = tree.repo.rs_files().unwrap();
        assert!(scan(&tree.repo, &files, "").is_err());
    }

    #[test]
    fn macros_attributes_and_split_lines() {
        let tree = fixture();
        tree.write(
            "apps/src/more.rs",
            r#"fn a() { let v = vec![Color32::rgb(1, 2, 3), theme.text]; }
fn b() { assert_eq!(c, Color32::WHITE, "{}", "Color32::BLACK"); }
fn c() { odd!(=> Color32::new(1, 2, 3, 4) <=); }
fn d() {
    let x = slopos_abi::draw::Color32
        ::rgb(1, 2, 3);
}
fn e() { let c = theme.accent.with_alpha(0x40); }
fn f() { let c = '}'; Node::VStack { spacing: 3, children }; }
fn g() { let p = slopos_appkit::EdgeInsets::all(/* 16 */ space::SM); }
fn h() { let colours = [0, 1].map(Color32); }
use slopos_appkit::theme::contrast::ratio;
fn i() { let s = Node::VStack { spacing: { let k = 2; space::SM * k }, children }; }
"#,
        );
        let out = outcome(&tree);
        assert_eq!(
            at(&out, "apps/src/more.rs"),
            [
                (1, Check::Colour),
                (2, Check::Colour),
                (3, Check::Colour),
                (5, Check::Colour),
                (8, Check::Colour),
                (9, Check::Spacing),
                (11, Check::Colour),
                (12, Check::Colour),
                (13, Check::Spacing),
            ]
        );
    }

    #[test]
    fn userland_modules_are_apps_by_their_syntax() {
        let tree = Tree::new(&[
            (
                "userland/src/apps/a.rs",
                "fn f() { let s = \"impl App for A\"; run_app; }\n",
            ),
            (
                "userland/src/apps/b/mod.rs",
                "mod view;\nfn main() { launcher.run_app(B) }\n",
            ),
            (
                "userland/src/apps/b/view.rs",
                "fn v() { Node::VStack { spacing: 2, children }; }\n",
            ),
            (
                "userland/src/apps/c.rs",
                "impl\n    slopos_appkit::App\n    for C {}\nfn v() { Node::VStack { spacing: 2, children }; }\n",
            ),
            (
                "userland/src/apps/d.rs",
                "impl Application for D {}\nfn v() { Node::VStack { spacing: 2, children }; }\n",
            ),
        ]);
        let files = tree.repo.rs_files().unwrap();
        let out = scan(&tree.repo, &files, "").unwrap();
        let paths: Vec<&str> = out.hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            ["userland/src/apps/b/view.rs", "userland/src/apps/c.rs"]
        );
    }
}
