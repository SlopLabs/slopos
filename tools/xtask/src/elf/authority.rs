//! No syscall handler may reach a terminal power primitive unless it is
//! accounted for.
//!
//! The dispatch table's classification is a `rustc` error already, but it
//! covers slots, not reachability: `roulette_result` was classified, the table
//! gate was green, and its loss arm called `kernel_reboot` two calls away from
//! an unprivileged caller. So this walks the call graph of the linked ELF,
//! where inlining, generic instantiation and dispatch have already decided who
//! really calls whom, from every `define_syscall!` handler to
//! `slopos_ostd::platform::power::{shutdown,reboot}`, the choke point every
//! caller funnels through. Each handler that reaches one must be listed in
//! `scripts/gates/authority/<variant>.txt`, which records why: it is
//! classified `Power` at its `define_syscall!` site, or its path is gated by
//! something else, named there. An entry that matches nothing fails the gate,
//! since a dead exemption would let the real path grow back under its cover.
//!
//! Edges are direct branch targets: `call`, and `jmp`/`jcc` into another
//! function, which is a tail call. The seam this does not close is indirect
//! control flow: a `call` through a function pointer or a vtable names no
//! callee, so a path through one is invisible here. That is why the
//! kernel-initiated callers, which reach power through the `PowerOps`
//! indirection deliberately, are a short reviewed list rather than something
//! this discovers; this gate keeps the syscall side from growing silently.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use iced_x86::{FlowControl, Instruction, OpKind};

use super::{Ctx, Region, demangle, regions};
use crate::Result;
use crate::report::Report;

const SINKS: &[&str] = &[
    "slopos_ostd8platform5power8shutdown",
    "slopos_ostd8platform5power6reboot",
];

fn is_sink(name: &str) -> bool {
    SINKS.iter().any(|s| name.contains(s))
}

/// `slopos_core` … `7syscall` … `syscall_<name>`: the module path
/// `define_syscall!` places every handler under.
fn is_handler(name: &str) -> bool {
    let Some(core) = name.find("slopos_core") else {
        return false;
    };
    let rest = &name[core + "slopos_core".len()..];
    let Some(module) = rest.find("7syscall") else {
        return false;
    };
    let rest = &rest[module + "7syscall".len()..];
    rest.match_indices("syscall_")
        .any(|(i, m)| rest[i + m.len()..].bytes().next().is_some_and(handler_byte))
}

fn handler_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

/// The bare `syscall_*` name an allowlist keys on: the last `syscall_` that
/// follows a length prefix, up to the first byte outside `[a-z0-9_]`. A
/// closure body `..14syscall_reboot0B7_` keys as `syscall_reboot0`.
fn handler_name(name: &str) -> &str {
    let bytes = name.as_bytes();
    name.match_indices("syscall_")
        .filter(|&(i, m)| {
            i > 0
                && bytes[i - 1].is_ascii_digit()
                && bytes.get(i + m.len()).is_some_and(|&b| handler_byte(b))
        })
        .last()
        .map_or(name, |(i, _)| {
            let len = bytes[i..].iter().take_while(|&&b| handler_byte(b)).count();
            &name[i..i + len]
        })
}

struct Graph<'a> {
    regions: Vec<Region<'a>>,
    /// For each region, the regions that branch into it directly.
    callers: Vec<Vec<u32>>,
    edges: usize,
}

fn direct_target(instr: &Instruction) -> Option<u64> {
    matches!(
        instr.flow_control(),
        FlowControl::Call | FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
    )
    .then_some(())?;
    matches!(
        instr.op0_kind(),
        OpKind::NearBranch64 | OpKind::NearBranch32 | OpKind::NearBranch16
    )
    .then(|| instr.near_branch_target())
}

