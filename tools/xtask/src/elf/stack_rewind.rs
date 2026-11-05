//! No Rust function may store to `BSP_BOOTSTRAP_TASK`, the BSP's SafeStack
//! bootstrap data-stack pointer.
//!
//! The BSP runs the whole boot call chain, `kernel_main` through
//! `boot_init_run_phase` and every init step, on `BOOTSTRAP_UNSAFE_STACK`. Its
//! allocation pointer is seeded once by `boot/limine_entry.s` and thereafter
//! moved only by instrumented prologues and epilogues: it is a live stack
//! pointer, not a configuration value. Storing the buffer top back into it
//! hands every live caller frame back to the allocator, and the frames opened
//! afterwards are laid over them (`init_bootstrap_tasks`, called from
//! `smp_init` while `boot_init_run_phase` holds its `ordered[64]` array below
//! the top, must never re-seed the slot).
//!
//! Only the shipped ELF shows it: in the dev and tests builds
//! `init_bootstrap_tasks` is itself instrumented, so its own epilogue restores
//! the slot and the store is invisible, while in release it has no such frame
//! and the store persists. So any `mov` whose destination is the slot,
//! addressed rip-relative, in any named function fails the gate. The boot
//! trampoline seeds it through a register base and holds no Rust symbol.

use iced_x86::{Instruction, OpKind, Register};
use object::{Object, ObjectSymbol};

use super::{Ctx, regions};
use crate::Result;
use crate::report::Report;

const SLOT: &str = "BSP_BOOTSTRAP_TASK";
const STACK: &str = "BOOTSTRAP_UNSAFE_STACK";

/// Names permitted to store to the slot. Empty by design: `limine_entry.s` is
/// the only writer and carries no Rust symbol.
const ALLOWED_WRITERS: &[&str] = &[];

fn defined(ctx: &Ctx, name: &str) -> Option<u64> {
    ctx.image
        .file
        .symbols()
        .find(|s| !s.is_undefined() && s.name().is_ok_and(|n| n == name))
        .map(|s| s.address())
}

fn stores_to(instr: &Instruction, addr: u64) -> bool {
    instr.op_count() > 0
        && instr.op0_kind() == OpKind::Memory
        && instr.memory_base() == Register::RIP
        && instr.ip_rel_memory_address() == addr
        && format!("{:?}", instr.mnemonic())
            .to_ascii_lowercase()
            .starts_with("mov")
}

