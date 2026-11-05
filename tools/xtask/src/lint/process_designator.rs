//! A process-keyed table is entered through a designator that carries a
//! generation, never through a bare `u32` process id.
//!
//! A `u32` says nothing about whether the process it names still exists, and
//! ids are recycled, so a stale one silently designates whichever process
//! holds that number now: a confused-deputy surface where the kernel services
//! the call against a stranger's address space or open files.
//! `slopos_ostd::process::ProcessId` and `slopos_fs::fileio::FdTable` can only
//! be built from a live process, so a stale one fails the generation check
//! instead of resolving to the slot's new occupant.
//!
//! Two checks, in the modules that own process-keyed tables
//! (`mm/src/process_vm.rs` and `fs/src/fileio/`; a `u32` pid elsewhere, as in
//! `getpid` or the syscall ABI, is often correct):
//!
//! 1. No `pub` fn takes a `u32` parameter whose name means "process id" (ends
//!    in `pid`, `process_id` or `parent_id`). Returning a `u32` (`fn id(self)
//!    -> u32`) is the point of having a designator, and a private or
//!    `pub(crate)` helper cannot be reached from another crate.
//! 2. No lookup by number: a `process_id` compared with `==` inside a loop or
//!    iterator chain over `PROCESS_TABLES`, `PROCESS_VMS` or `0..MAX_PROCESSES`.
//!    A designator names its slot. A comparison against the
//!    `INVALID_PROCESS_ID` sentinel looks for a free slot, not a process, and
//!    is accepted.
//!
//! A scoped path with no sources is a finding, so the gate cannot pass by
//! scanning nothing.

use std::collections::BTreeSet;

use syn::visit::{self, Visit};
use syn::{BinOp, Expr, FnArg, Macro, Pat, Signature, Type, Visibility};

use crate::Result;
use crate::repo::Repo;
use crate::report::Report;
use crate::rust::{self, Source, visit_expansion};

const PROCESS_VM: &str = "mm/src/process_vm.rs";
const FILEIO: &str = "fs/src/fileio";

/// Parameter-name endings that mean "a process id" in this tree: `pid`,
/// `caller_pid`, `process_id`, `src_process_id`, `parent_id`, ...
const PID_SUFFIXES: &[&str] = &["pid", "process_id", "parent_id"];

/// What a loop over the process tables iterates.
const TABLES: &[&str] = &["PROCESS_TABLES", "PROCESS_VMS", "MAX_PROCESSES"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Check {
    PubPid,
    PidScan,
}

impl Check {
    fn message(self) -> &'static str {
        match self {
            Check::PubPid => {
                "public entry point takes a bare u32 process id — ids recycle, so a stale one \
                 resolves to whichever process holds that number now; take \
                 slopos_ostd::process::ProcessId (or slopos_fs::fileio::FdTable), which carry \
                 a generation and fail closed on a rebound slot"
            }
            Check::PidScan => {
                "process table scanned for a matching process_id — a lookup by number resolves \
                 a recycled id to the slot's new occupant; index the slot through a \
                 ProcessId/FdTable designator and check its generation"
            }
        }
    }
}

pub fn run(repo: &Repo) -> Result<Report> {
    let mut report = Report::new();
    let mut files = Vec::new();
    if repo.exists(PROCESS_VM) {
        files.push(PROCESS_VM.to_string());
    } else {
        report.finding(
            PROCESS_VM,
            "scoped file no longer exists — fix PROCESS_VM in \
             tools/xtask/src/lint/process_designator.rs",
        );
    }
    let fileio = repo.rs_files_under(&[FILEIO])?;
    if fileio.is_empty() {
        report.finding(
            FILEIO,
            "no Rust sources under the scoped directory — fix FILEIO in \
             tools/xtask/src/lint/process_designator.rs",
        );
    }
    files.extend(fileio);
    if files.is_empty() {
        return Err("none of the scoped paths hold sources — the scan would be a no-op".into());
    }
    for path in &files {
        for (line, check) in scan(&Source::load(repo, path)?) {
            report.at(path, line, check.message());
        }
    }
    report.summary(format!(
        "scanned {} files owning process-keyed tables ({PROCESS_VM}, {FILEIO}/)",
        files.len()
    ));
    Ok(report)
}

