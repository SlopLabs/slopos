//! No kernel crate other than slopos-ostd names `alloc` directly.
//!
//! Kernel crates reach the heap through `slopos_ostd::mm::heap`, the one
//! kernel module allowed to depend on `alloc`, so every allocation goes
//! through the surface that accounts for it. Two halves:
//! - no crate manifest outside the carve-outs declares an `alloc`
//!   dependency in a `[dependencies]`-family table;
//! - no source file outside the carve-outs writes `extern crate alloc` or a
//!   `use alloc::` / `use ::alloc::` import, save `kernel/src/main.rs`, which
//!   needs `extern crate alloc` for its `#[global_allocator]` and
//!   `#[alloc_error_handler]`. An item under `#[cfg(...)]` compiles out of
//!   the kernel build and is skipped whole.
//!
//! The carve-outs are slopos-ostd, the userland crates (the host-testable
//! `*-core` ones among them), which the kernel does not link, and the named
//! TCB annexes vendor/unwinding and vendor/gimli, pinned third-party code
//! reached only through OSTD. Other vendored crates are scanned.

use std::collections::BTreeMap;

use proc_macro2::{Span, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Expr, ForeignItem, ImplItem, Item, Stmt, TraitItem, UseTree};

use super::TCB_ANNEXES;
use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{
    self, Source, foreign_item_gated, impl_item_gated, item_gated, stmt_gated, trait_item_gated,
};

/// Crates both halves skip, by directory name: slopos-ostd, the sanctioned
/// allocation surface, and the userland crates.
const CARVE_OUTS: &[&str] = &[
    "userland",
    "terminal-core",
    "shell-core",
    "editor-core",
    "shooter-core",
    "http-core",
    "tls-core",
    "fat-core",
    "tree-core",
    "extension-core",
    "accounts-core",
    "settings-core",
    "remote-core",
    "installer-core",
    "priv-core",
    "chrome-core",
    "fate-core",
    "slibc",
    "slop-protocol",
    "image",
    "slopos-ostd",
];

/// The one file outside slopos-ostd that may name `alloc`.
const SOURCE_ALLOWLIST: &[&str] = &["kernel/src/main.rs"];

/// Directories the manifest walk never enters.
const PRUNED: &[&str] = &["builddir", "third_party", "target", ".git"];

/// How deep below the root the manifest walk looks: `a/b/Cargo.toml` at most,
/// where the workspace's crates live.
const MANIFEST_DEPTH: usize = 2;

pub fn run(repo: &Repo) -> Result<Report> {
    let mut report = Report::new();
    let manifests = manifests(repo)?;
    if manifests.is_empty() {
        return Err("no crate manifests found — the manifest scan would be a no-op".into());
    }
    for m in &manifests {
        scan_manifest(m, &repo.read(m)?, &mut report);
    }

    let all = repo.rs_files()?;
    if all.is_empty() {
        return Err(format!(
            "no Rust sources found under {} — the scan would be a no-op",
            repo.root.display()
        )
        .into());
    }
    let files: Vec<&String> = all.iter().filter(|f| source_in_scope(f)).collect();
    if files.is_empty() {
        return Err("every Rust source is exempt — the scan would be a no-op".into());
    }
    for f in &files {
        scan_source(&Source::load(repo, f)?, &mut report);
    }
    report.summary(format!(
        "scanned {} crate manifests and {} source files outside slopos-ostd and the userland crates",
        manifests.len(),
        files.len()
    ));
    Ok(report)
}

