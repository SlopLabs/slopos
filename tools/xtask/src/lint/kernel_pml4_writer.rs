//! The kernel master's page tables have one writer. Every kernel-half
//! page-table write goes through `slopos_ostd::mm::vm_space::CursorMut` under
//! the `KERNEL_VM_SPACE` lock, and `slopos_mm::kernel_mappings` is the one
//! module outside OSTD that takes that cursor. The cursor's exclusivity comes
//! from the borrow checker over one `VmSpace`; a second module driving it, or
//! a second handle on the static, is a writer the review of that one module
//! does not cover. `verification/STATUS.md`'s tier-2 argument for `vm_space`
//! rests on this.
//!
//! Outside `slopos-ostd`, which owns the tables and is audited as the TCB, and
//! outside the host tools under `tools/`, which are not kernel code:
//!
//! - `KERNEL_VM_SPACE` is named only where it is defined
//!   (`kernel-services/src/kernel_vm_space.rs`) and installed at boot
//!   (`boot/src/boot_memory.rs`); everything else uses the
//!   `kernel_vm_space()` accessors.
//! - A file that names the space and takes a mutating cursor (`cursor_mut`,
//!   `CursorMut`) is `mm/src/kernel_mappings.rs`. Reads (`cursor`,
//!   `pml4_paddr`) and CR3 loads (`activate_*`) are allowed anywhere.
//! - A file reaching a page-table word through
//!   `ptr_buf::with_atomic_u64_in_page` only loads it.
//! - `mm/src/kernel_mappings.rs` does take the cursor, so the gate cannot pass
//!   on a writer that moved or was renamed.
//!
//! Matching is on tokens, so comments, doc comments and strings never count,
//! and macro bodies do.

use std::collections::HashMap;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use crate::Result;
use crate::repo::{self, Repo};
use crate::report::Report;

const WRITER: &str = "mm/src/kernel_mappings.rs";
const STATIC_HOMES: &[&str] = &[
    "kernel-services/src/kernel_vm_space.rs",
    "boot/src/boot_memory.rs",
];
const EXEMPT: &[&str] = &[
    "slopos-ostd",
    "slopos-ostd-derive",
    "verification",
    "vendor",
    "third_party",
    "builddir",
    "tools",
];
const STATIC: &str = "KERNEL_VM_SPACE";
const SPACE: &[&str] = &["kernel_vm_space", "try_kernel_vm_space", STATIC];
const CURSOR: &[&str] = &["cursor_mut", "CursorMut"];
const WORD_ACCESS: &str = "with_atomic_u64_in_page";

pub fn run(repo: &Repo) -> Result<Report> {
    let files: Vec<String> = repo
        .rs_files()?
        .into_iter()
        .filter(|f| !EXEMPT.iter().any(|d| repo::under(f, d)))
        .collect();
    if files.is_empty() {
        return Err("no Rust file outside slopos-ostd to scan".into());
    }
    let mut report = Report::new();
    let mut found_writer = false;
    let mut naming = 0;
    for file in &files {
        let text = repo.read(file)?;
        if !["KERNEL_VM_SPACE", "kernel_vm_space", WORD_ACCESS]
            .iter()
            .any(|s| text.contains(s))
        {
            continue;
        }
        let tokens: TokenStream = text
            .parse()
            .map_err(|e: proc_macro2::LexError| format!("{file}: {e}"))?;
        let mut scan = Scan::default();
        scan.walk(tokens);
        let first = |names: &[&str]| {
            names
                .iter()
                .filter_map(|n| scan.idents.get(*n))
                .min()
                .copied()
        };

        if let Some(line) = scan.idents.get(STATIC)
            && !STATIC_HOMES.contains(&file.as_str())
        {
            report.at(
                file,
                *line,
                "names `KERNEL_VM_SPACE` outside its definition and its boot install — reach \
                 the kernel space through the `kernel_vm_space()` accessors",
            );
        }
        if scan.idents.contains_key(WORD_ACCESS)
            && let Some((line, method)) = &scan.store
        {
            report.at(
                file,
                *line,
                format!(
                    "reaches a page-table word through `{WORD_ACCESS}` and writes one (`.{method}`) \
                     — page-table writes go through the kernel space's `CursorMut`; only load here"
                ),
            );
        }
        if first(SPACE).is_some() {
            naming += 1;
            if let Some(line) = first(CURSOR) {
                if file == WRITER {
                    found_writer = true;
                } else {
                    report.at(
                        file,
                        line,
                        format!(
                            "names the kernel space and takes its mutating cursor outside \
                             {WRITER} — add a kernel mapping helper there rather than a \
                             second writer"
                        ),
                    );
                }
            }
        }
    }
    if !found_writer {
        report.finding(
            WRITER,
            "takes no `kernel_vm_space()` `cursor_mut` — the one writer of the kernel master's \
             page tables moved or was renamed; point this gate at where it went",
        );
    }
    report.summary(format!(
        "scanned {} Rust files outside slopos-ostd and tools/, {naming} naming the kernel space; \
         {WRITER} is its one writer",
        files.len()
    ));
    Ok(report)
}