fn graph<'a>(ctx: &Ctx<'a>) -> Result<Graph<'a>> {
    let regions = regions(ctx.image, None)?;
    let starts: Vec<u64> = regions.iter().map(|r| r.address).collect();
    let region_of = |addr: u64| -> Option<usize> {
        let i = starts.partition_point(|&s| s <= addr).checked_sub(1)?;
        let r = &regions[i];
        (addr < r.address + r.bytes.len() as u64).then_some(i)
    };
    let mut callers: Vec<Vec<u32>> = vec![Vec::new(); regions.len()];
    let mut instr = Instruction::default();
    for (from, region) in regions.iter().enumerate() {
        let mut decoder = region.decoder();
        while decoder.can_decode() {
            decoder.decode_out(&mut instr);
            if let Some(to) = direct_target(&instr).and_then(region_of)
                && to != from
            {
                callers[to].push(u32::try_from(from)?);
            }
        }
    }
    let mut edges = 0;
    for c in &mut callers {
        c.sort_unstable();
        c.dedup();
        edges += c.len();
    }
    Ok(Graph {
        regions,
        callers,
        edges,
    })
}

struct Reach {
    /// Handler name, with one witness path from it to a primitive.
    found: BTreeMap<String, Vec<usize>>,
    handlers: usize,
    sinks: usize,
}

fn reach(g: &Graph) -> Reach {
    let n = g.regions.len();
    let mut toward: Vec<Option<usize>> = vec![None; n];
    let mut hit = vec![false; n];
    let mut queue = VecDeque::new();
    for (i, r) in g.regions.iter().enumerate() {
        if r.names.iter().any(|s| is_sink(s)) {
            hit[i] = true;
            queue.push_back(i);
        }
    }
    let sinks = queue.len();
    while let Some(node) = queue.pop_front() {
        for &p in &g.callers[node] {
            let p = p as usize;
            if !hit[p] {
                hit[p] = true;
                toward[p] = Some(node);
                queue.push_back(p);
            }
        }
    }
    let mut found = BTreeMap::new();
    let mut handlers = 0;
    for (i, r) in g.regions.iter().enumerate() {
        let names: Vec<&str> = r.names.iter().copied().filter(|n| is_handler(n)).collect();
        if names.is_empty() {
            continue;
        }
        handlers += 1;
        if !hit[i] {
            continue;
        }
        let mut path = vec![i];
        while let Some(next) = toward[*path.last().unwrap_or(&i)] {
            path.push(next);
        }
        for name in names {
            found
                .entry(handler_name(name).to_string())
                .or_insert_with(|| path.clone());
        }
    }
    Reach {
        found,
        handlers,
        sinks,
    }
}

fn allowlist_rel(variant: &str) -> String {
    format!("scripts/gates/authority/{variant}.txt")
}

fn walk<'a>(ctx: &Ctx<'a>) -> Result<(Graph<'a>, Reach)> {
    let g = graph(ctx)?;
    let r = reach(&g);
    if r.found.is_empty() {
        return Err(format!(
            "no handler in {} reaches a power primitive ({} handler(s), {} primitive(s) found), \
             which cannot be right: `halt` calls one directly; the symbol patterns have rotted \
             or the image carries no code",
            ctx.path.display(),
            r.handlers,
            r.sinks
        )
        .into());
    }
    Ok((g, r))
}

pub fn run(ctx: &Ctx) -> Result<Report> {
    let rel = allowlist_rel(ctx.variant);
    if !ctx.repo.exists(&rel) {
        return Err(format!(
            "no allowlist at {rel}; generate one with `--emit-allowlist authority` and review \
             every entry"
        )
        .into());
    }
    let text = ctx.repo.read(&rel)?;
    let (g, r) = walk(ctx)?;

    let mut allowed: BTreeMap<&str, usize> = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.split_whitespace().next() {
            allowed.entry(name).or_insert(i + 1);
        }
    }

    let mut report = Report::new();
    for (name, path) in &r.found {
        if allowed.contains_key(name.as_str()) {
            continue;
        }
        let via: Vec<String> = path
            .iter()
            .map(|&i| demangle(g.regions[i].name()))
            .collect();
        report.finding(
            name.as_str(),
            format!(
                "reaches a power primitive but is not accounted for (via {}) — classify the \
                 handler `Power`, or, if the path is genuinely gated by something else (as \
                 roulette_result's reboot arm is by BOOT_FLAG_FATE_REBOOT), add it to {rel} with \
                 that gate named in a comment",
                via.join(" → ")
            ),
        );
    }
    for (name, line) in &allowed {
        if !r.found.contains_key(*name) {
            report.at(
                &rel,
                *line,
                format!(
                    "`{name}` matches no handler that reaches a power primitive — remove it; a \
                     dead exemption hides the path growing back"
                ),
            );
        }
    }
    report.summary(format!(
        "variant={}, {} handler(s) reach a power primitive, all accounted for; walked {} direct \
         edges over {} functions from {} handlers to {} primitive(s)",
        ctx.variant,
        r.found.len(),
        g.edges,
        g.regions.len(),
        r.handlers,
        r.sinks
    ));
    Ok(report)
}