/// Repo-relative member manifests in scope: at most [`MANIFEST_DEPTH`]
/// directories down, not the workspace root, not a carve-out (by the crate
/// directory's name), not a TCB annex.
fn manifests(repo: &Repo) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![(String::new(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(repo.path(&dir))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if dir.is_empty() {
                name.clone()
            } else {
                format!("{dir}/{name}")
            };
            if entry.file_type()?.is_dir() {
                if depth < MANIFEST_DEPTH && !PRUNED.contains(&name.as_str()) {
                    stack.push((rel, depth + 1));
                }
            } else if name == "Cargo.toml" && !dir.is_empty() {
                let crate_name = dir.rsplit('/').next().unwrap_or(&dir);
                if !CARVE_OUTS.contains(&crate_name)
                    && !TCB_ANNEXES.iter().any(|a| dir == *a || under(&dir, a))
                {
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Flag each line of a manifest that declares a dependency named `alloc` in
/// a dependency table: `alloc = ...` / `alloc.workspace = ...` under
/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]` or their
/// `[target.<triple or 'cfg(...)'>.*]` forms, and the table form
/// `[dependencies.alloc]`. A `[features]` entry named `alloc` forwards a
/// feature and is not a dependency.
fn scan_manifest(path: &str, text: &str, report: &mut Report) {
    let mut in_deps = false;
    for (n, raw) in text.lines().enumerate() {
        let line = strip_toml_comment(raw).trim();
        if let Some(header) = line.strip_prefix('[') {
            if header.starts_with('[') {
                in_deps = false;
                continue;
            }
            let Some(keys) = header.strip_suffix(']').map(table_keys) else {
                in_deps = false;
                continue;
            };
            let (table, rest) = dependency_table(&keys);
            in_deps = table && rest.is_empty();
            if table && rest.len() == 1 && rest[0] == "alloc" {
                report.at(path, n + 1, manifest_message(raw));
            }
            continue;
        }
        if in_deps {
            let key = line
                .split(['=', '.'])
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            let has_value = line.contains('=');
            if key == "alloc" && has_value {
                report.at(path, n + 1, manifest_message(raw));
            }
        }
    }
}

fn manifest_message(line: &str) -> String {
    format!(
        "crate manifest declares an `alloc` dependency (`{}`) — kernel crates must route heap \
         allocation through slopos_ostd::mm::heap instead",
        line.trim()
    )
}

/// A TOML line with any `#` comment outside a string removed.
fn strip_toml_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '#') => return &line[..i],
            _ => {}
        }
    }
    line
}

/// The dotted keys of a table header, quotes removed:
/// `target.'cfg(unix)'.dependencies` → `["target", "cfg(unix)", "dependencies"]`.
fn table_keys(header: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut cur = String::new();
    let mut quote = None;
    for c in header.chars() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), _) if c == q => quote = None,
            (None, '.') => keys.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    keys.push(cur.trim().to_string());
    keys
}

/// Whether `keys` open a dependency table, and the keys past it (a
/// dependency's own table: `[dependencies.alloc]`).
fn dependency_table(keys: &[String]) -> (bool, &[String]) {
    let is_deps = |k: &str| {
        matches!(
            k,
            "dependencies" | "dev-dependencies" | "build-dependencies"
        )
    };
    match keys {
        [first, rest @ ..] if is_deps(first) => (true, rest),
        [target, _, deps, rest @ ..] if target == "target" && is_deps(deps) => (true, rest),
        _ => (false, &[]),
    }
}

fn source_in_scope(path: &str) -> bool {
    !CARVE_OUTS.iter().any(|d| under(path, d))
        && !TCB_ANNEXES.iter().any(|d| under(path, d))
        && !SOURCE_ALLOWLIST.contains(&path)
}

fn scan_source(src: &Source, report: &mut Report) {
    let mut v = AllocVisitor::default();
    v.visit_file(&src.file);
    for (line, what) in v.hits {
        report.at(
            &src.path,
            line,
            format!(
                "{what} names `alloc` directly — only kernel/src/main.rs and slopos-ostd may; go \
                 through slopos_ostd::* instead"
            ),
        );
    }
}

/// The `extern crate alloc` and `use alloc::` items of a file, one per line.
#[derive(Default)]
struct AllocVisitor {
    hits: BTreeMap<usize, &'static str>,
}

impl AllocVisitor {
    fn hit(&mut self, span: Span, what: &'static str) {
        self.hits.entry(rust::line(span)).or_insert(what);
    }

    fn tokens(&mut self, tokens: &TokenStream) {
        for (span, what) in alloc_tokens(tokens) {
            self.hit(span, what);
        }
    }
}