#[derive(Default)]
struct Scan {
    /// Each identifier of interest, at the line it first appears.
    idents: HashMap<String, usize>,
    /// The first atomic write by method call: its line and method.
    store: Option<(usize, String)>,
}

fn is_write(method: &str) -> bool {
    matches!(
        method,
        "store" | "swap" | "compare_exchange" | "compare_exchange_weak"
    ) || method
        .strip_prefix("fetch_")
        .is_some_and(|op| !op.is_empty() && op.bytes().all(|b| b.is_ascii_lowercase()))
}

impl Scan {
    fn walk(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(g) => self.walk(g.stream()),
                TokenTree::Ident(ident) => {
                    let name = ident.to_string();
                    let line = ident.span().start().line;
                    if SPACE.contains(&name.as_str())
                        || CURSOR.contains(&name.as_str())
                        || name == WORD_ACCESS
                    {
                        self.idents.entry(name.clone()).or_insert(line);
                    }
                    let after_dot = i > 0
                        && matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
                    let called = matches!(tokens.get(i + 1),
                        Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis);
                    if after_dot && called && is_write(&name) && self.store.is_none() {
                        self.store = Some((line, name));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::Tree;

    const KERNEL_MAPPINGS: &str = "\
use slopos_kernel_services::kernel_vm_space::kernel_vm_space;
pub fn map_kernel_page() {
    let mut guard = kernel_vm_space().lock();
    let mut cursor = guard.cursor_mut(range).unwrap();
}
";

    fn fixture() -> Tree {
        Tree::new(&[
            (WRITER, KERNEL_MAPPINGS),
            (
                "kernel-services/src/kernel_vm_space.rs",
                "pub static KERNEL_VM_SPACE: OnceLock<SpinLock<VmSpace>> = OnceLock::new();
pub fn kernel_vm_space() -> &'static SpinLock<VmSpace> { KERNEL_VM_SPACE.get().unwrap() }
",
            ),
            (
                "slopos-ostd/src/mm/vm_space.rs",
                "fn ostd_owns_the_tables() { KERNEL_VM_SPACE; kernel_vm_space(); let c: CursorMut; }\n",
            ),
            (
                "drivers/src/second_writer.rs",
                "fn remap() {
    let mut guard = slopos_kernel_services::kernel_vm_space::kernel_vm_space().lock();
    let cursor = guard.cursor_mut(range);
}
",
            ),
            (
                "net/src/second_handle.rs",
                "fn peek() -> bool { slopos_kernel_services::kernel_vm_space::KERNEL_VM_SPACE.get().is_some() }\n",
            ),
            (
                "boot/src/boot_memory.rs",
                "fn install() { KERNEL_VM_SPACE.call_once(|| SpinLock::new(space)); }
fn remap() { KERNEL_VM_SPACE.get().unwrap().lock().cursor_mut(range); }
",
            ),
            (
                "mm/src/paging/table_store.rs",
                "fn set(phys: u64, i: usize, v: u64) {
    with_atomic_u64_in_page(base(phys), i, |slot| slot.store(v, Ordering::Release));
}
",
            ),
            (
                "mm/src/paging/table_load.rs",
                "fn get(phys: u64, i: usize) -> u64 {
    with_atomic_u64_in_page(base(phys), i, |slot| slot.load(Ordering::Acquire))
}
",
            ),
            (
                "core/src/reader.rs",
                "fn read_side() {
    let guard = kernel_vm_space().lock();
    let cur = guard.cursor(range);
    kernel_vm_space().lock().activate_kernel_master();
}
",
            ),
            (
                "boot/src/commented.rs",
                "// Every kernel_vm_space() write goes through CursorMut in kernel_mappings;
// KERNEL_VM_SPACE is installed by boot_memory.
fn nothing() {}
",
            ),
        ])
    }

    fn hits(report: &Report, kind: &str) -> Vec<String> {
        report
            .findings
            .iter()
            .filter(|f| f.message.contains(kind))
            .map(|f| f.location.clone())
            .collect()
    }

    const WRITER_HIT: &str = "second writer";
    const STATIC_HIT: &str = "outside its definition";
    const STORE_HIT: &str = "only load here";
    const FLOOR_HIT: &str = "moved or was renamed";

    #[test]
    fn second_writers_handles_and_stores_fail() {
        let tree = fixture();
        let report = run(&tree.repo).unwrap();
        assert_eq!(
            hits(&report, WRITER_HIT),
            [
                "boot/src/boot_memory.rs:2",
                "drivers/src/second_writer.rs:3"
            ]
        );
        assert_eq!(hits(&report, STORE_HIT), ["mm/src/paging/table_store.rs:2"]);
        assert_eq!(hits(&report, STATIC_HIT), ["net/src/second_handle.rs:1"]);
        assert!(hits(&report, FLOOR_HIT).is_empty());
        assert_eq!(report.findings.len(), 4, "{:?}", report.findings);
    }

    #[test]
    fn a_writer_that_moved_trips_the_floor() {
        let tree = fixture();
        tree.write(WRITER, "fn moved() {}\n");
        let report = run(&tree.repo).unwrap();
        assert_eq!(hits(&report, FLOOR_HIT), [WRITER]);
    }

    #[test]
    fn comments_strings_and_doc_comments_never_count() {
        let tree = fixture();
        tree.write(
            "drivers/src/prose.rs",
            "/// Takes `kernel_vm_space()` and `cursor_mut`, as KERNEL_VM_SPACE's
/* a block comment naming kernel_vm_space and CursorMut,
   /* nested */ and KERNEL_VM_SPACE */
fn f() -> &'static str { \"KERNEL_VM_SPACE kernel_vm_space cursor_mut\" }
fn g() { let c = '}'; let u = \"http://kernel_vm_space\"; }
",
        );
        tree.write(
            "mm/src/paging/prose_store.rs",
            "fn get() -> u64 {
    // with_atomic_u64_in_page(.., |s| s.store(v, ..)) is not ours to do
    with_atomic_u64_in_page(base, 0, |slot| slot.load(Ordering::Acquire))
}
fn swap_names(a: &mut X) { let s = \"slot.swap(1)\"; }
",
        );
        let report = run(&tree.repo).unwrap();
        assert_eq!(report.findings.len(), 4, "{:?}", report.findings);
    }

    #[test]
    fn macro_bodies_and_split_lines_count() {
        let tree = fixture();
        tree.write(
            "sched/src/via_macro.rs",
            "fn f() {
    with_space!(slopos_kernel_services::kernel_vm_space::
        KERNEL_VM_SPACE, |s| s.cursor_mut(r));
}
",
        );
        tree.write(
            "mm/src/paging/fetch.rs",
            "fn bump() {
    with_atomic_u64_in_page(base, 0, |slot| {
        slot
            .fetch_or(1, Ordering::AcqRel)
    });
}
",
        );
        let report = run(&tree.repo).unwrap();
        assert_eq!(hits(&report, STATIC_HIT).len(), 2);
        assert!(hits(&report, STATIC_HIT).contains(&"sched/src/via_macro.rs:3".to_string()));
        assert!(hits(&report, WRITER_HIT).contains(&"sched/src/via_macro.rs:3".to_string()));
        assert!(hits(&report, STORE_HIT).contains(&"mm/src/paging/fetch.rs:4".to_string()));
    }

    #[test]
    fn host_tools_are_not_scanned() {
        let tree = fixture();
        tree.write(
            "tools/xtask/src/lint/gate.rs",
            "fn f() { KERNEL_VM_SPACE.get(); kernel_vm_space().lock().cursor_mut(r);
    with_atomic_u64_in_page(b, 0, |s| s.store(1, Ordering::Release)); }\n",
        );
        let report = run(&tree.repo).unwrap();
        assert!(
            report
                .findings
                .iter()
                .all(|f| !f.location.starts_with("tools/")),
            "{:?}",
            report.findings
        );
        assert_eq!(report.findings.len(), 4, "{:?}", report.findings);
    }

    #[test]
    fn atomic_writes_are_recognised_by_name() {
        for m in [
            "store",
            "swap",
            "compare_exchange",
            "compare_exchange_weak",
            "fetch_add",
            "fetch_update",
        ] {
            assert!(is_write(m), "{m}");
        }
        for m in ["load", "fetch_", "fetch_Add", "into_inner"] {
            assert!(!is_write(m), "{m}");
        }
    }
}
