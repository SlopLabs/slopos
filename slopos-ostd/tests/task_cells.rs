//! Coverage for the witness-gated task cells, from outside the crate.
//!
//! The aliasing proofs live in `slopos-ostd/src/task/cell.rs`, because they
//! must call the production `pub(crate) TaskOwnCell::get_ptr`. What this file
//! covers is the surface `sched`/`core` can reach: the witness-taking methods
//! on `TaskInner`, the only way a `#![forbid(unsafe_code)]` crate writes
//! register-adjacent state after publication.
//!
//! `SwitchWindow` is the witness a host test can mint; `CurrentTask::get()` is
//! always `None` here because the PCR read short-circuits on `GS_BASE_SET`.

use slopos_ostd::KArc;
use slopos_ostd::task::HostStack;
use slopos_ostd::task::kernel_task::TaskInner;
use slopos_ostd::task::{CurrentTask, SwitchWindow};

type HostTask = TaskInner<(), ()>;

fn fresh() -> KArc<HostTask> {
    KArc::try_new(HostTask::invalid()).expect("task allocation")
}

/// # Safety
/// Single-threaded host test: this "CPU" performs the switch, holds the only
/// reference to `task`, and the window cannot be re-entered.
fn window(task: &HostTask) -> SwitchWindow<'_, (), ()> {
    unsafe { SwitchWindow::new(task) }
}

/// Why a host test mints a `SwitchWindow` rather than a `CurrentTask`, and why
/// the dispatcher needs the former: it covers the outgoing task, no longer the
/// CPU's current by the time its registers are saved.
#[test]
fn current_task_is_none_without_a_pcr() {
    assert!(CurrentTask::<HostStack, HostStack>::get().is_none());
}

/// A witness is a safety argument, so one naming the wrong task is unsound
/// rather than merely wrong: the owner check must be loud, not silent.
#[test]
#[should_panic(expected = "witness names a different task")]
fn a_witness_for_another_task_is_refused() {
    let first = fresh();
    let second = fresh();
    let w = window(&first);
    // The owner check is a debug assertion; in release it compiles out and the
    // type-level sealing of `TaskExclusive` is what remains.
    let _ = second.user_ctx(&w);
}