/// Whether a `use` tree imports from the `alloc` crate at its root.
fn uses_alloc(tree: &UseTree) -> bool {
    match tree {
        UseTree::Path(p) => p.ident == "alloc",
        UseTree::Name(n) => n.ident == "alloc",
        UseTree::Rename(r) => r.ident == "alloc",
        UseTree::Group(g) => g.items.iter().any(uses_alloc),
        UseTree::Glob(_) => false,
    }
}

impl<'ast> Visit<'ast> for AllocVisitor {
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        if i.ident == "alloc" {
            self.hit(i.extern_token.span, "`extern crate alloc`");
        }
        visit::visit_item_extern_crate(self, i);
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        if uses_alloc(&i.tree) {
            self.hit(i.use_token.span, "`use alloc::`");
        }
        visit::visit_item_use(self, i);
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
        if !impl_item_gated(i) {
            visit::visit_impl_item(self, i);
        }
    }

    fn visit_trait_item(&mut self, i: &'ast TraitItem) {
        if !trait_item_gated(i) {
            visit::visit_trait_item(self, i);
        }
    }

    fn visit_foreign_item(&mut self, i: &'ast ForeignItem) {
        if !foreign_item_gated(i) {
            visit::visit_foreign_item(self, i);
        }
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

/// `extern crate alloc` and `use alloc::` / `use ::alloc::` written inside
/// macro tokens (a `macro_rules!` body, say).
fn alloc_tokens(tokens: &TokenStream) -> Vec<(Span, &'static str)> {
    let mut out = Vec::new();
    collect_alloc(tokens, &mut out);
    out
}

fn collect_alloc(tokens: &TokenStream, out: &mut Vec<(Span, &'static str)>) {
    let tts: Vec<TokenTree> = tokens.clone().into_iter().collect();
    let ident = |i: usize, s: &str| matches!(tts.get(i), Some(TokenTree::Ident(id)) if id == s);
    let colons = |i: usize| {
        matches!(
            (tts.get(i), tts.get(i + 1)),
            (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b))) if a.as_char() == ':' && b.as_char() == ':'
        )
    };
    for (i, tt) in tts.iter().enumerate() {
        match tt {
            TokenTree::Ident(id)
                if id == "extern" && ident(i + 1, "crate") && ident(i + 2, "alloc") =>
            {
                out.push((id.span(), "`extern crate alloc`"));
            }
            TokenTree::Ident(id) if id == "use" => {
                let at = if colons(i + 1) { i + 3 } else { i + 1 };
                if ident(at, "alloc") && colons(at + 1) {
                    out.push((id.span(), "`use alloc::`"));
                }
            }
            TokenTree::Group(g) => collect_alloc(&g.stream(), out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    fn manifest_findings(tree: &Tree) -> Vec<String> {
        let mut report = Report::new();
        for m in manifests(&tree.repo).unwrap() {
            scan_manifest(&m, &tree.repo.read(&m).unwrap(), &mut report);
        }
        report.findings.iter().map(|f| f.location.clone()).collect()
    }

    fn source_findings(files: &[(&str, &str)]) -> Vec<String> {
        let mut report = Report::new();
        for (path, text) in files {
            if source_in_scope(path) {
                scan_source(&Source::parse(path, text.to_string()).unwrap(), &mut report);
            }
        }
        report.findings.iter().map(|f| f.location.clone()).collect()
    }

    const DEP: &str = "[dependencies]\nalloc = \"1\"\n";

    #[test]
    fn manifest_alloc_dependencies_are_flagged() {
        let tree = Tree::new(&[
            (
                "mm/Cargo.toml",
                "[package]\nname = \"slopos-mm\"\n\n[dependencies]\nalloc = \"1\"\n\n\
                 [target.x86_64-slos.dependencies]\nalloc = \"1\"\n",
            ),
            (
                "sched/Cargo.toml",
                "[dependencies]\nalloc.workspace = true\n",
            ),
            ("net/sub/Cargo.toml", "[dev-dependencies]\nalloc = \"1\"\n"),
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"mm\"]\n\n[dependencies]\nalloc = \"1\"\n",
            ),
            ("gfx/Cargo.toml", "[features]\nalloc = [\"dep:foo\"]\n"),
            ("userland/Cargo.toml", DEP),
            ("slopos-ostd/Cargo.toml", DEP),
            ("vendor/unwinding/Cargo.toml", DEP),
            ("a/b/c/Cargo.toml", DEP),
        ]);
        assert_eq!(
            manifest_findings(&tree),
            [
                "mm/Cargo.toml:5",
                "mm/Cargo.toml:8",
                "net/sub/Cargo.toml:2",
                "sched/Cargo.toml:2"
            ]
        );
    }

    #[test]
    fn manifest_forms_the_line_scan_missed_are_flagged() {
        let tree = Tree::new(&[
            (
                "fs/Cargo.toml",
                "[target.'cfg(target_os = \"none\")'.dependencies]\nalloc = \"1\"\n\
                 [dependencies.alloc]\nversion = \"1\"\n\
                 [build-dependencies]\n\"alloc\" = { version = \"1\" }  # quoted key\n",
            ),
            (
                "net/Cargo.toml",
                "[dependencies]\nallocator = \"1\"\nfoo = { features = [\"alloc\"] }\n\
                 # alloc = \"1\"\n[package.metadata.dependencies]\nalloc = \"1\"\n\
                 [[bin]]\nalloc = 1\n",
            ),
        ]);
        assert_eq!(
            manifest_findings(&tree),
            ["fs/Cargo.toml:2", "fs/Cargo.toml:3", "fs/Cargo.toml:6"]
        );
    }

    #[test]
    fn source_alloc_imports_are_flagged() {
        let hits = source_findings(&[
            (
                "mm/src/lib.rs",
                "extern crate alloc;\nuse alloc::vec::Vec;\n",
            ),
            ("mm/src/b.rs", "use ::alloc::boxed::Box;\n"),
        ]);
        assert_eq!(
            hits,
            ["mm/src/lib.rs:1", "mm/src/lib.rs:2", "mm/src/b.rs:1"]
        );
    }

    #[test]
    fn gated_quoted_allowlisted_and_carved_out_sources_stay_silent() {
        let negatives = concat!(
            "#[cfg(feature = \"std\")]\n",
            "use alloc::vec::Vec;\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    extern crate alloc;\n",
            "}\n",
            "fn quoted() { let s = \"use alloc::vec::Vec;\"; let _ = s; }\n",
            "/* use alloc::vec::Vec; */\n",
            "use allocator::Heap;\n",
            "use crate::alloc::Thing;\n",
        );
        let hits = source_findings(&[
            ("mm/src/negatives.rs", negatives),
            ("kernel/src/main.rs", "extern crate alloc;\n"),
            ("terminal-core/src/x.rs", "use alloc::vec::Vec;\n"),
            ("slopos-ostd/src/x.rs", "use alloc::vec::Vec;\n"),
            ("vendor/gimli/src/x.rs", "use alloc::vec::Vec;\n"),
        ]);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn forms_the_line_scan_missed_are_flagged() {
        let src = concat!(
            "pub use alloc::vec::Vec;\n",
            "fn f() {\n",
            "    use alloc::boxed::Box;\n",
            "}\n",
            "use {alloc::string::String, core::fmt};\n",
            "extern crate alloc as heap;\n",
            "macro_rules! m { () => { use ::alloc::rc::Rc; }; }\n",
            "#[cfg(test)] fn g() {} use alloc::sync::Arc;\n",
        );
        assert_eq!(
            source_findings(&[("vendor/other/src/a.rs", src)]),
            [
                "vendor/other/src/a.rs:1",
                "vendor/other/src/a.rs:3",
                "vendor/other/src/a.rs:5",
                "vendor/other/src/a.rs:6",
                "vendor/other/src/a.rs:7",
                "vendor/other/src/a.rs:8",
            ]
        );
    }
}
