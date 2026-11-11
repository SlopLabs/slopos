//! Kernel-side destructors do not panic directly.
//!
//! Destructors run on cleanup paths and may run during unwinding; a panic
//! from `Drop` turns a recoverable unwind into an abort or masks the original
//! fault. Inside the `fn drop` of every `impl Drop for` outside the userland
//! crates and the pinned TCB annexes, this gate flags the direct panic
//! triggers: `panic!`, `todo!`, `unimplemented!`, `unreachable!`, the
//! `assert*!` and `debug_assert*!` families, and `.unwrap()`, `.expect(..)`,
//! `.unwrap_err()`, `.expect_err(..)`. It is syntactic: a destructor calling a
//! helper that panics is not followed.
//!
//! Reviewed exceptions, each one call by file, macro and message:
//! - `PreemptGuard::drop` keeps its always-on `preempt_count underflow`
//!   invariant assert;
//! - the tty and ext4 inode destructors keep debug-only assertions that catch
//!   a missing explicit cleanup during development;
//! - the `panic.nested_drop_smoke` boot hook's guard panics by design: it is
//!   the fault injection proving a Drop panic mid-unwind lands on the fatal
//!   path.

use std::collections::BTreeMap;

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use syn::visit::{self, Visit};

use crate::Result;
use crate::repo::{Repo, under};
use crate::report::Report;
use crate::rust::{self, Source};

/// Userland crates, outside the framekernel discipline, and the pinned TCB
/// annexes, which the vendor-pin gate covers instead.
const OUT_OF_SCOPE: &[&str] = &[
    "userland",
    "terminal-core",
    "editor-core",
    "shooter-core",
    "http-core",
    "tls-core",
    "slibc",
    "slop-protocol",
    "appkit",
    "image",
    "slopos-rt",
    "vendor/unwinding",
    "vendor/gimli",
];

const PANIC_MACROS: &[&str] = &[
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
];

const PANIC_METHODS: &[&str] = &["unwrap", "expect", "unwrap_err", "expect_err"];

/// A reviewed panic inside a destructor: the file it is in, the macro, and
/// its message — the first string literal among the macro's arguments.
struct Exception {
    file: &'static str,
    macro_name: &'static str,
    message: &'static str,
}

const REVIEWED: &[Exception] = &[
    Exception {
        file: "slopos-ostd/src/cpu/preempt.rs",
        macro_name: "assert",
        message: "preempt_count underflow",
    },
    Exception {
        file: "drivers/src/tty/mod.rs",
        macro_name: "debug_assert",
        message: "PostLockWork dropped with pending deferred work — call .execute()",
    },
    Exception {
        file: "fs/src/ext4/inode.rs",
        macro_name: "debug_assert",
        message: "InodeHandle for inode {} dropped while dirty — call flush first",
    },
    Exception {
        file: "boot/src/early_init.rs",
        macro_name: "panic",
        message: "panic.nested_drop_smoke: Drop panic during unwind",
    },
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
    let mut destructors = 0;
    for f in &files {
        destructors += scan_source(&Source::load(repo, f)?, &mut report);
    }
    report.summary(format!(
        "scanned {destructors} Drop impls in {} files outside the userland crates and the TCB annexes",
        files.len()
    ));
    Ok(report)
}

fn in_scope(path: &str) -> bool {
    !OUT_OF_SCOPE.iter().any(|d| under(path, d))
}

/// Report the file's destructor panics; returns how many `fn drop` bodies of
/// `impl Drop for` it holds.
fn scan_source(src: &Source, report: &mut Report) -> usize {
    let mut v = DropVisitor {
        file: &src.path,
        in_drop: false,
        destructors: 0,
        hits: BTreeMap::new(),
    };
    v.visit_file(&src.file);
    for (line, what) in v.hits {
        report.at(
            &src.path,
            line,
            format!(
                "{what} inside Drop — destructors must not panic; return the error before \
                 ownership reaches Drop, or make the cleanup best-effort"
            ),
        );
    }
    v.destructors
}

struct DropVisitor<'a> {
    file: &'a str,
    in_drop: bool,
    destructors: usize,
    hits: BTreeMap<usize, String>,
}