/// The handlers that reach a power primitive, one per line under a header,
/// for `scripts/gates/authority/<variant>.txt`. Each still needs its reason
/// written above it before it is committed.
pub fn emit_allowlist(ctx: &Ctx) -> Result<String> {
    let (_, r) = walk(ctx)?;
    let v = ctx.variant;
    let mut out = format!(
        "# authority allowlist — variant: {v}\n#\n# Syscall handlers that can reach a terminal \
         power primitive.\n# Each must be classified Power, or carry a stated reason here.\n#\n\
         #     xtask elf --variant {v} builddir/kernel-{v}.elf --emit-allowlist authority\n\n"
    );
    let names: BTreeSet<&String> = r.found.keys().collect();
    for name in names {
        out.push_str(name);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::Image;
    use super::super::fixture::{Elf, call, ctx, jmp, with_image};
    use super::*;
    use crate::fixture::Tree;

    const REBOOT: &str = "_RNvNtNtCs1_11slopos_ostd8platform5power6reboot";
    const SHUTDOWN: &str = "_RNvNtNtCs1_11slopos_ostd8platform5power8shutdown";
    const H_REBOOT: &str = "_RNvNtNtCs1_11slopos_core7syscall13core_handlers14syscall_reboot";
    const H_ROULETTE: &str =
        "_RNvNtNtCs1_11slopos_core7syscall11ui_handlers23syscall_roulette_result";
    const FATE: &str = "_RNvNtNtCs1_11slopos_core7syscall11ui_handlers11fate_unwind";
    const H_YIELD: &str = "_RNvNtNtCs1_11slopos_core7syscall13core_handlers13syscall_yield";
    const SCHEDULE: &str = "_RNvNtCs1_12slopos_sched9scheduler8schedule";

    /// The script's self-test graph, with the two-hop path ending in a tail
    /// `jmp` rather than a `call`: reboot ← syscall_reboot (call);
    /// reboot ← fate_unwind (jmp) ← syscall_roulette_result (call);
    /// schedule ← syscall_yield.
    fn probe() -> Vec<u8> {
        let mut e = Elf::new();
        let base = e.next();
        let (reboot, schedule, fate) = (base, base + 16, base + 32);
        e.function(REBOOT, &[0xc3]);
        e.function(SCHEDULE, &[0xc3]);
        let at = e.next();
        e.function(FATE, &jmp(at, reboot));
        let at = e.next();
        e.function(H_REBOOT, &[call(at, reboot), vec![0xc3]].concat());
        let at = e.next();
        e.function(H_ROULETTE, &[call(at, fate), vec![0xc3]].concat());
        let at = e.next();
        e.function(H_YIELD, &[call(at, schedule), vec![0xc3]].concat());
        assert_eq!(fate, e.next() - 64);
        e.finish()
    }

    fn found(elf: &[u8]) -> Vec<String> {
        let tree = Tree::new(&[]);
        with_image(elf, |image| {
            let g = graph(&ctx(&tree.repo, "t", image)).unwrap();
            reach(&g).found.into_keys().collect()
        })
    }

    fn gate(elf: &[u8], allow: &str) -> Result<Report> {
        let tree = Tree::new(&[("scripts/gates/authority/t.txt", allow)]);
        with_image(elf, |image: &Image| run(&ctx(&tree.repo, "t", image)))
    }

    #[test]
    fn a_direct_call_and_a_two_hop_tail_jump_are_found_and_an_unrelated_handler_is_not() {
        assert_eq!(
            found(&probe()),
            ["syscall_reboot", "syscall_roulette_result"]
        );
    }

    #[test]
    fn a_conditional_tail_jump_is_an_edge() {
        let mut e = Elf::new();
        let shutdown = e.next();
        e.function(SHUTDOWN, &[0xc3]);
        let at = e.next();
        let mut jne = vec![0x0f, 0x85];
        jne.extend_from_slice(&((shutdown as i64 - (at as i64 + 6)) as i32).to_le_bytes());
        e.function(H_YIELD, &[jne, vec![0xc3]].concat());
        assert_eq!(found(&e.finish()), ["syscall_yield"]);
    }

    #[test]
    fn a_branch_within_a_function_is_no_edge() {
        let mut e = Elf::new();
        e.function(REBOOT, &[0xc3]);
        e.function(H_YIELD, &[0xeb, 0x00, 0xc3]);
        let err = gate(&e.finish(), "syscall_yield\n")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no handler in kernel.elf reaches a power primitive"),
            "{err}"
        );
    }

    #[test]
    fn an_accounted_handler_set_passes() {
        let report = gate(
            &probe(),
            "# reasons\nsyscall_reboot\n  syscall_roulette_result  # gated by the boot flag\n",
        )
        .unwrap();
        assert!(report.passed(), "{}", report.render("authority"));
        assert!(
            report
                .summary
                .contains("2 handler(s) reach a power primitive")
        );
        assert!(report.summary.contains("from 3 handlers to 1 primitive(s)"));
    }

    #[test]
    fn an_unaccounted_handler_fails_with_its_path() {
        let report = gate(&probe(), "syscall_reboot\n").unwrap();
        assert_eq!(report.findings.len(), 1);
        let f = &report.findings[0];
        assert_eq!(f.location, "syscall_roulette_result");
        assert!(
            f.message.contains(
                "via slopos_core::syscall::ui_handlers::syscall_roulette_result → \
                 slopos_core::syscall::ui_handlers::fate_unwind → \
                 slopos_ostd::platform::power::reboot"
            ),
            "{}",
            f.message
        );
    }

    #[test]
    fn a_dead_entry_fails() {
        let report = gate(
            &probe(),
            "syscall_reboot\nsyscall_roulette_result\n\nsyscall_yield\n",
        )
        .unwrap();
        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].location,
            "scripts/gates/authority/t.txt:4"
        );
        assert!(report.findings[0].message.contains("`syscall_yield`"));
    }

    #[test]
    fn a_missing_allowlist_is_an_error() {
        let tree = Tree::new(&[]);
        let elf = probe();
        let err = with_image(&elf, |image| run(&ctx(&tree.repo, "t", image)))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no allowlist at scripts/gates/authority/t.txt"),
            "{err}"
        );
    }

    #[test]
    fn handler_names_follow_define_syscall() {
        assert!(is_handler(H_REBOOT));
        assert!(!is_handler(FATE));
        assert!(!is_handler(
            "_RNvNtCs1_11slopos_sched7syscall14syscall_reboot"
        ));
        assert!(!is_handler("_RNvNtCs1_11slopos_core7syscall8syscall_"));
        assert_eq!(handler_name(H_REBOOT), "syscall_reboot");
        assert_eq!(
            handler_name("_RNCNvNtNtCs1_11slopos_core7syscall13core_handlers14syscall_reboot0B7_"),
            "syscall_reboot0"
        );
    }

    #[test]
    fn emitted_allowlist_round_trips_through_the_gate() {
        let elf = probe();
        let tree = Tree::new(&[]);
        let emitted =
            with_image(&elf, |image| emit_allowlist(&ctx(&tree.repo, "t", image))).unwrap();
        assert!(
            emitted.ends_with("\nsyscall_reboot\nsyscall_roulette_result\n"),
            "{emitted}"
        );
        let report = gate(&elf, &emitted).unwrap();
        assert!(report.passed(), "{}", report.render("authority"));
    }
}