fn scan(src: &Source) -> BTreeSet<(usize, Check)> {
    let mut v = Scan::default();
    v.visit_file(&src.file);
    v.hits
}

#[derive(Default)]
struct Scan {
    hits: BTreeSet<(usize, Check)>,
}

impl Scan {
    fn signature(&mut self, vis: &Visibility, sig: &Signature) {
        if !matches!(vis, Visibility::Public(_)) {
            return;
        }
        for arg in &sig.inputs {
            let FnArg::Typed(pt) = arg else {
                continue;
            };
            if let Some(name) = binding_name(&pt.pat)
                && PID_SUFFIXES.iter().any(|s| name.ends_with(s))
                && is_u32(&pt.ty)
            {
                self.hits
                    .insert((rust::line(pt.colon_token.span), Check::PubPid));
            }
        }
    }
}

/// Check 2's comparisons, within one loop body or iterator argument.
struct Lookups<'h>(&'h mut BTreeSet<(usize, Check)>);

impl<'ast> Visit<'ast> for Lookups<'_> {
    fn visit_expr_binary(&mut self, i: &'ast syn::ExprBinary) {
        if let BinOp::Eq(eq) = i.op
            && (mentions(&i.left, "process_id") || mentions(&i.right, "process_id"))
            && !is_sentinel(&i.left)
            && !is_sentinel(&i.right)
        {
            self.0.insert((rust::line(eq.spans[0]), Check::PidScan));
        }
        visit::visit_expr_binary(self, i);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
    }
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.signature(&i.vis, &i.sig);
        visit::visit_item_fn(self, i);
    }

    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.signature(&i.vis, &i.sig);
        visit::visit_impl_item_fn(self, i);
    }

    fn visit_expr_for_loop(&mut self, i: &'ast syn::ExprForLoop) {
        if over_tables(&i.expr) {
            Lookups(&mut self.hits).visit_block(&i.body);
        }
        visit::visit_expr_for_loop(self, i);
    }

    fn visit_expr_while(&mut self, i: &'ast syn::ExprWhile) {
        if over_tables(&i.cond) {
            Lookups(&mut self.hits).visit_block(&i.body);
        }
        visit::visit_expr_while(self, i);
    }

    fn visit_expr_method_call(&mut self, i: &'ast syn::ExprMethodCall) {
        if over_tables(&i.receiver) {
            for arg in &i.args {
                Lookups(&mut self.hits).visit_expr(arg);
            }
        }
        visit::visit_expr_method_call(self, i);
    }

    fn visit_macro(&mut self, i: &'ast Macro) {
        visit_expansion(self, i);
    }
}

fn binding_name(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(p) => Some(p.ident.to_string()),
        Pat::Reference(r) => binding_name(&r.pat),
        Pat::Paren(p) => binding_name(&p.pat),
        _ => None,
    }
}

fn is_u32(ty: &Type) -> bool {
    match ty {
        Type::Path(p) => p.qself.is_none() && p.path.is_ident("u32"),
        Type::Group(g) => is_u32(&g.elem),
        Type::Paren(p) => is_u32(&p.elem),
        _ => false,
    }
}

fn over_tables(e: &Expr) -> bool {
    TABLES.iter().any(|t| mentions(e, t))
}

fn is_sentinel(e: &Expr) -> bool {
    matches!(e, Expr::Path(p) if rust::last_ident(&p.path).as_deref() == Some("INVALID_PROCESS_ID"))
}

/// Whether `e` names `ident` anywhere: a path, field, method or macro token.
fn mentions(e: &Expr, ident: &str) -> bool {
    struct Mentions<'n>(&'n str, bool);
    impl<'ast> Visit<'ast> for Mentions<'_> {
        fn visit_ident(&mut self, i: &'ast proc_macro2::Ident) {
            self.1 |= i == self.0;
        }
        fn visit_macro(&mut self, i: &'ast Macro) {
            visit_expansion(self, i);
        }
    }
    let mut v = Mentions(ident, false);
    v.visit_expr(e);
    v.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const PROCESS_VM_RS: &str = r#"pub fn process_vm_get_stack_top(process_id: u32) -> u64 { 0 }