impl DropVisitor<'_> {
    fn hit(&mut self, span: Span, what: String) {
        self.hits.entry(rust::line(span)).or_insert(what);
    }

    fn reviewed(&self, macro_name: &str, tokens: &TokenStream) -> bool {
        let message = first_str_literal(tokens);
        REVIEWED.iter().any(|e| {
            e.file == self.file
                && e.macro_name == macro_name
                && message.as_deref() == Some(e.message)
        })
    }

    /// Panic triggers inside the arguments of some other macro (`klog!(..,
    /// x.unwrap())`), which syn leaves as tokens.
    fn tokens(&mut self, tokens: &TokenStream) {
        let tts: Vec<TokenTree> = tokens.clone().into_iter().collect();
        for (i, tt) in tts.iter().enumerate() {
            match tt {
                TokenTree::Ident(id) => {
                    let name = id.to_string();
                    let bang =
                        matches!(tts.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!');
                    let args = match tts.get(i + 2) {
                        Some(TokenTree::Group(g)) if bang => Some(g),
                        _ => None,
                    };
                    let dot =
                        i > 0 && matches!(&tts[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
                    let call = matches!(tts.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis);
                    if let Some(args) = args.filter(|_| PANIC_MACROS.contains(&name.as_str())) {
                        if !self.reviewed(&name, &args.stream()) {
                            self.hit(id.span(), format!("`{name}!`"));
                        }
                    } else if dot && call && PANIC_METHODS.contains(&name.as_str()) {
                        self.hit(id.span(), format!("`.{name}()`"));
                    }
                }
                TokenTree::Group(g) => self.tokens(&g.stream()),
                _ => {}
            }
        }
    }
}

/// The value of the first string literal at the top level of a macro's
/// arguments: the message of `assert!(cond, "msg", ..)` or `panic!("msg")`.
fn first_str_literal(tokens: &TokenStream) -> Option<String> {
    tokens.clone().into_iter().find_map(|tt| match tt {
        TokenTree::Literal(l) => match syn::Lit::new(l) {
            syn::Lit::Str(s) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

impl<'ast> Visit<'ast> for DropVisitor<'_> {
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let is_drop = matches!(&i.trait_, Some((None, path, _)) if rust::last_ident(path).as_deref() == Some("Drop"));
        if !is_drop || self.in_drop {
            return visit::visit_item_impl(self, i);
        }
        for item in &i.items {
            match item {
                syn::ImplItem::Fn(f) if f.sig.ident == "drop" => {
                    self.destructors += 1;
                    self.in_drop = true;
                    visit::visit_impl_item_fn(self, f);
                    self.in_drop = false;
                }
                other => self.visit_impl_item(other),
            }
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.in_drop {
            let name = rust::last_ident(&m.path).unwrap_or_default();
            if PANIC_MACROS.contains(&name.as_str()) {
                if !self.reviewed(&name, &m.tokens) {
                    self.hit(m.bang_token.span, format!("`{name}!`"));
                }
            } else {
                self.tokens(&m.tokens);
            }
        }
        visit::visit_macro(self, m);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if self.in_drop {
            let name = e.method.to_string();
            if PANIC_METHODS.contains(&name.as_str()) {
                self.hit(e.method.span(), format!("`.{name}()`"));
            }
        }
        visit::visit_expr_method_call(self, e);
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

    fn drop_body(body: &str) -> String {
        format!("impl Drop for D {{\n    fn drop(&mut self) {{\n{body}    }}\n}}\n")
    }

    #[test]
    fn every_trigger_form_and_a_generic_header_are_flagged() {
        let positives = concat!(
            "impl Drop for Foo {\n",
            "    fn drop(&mut self) {\n",
            "        panic!(\"boom\");\n",
            "        assert!(self.ok);\n",
            "        debug_assert_eq!(self.a, self.b);\n",
            "        let _ = self.slot.unwrap();\n",
            "        let _ = self.slot.expect(\"gone\");\n",
            "        let _ = self.res.unwrap_err();\n",
            "        todo!();\n",
            "        unreachable!();\n",
            "    }\n",
            "}\n",
            "impl<T> Drop for Bar<T> {\n",
            "    fn drop(&mut self) {\n",
            "        panic!(\"generic impl header must still enter the state machine\");\n",
            "    }\n",
            "}\n",
        );
        let hits = findings(&[("mm/src/positives.rs", positives)]);
        let want: Vec<String> = [3, 4, 5, 6, 7, 8, 9, 10, 15]
            .iter()
            .map(|l| format!("mm/src/positives.rs:{l}"))
            .collect();
        assert_eq!(hits, want);
    }

    #[test]
    fn panics_outside_destructors_and_comments_stay_silent() {
        let negatives = concat!(
            "fn before_any_drop_impl() {\n",
            "    panic!(\"not a destructor\");\n",
            "    let _ = opt.unwrap();\n",
            "}\n",
            "impl Drop for Quiet {\n",
            "    fn drop(&mut self) {\n",
            "        // panic!(\"commented out\")\n",
            "        self.release();\n",
            "    }\n",
            "}\n",
            "fn after_the_drop_body_closed() {\n",
            "    panic!(\"the brace counter must have left the destructor\");\n",
            "    let _ = opt.expect(\"still not a destructor\");\n",
            "}\n",
            "impl Clone for Quiet {\n",
            "    fn drop(&mut self) { panic!(\"not Drop\"); }\n",
            "}\n",
        );
        let hits = findings(&[
            ("mm/src/negatives.rs", negatives),
            (
                "userland/src/x.rs",
                "impl Drop for U {\n    fn drop(&mut self) { panic!(\"userland is out of scope\"); }\n}\n",
            ),
            (
                "vendor/unwinding/src/x.rs",
                "impl Drop for A {\n    fn drop(&mut self) { panic!(\"pinned annex\"); }\n}\n",
            ),
        ]);
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn a_non_annex_vendor_crate_is_in_scope() {
        let src = "impl Drop for V {\n    fn drop(&mut self) { panic!(\"in scope\"); }\n}\n";
        assert_eq!(
            findings(&[("vendor/othercrate/src/x.rs", src)]),
            ["vendor/othercrate/src/x.rs:2"]
        );
    }

    /// Each exception excuses one call — by file, macro and message — not the
    /// destructor or the file.
    #[test]
    fn reviewed_exceptions_excuse_one_call_each() {
        let preempt = drop_body(concat!(
            "        assert!(self.count > 0, \"preempt_count underflow\");\n",
            "        panic!(\"a second, unreviewed panic in the same destructor\");\n",
            "        debug_assert!(self.count > 0, \"preempt_count underflow\");\n",
        ));
        let tty = drop_body(concat!(
            "        debug_assert!(\n",
            "            self.executed,\n",
            "            \"PostLockWork dropped with pending deferred work — call .execute()\"\n",
            "        );\n",
            "        assert!(self.drained);\n",
        ));
        let inode = drop_body(concat!(
            "        debug_assert!(\n",
            "            !self.dirty,\n",
            "            \"InodeHandle for inode {} dropped while dirty — call flush first\",\n",
            "            self.num.raw()\n",
            "        );\n",
        ));
        let boot =
            drop_body("        panic!(\"panic.nested_drop_smoke: Drop panic during unwind\");\n");
        let hits = findings(&[
            ("slopos-ostd/src/cpu/preempt.rs", &preempt),
            ("drivers/src/tty/mod.rs", &tty),
            ("fs/src/ext4/inode.rs", &inode),
            ("boot/src/early_init.rs", &boot),
            ("mm/src/elsewhere.rs", &boot),
        ]);
        assert_eq!(
            hits,
            [
                "slopos-ostd/src/cpu/preempt.rs:4",
                "slopos-ostd/src/cpu/preempt.rs:5",
                "drivers/src/tty/mod.rs:7",
                "mm/src/elsewhere.rs:3",
            ]
        );
    }

    /// The forms the line-based scan got wrong: a `'}'` char literal or a
    /// `"http://x"` string ahead of the trigger, a header split across lines,
    /// and a nested block whose closing brace the counter misread.
    #[test]
    fn literals_split_headers_and_nested_blocks_do_not_hide_a_trigger() {
        let src = concat!(
            "impl Drop for A {\n",
            "    fn drop(&mut self) {\n",
            "        let c = '}'; let _ = self.x.unwrap();\n",
            "    }\n",
            "}\n",
            "impl Drop for B {\n",
            "    fn drop(&mut self) {\n",
            "        log(\"http://x\"); let _ = self.x.expect(\"y\");\n",
            "    }\n",
            "}\n",
            "impl<T>\n",
            "    Drop\n",
            "    for C<T>\n",
            "where\n",
            "    T: Copy,\n",
            "{\n",
            "    fn drop(&mut self) {\n",
            "        todo!()\n",
            "    }\n",
            "}\n",
            "impl Drop for E {\n",
            "    fn drop(&mut self) {\n",
            "        if self.a { for x in self.v.iter() { let _ = x; } }\n",
            "        { }\n",
            "        unimplemented! {}\n",
            "        let _ = core::ops::Drop::drop;\n",
            "        klog!(\"{}\", self.y.unwrap_err());\n",
            "        let f = || assert_ne![1, 2];\n",
            "    }\n",
            "}\n",
            "impl core::ops::Drop for F {\n",
            "    fn drop(&mut self) { /* } */ self.z.expect_err(\"q\"); }\n",
            "}\n",
        );
        let hits = findings(&[("mm/src/bypass.rs", src)]);
        let want: Vec<String> = [3, 8, 18, 25, 27, 28, 32]
            .iter()
            .map(|l| format!("mm/src/bypass.rs:{l}"))
            .collect();
        assert_eq!(hits, want);
    }

    /// Out of scope by design: the gate is syntactic and does not follow a
    /// destructor into the functions it calls.
    #[test]
    fn a_helper_that_panics_is_not_followed() {
        let src = concat!(
            "fn release(x: Option<u8>) { x.unwrap(); }\n",
            "impl Drop for G {\n",
            "    fn drop(&mut self) { release(self.x); }\n",
            "}\n",
        );
        assert!(findings(&[("mm/src/helper.rs", src)]).is_empty());
    }
}
