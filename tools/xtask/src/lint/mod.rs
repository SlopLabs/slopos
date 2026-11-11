//! Source gates: each holds the working tree to one of the kernel's
//! disciplines, matching syntax trees rather than lines.

use crate::Result;
use crate::repo::Repo;
use crate::report::{self, Report};

mod agents_doc;
mod alloc_dep;
mod app_deps;
mod charge_linearity;
mod drop_panic_free;
mod frame_ownership;
mod kernel_pml4_writer;
mod no_kernel_async;
mod process_designator;
mod syscall_abi;
mod task_ownership;
mod tcb_ratio;
mod ui_literals;
mod unsafe_expansion;
mod unsafe_outside_ostd;
mod wait_api;
mod wait_predicate_purity;
mod wait_result_handling;

/// Named TCB annexes; every other vendor crate is scanned.
const TCB_ANNEXES: &[&str] = &["vendor/unwinding", "vendor/gimli"];

pub struct Gate {
    pub name: &'static str,
    pub run: fn(&Repo) -> Result<Report>,
}

pub const GATES: &[Gate] = &[
    Gate {
        name: "unsafe-outside-ostd",
        run: unsafe_outside_ostd::run,
    },
    Gate {
        name: "unsafe-expansion",
        run: unsafe_expansion::run,
    },
    Gate {
        name: "alloc-dep",
        run: alloc_dep::run,
    },
    Gate {
        name: "no-kernel-async",
        run: no_kernel_async::run,
    },
    Gate {
        name: "drop-panic-free",
        run: drop_panic_free::run,
    },
    Gate {
        name: "kernel-pml4-writer",
        run: kernel_pml4_writer::run,
    },
    Gate {
        name: "tcb-ratio",
        run: tcb_ratio::run,
    },
    Gate {
        name: "wait-predicate-purity",
        run: wait_predicate_purity::run,
    },
    Gate {
        name: "wait-result-handling",
        run: wait_result_handling::run,
    },
    Gate {
        name: "task-ownership",
        run: task_ownership::run,
    },
    Gate {
        name: "frame-ownership",
        run: frame_ownership::run,
    },
    Gate {
        name: "process-designator",
        run: process_designator::run,
    },
    Gate {
        name: "charge-linearity",
        run: charge_linearity::run,
    },
    Gate {
        name: "ui-literals",
        run: ui_literals::run,
    },
    Gate {
        name: "syscall-abi",
        run: syscall_abi::run,
    },
    Gate {
        name: "app-deps",
        run: app_deps::run,
    },
    Gate {
        name: "agents-doc",
        run: agents_doc::run,
    },
];

/// Run the named gates (every gate when none is named), print each report,
/// and say whether all passed. A gate that cannot run is an error, reported
/// after the others have had their say.
pub fn main(repo: &Repo, args: &[String]) -> Result<bool> {
    let gates: Vec<&Gate> = if args.is_empty() {
        GATES.iter().collect()
    } else {
        args.iter()
            .map(|name| {
                GATES
                    .iter()
                    .find(|g| g.name == name)
                    .ok_or_else(|| format!("no lint gate named `{name}` (see `xtask list`)"))
            })
            .collect::<Result<_, _>>()?
    };
    let mut results = Vec::new();
    let mut broken = Vec::new();
    for gate in gates {
        match (gate.run)(repo) {
            Ok(r) => results.push((gate.name, r)),
            Err(e) => broken.push(format!("{}: {e}", gate.name)),
        }
    }
    let passed = report::print_all(&results);
    if !broken.is_empty() {
        return Err(broken.join("\n").into());
    }
    Ok(passed)
}