pub fn run(ctx: &Ctx) -> Result<Report> {
    let mut report = Report::new();
    let elf = ctx.path.display().to_string();
    let Some(slot) = defined(ctx, SLOT) else {
        report.finding(
            &elf,
            format!("{SLOT} not found (input sanity) — without it the scan is vacuous; is this a SlopOS kernel image?"),
        );
        return Ok(report);
    };
    if defined(ctx, STACK).is_none() {
        report.finding(
            &elf,
            format!("{STACK} not found (input sanity) — without it the scan is vacuous; is this a SlopOS kernel image?"),
        );
        return Ok(report);
    }

    let regions = regions(ctx.image, None)?;
    let mut functions = 0;
    let mut instructions = 0u64;
    let mut instr = Instruction::default();
    for region in &regions {
        if region.names.is_empty() {
            continue;
        }
        functions += 1;
        let name = region.name();
        let allowed = ALLOWED_WRITERS.iter().any(|a| name.contains(a));
        let mut decoder = region.decoder();
        while decoder.can_decode() {
            decoder.decode_out(&mut instr);
            instructions += 1;
            if !allowed && stores_to(&instr, slot) {
                report.finding(
                    name,
                    format!(
                        "stores to {SLOT} ({slot:#x}) at {:#x} in {} — that slot is the BSP's \
                         live data-stack pointer and rewinding it frees every live caller frame; \
                         only boot/limine_entry.s may seed it",
                        instr.ip(),
                        super::demangle(name)
                    ),
                );
            }
        }
    }
    report.summary(format!(
        "variant={}, {SLOT} ({slot:#x}) has no Rust writer; scanned {instructions} instructions \
         in {functions} functions",
        ctx.variant
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::super::fixture::{Elf, ctx, with_image};
    use super::*;
    use crate::fixture::Tree;

    /// `.data` offset of the slot; the stack lies before it, as in link order.
    const AT: u64 = 0x2000;

    /// `prefix modrm-dest-is-[rip+disp32] ... suffix` aimed at `target`, with
    /// the instruction placed at `.text` offset `ip`.
    fn rip(ip: u64, prefix: &[u8], suffix: &[u8], target: u64) -> Vec<u8> {
        let len = (prefix.len() + 4 + suffix.len()) as i64;
        let disp = (target as i64 - (ip as i64 + len)) as i32;
        [prefix, &disp.to_le_bytes(), suffix].concat()
    }

    fn image(name: &str, code: impl Fn(u64) -> Vec<u8>, symbols: &[&str]) -> Vec<u8> {
        let mut e = Elf::new();
        if symbols.contains(&STACK) {
            e.object(STACK, 0x1000, 0x1000);
        }
        if symbols.contains(&SLOT) {
            e.object(SLOT, AT, 8);
        }
        e.object("OTHER", AT + 8, 8);
        e.function("main", &[0xc3]);
        let at = e.next();
        e.function(name, &[code(at), vec![0xc3]].concat());
        e.finish()
    }

    fn gate(elf: &[u8]) -> Report {
        let tree = Tree::new(&[]);
        with_image(elf, |image| run(&ctx(&tree.repo, "selftest", image))).unwrap()
    }

    const BOTH: &[&str] = &[SLOT, STACK];

    #[test]
    fn a_planted_writer_is_rejected() {
        // mov [rip+slot], rax
        let report = gate(&image(
            "init_bootstrap_tasks",
            |ip| rip(ip, &[0x48, 0x89, 0x05], &[], AT),
            BOTH,
        ));
        assert_eq!(
            report.findings.len(),
            1,
            "{}",
            report.render("stack-rewind")
        );
        assert_eq!(report.findings[0].location, "init_bootstrap_tasks");
        assert!(
            report.findings[0]
                .message
                .contains("stores to BSP_BOOTSTRAP_TASK")
        );
    }

    #[test]
    fn an_immediate_store_is_rejected() {
        // mov qword ptr [rip+slot], 0
        let report = gate(&image(
            "init_bootstrap_tasks",
            |ip| rip(ip, &[0x48, 0xc7, 0x05], &[0, 0, 0, 0], AT),
            BOTH,
        ));
        assert_eq!(
            report.findings.len(),
            1,
            "{}",
            report.render("stack-rewind")
        );
    }

    #[test]
    fn a_reader_is_accepted() {
        // mov rax, [rip+slot]
        let report = gate(&image(
            "read_only",
            |ip| rip(ip, &[0x48, 0x8b, 0x05], &[], AT),
            BOTH,
        ));
        assert!(report.passed(), "{}", report.render("stack-rewind"));
        assert!(report.summary.contains("has no Rust writer; scanned"));
    }

    #[test]
    fn an_address_taken_or_a_neighbour_written_is_accepted() {
        let lea = gate(&image(
            "lea_only",
            |ip| rip(ip, &[0x48, 0x8d, 0x05], &[], AT),
            BOTH,
        ));
        assert!(lea.passed(), "{}", lea.render("stack-rewind"));
        let neighbour = gate(&image(
            "other_writer",
            |ip| rip(ip, &[0x48, 0x89, 0x05], &[], AT + 8),
            BOTH,
        ));
        assert!(neighbour.passed(), "{}", neighbour.render("stack-rewind"));
    }

    #[test]
    fn missing_symbols_fail_as_input_sanity() {
        for (present, missing) in [(&[STACK][..], SLOT), (&[SLOT][..], STACK)] {
            let report = gate(&image("main2", |_| vec![0x90], present));
            assert_eq!(report.findings.len(), 1);
            assert!(
                report.findings[0]
                    .message
                    .starts_with(&format!("{missing} not found (input sanity)"))
            );
        }
    }
}