pub fn process_vm_ok(process: ProcessId) -> u64 { 0 }
fn private_helper(pid: u32) -> u64 { 0 }
impl FdTable {
    pub fn id(self) -> u32 { self.id }
}
"#;

    const FDOPS_RS: &str = r#"pub fn file_close_fd(
    pid: u32,
    fd: c_int,
) -> i32 { 0 }
pub fn file_read_fd(table: FdTable, fd: c_int) -> i32 { 0 }
"#;

    const MOD_RS: &str = r#"pub(super) fn slot_for_pid(pid_arg: u32) -> Option<&'static FileTableSlot> {
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) == pid_arg {
            return Some(slot);
        }
    }
    None
}
"#;

    fn hits(text: &str) -> Vec<(usize, Check)> {
        scan(&Source::parse("fs/src/fileio/x.rs", text.to_string()).unwrap())
            .into_iter()
            .collect()
    }

    #[test]
    fn self_test_fixtures_fire_at_their_counts() {
        let tree = Tree::new(&[
            ("mm/src/process_vm.rs", PROCESS_VM_RS),
            ("fs/src/fileio/fdops.rs", FDOPS_RS),
            ("fs/src/fileio/mod.rs", MOD_RS),
        ]);
        let report = run(&tree.repo).unwrap();
        let mut found: Vec<_> = report
            .findings
            .iter()
            .map(|f| (f.location.as_str(), f.message == Check::PidScan.message()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("fs/src/fileio/fdops.rs:2", false),
                ("fs/src/fileio/mod.rs:3", true),
                ("mm/src/process_vm.rs:1", false),
            ]
        );
        assert!(
            report.summary.contains("scanned 3 files"),
            "{}",
            report.summary
        );
    }

    #[test]
    fn accepted_forms_stay_silent() {
        assert_eq!(hits(PROCESS_VM_RS), [(1, Check::PubPid)]);
        assert_eq!(hits(FDOPS_RS), [(2, Check::PubPid)]);
    }

    #[test]
    fn spellings_the_line_scan_missed() {
        let text = r#"
pub const fn a(caller_pid: u32) {}
pub unsafe fn b(x: u8, mut parent_process_id: u32) {}
pub extern "C" fn c(
    target_pid
        : u32,
) {}
pub(crate) fn d(pid: u32) {}
pub fn e(pid: &u32, pid_arg: u32, ppid: u64) {}
impl Tables {
    pub fn f(&self, src_process_id: u32) {}
}
"#;
        assert_eq!(
            hits(text),
            [
                (2, Check::PubPid),
                (3, Check::PubPid),
                (6, Check::PubPid),
                (11, Check::PubPid)
            ]
        );
    }

    #[test]
    fn table_scans_in_every_loop_shape() {
        let text = r#"
fn scans(pid: u32) {
    for i in 0..MAX_PROCESSES {
        let guard = PROCESS_VMS[i].lock();
        // A long comment, then the comparison far below the loop head.




        if guard.process_id == pid { return; }
    }
    let hit = PROCESS_TABLES.iter().position(|s| s.process_id.load(Ordering::Acquire) == pid);
    let mut i = 0;
    while i < MAX_PROCESSES {
        if pid == PROCESS_VMS[i].lock().process_id { return; }
        i += 1;
    }
}
fn accepted(handle: Handle) {
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) == INVALID_PROCESS_ID { return; }
        if slot.process_id.load(Ordering::Acquire) != INVALID_PROCESS_ID { continue; }
    }
    let s = "for slot in PROCESS_TABLES.iter() { slot.process_id == pid }";
    if guard.process_id == process.id() {}
}
"#;
        assert_eq!(
            hits(text),
            [
                (10, Check::PidScan),
                (12, Check::PidScan),
                (15, Check::PidScan)
            ]
        );
    }

    #[test]
    fn missing_scope_is_a_finding_and_no_scope_is_an_error() {
        let tree = Tree::new(&[("mm/src/process_vm.rs", PROCESS_VM_RS)]);
        let report = run(&tree.repo).unwrap();
        assert!(report.findings.iter().any(|f| f.location == FILEIO));
        let empty = Tree::new(&[("README", "")]);
        assert!(run(&empty.repo).is_err());
    }
}
