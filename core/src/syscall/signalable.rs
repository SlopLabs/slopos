//! `Signalable` — a signal target, resolved once and authorized in the same
//! step.
//!
//! A bare witness proves only that *a* check ran, not that it named this
//! target: a call that checked one capability and then acted on any id would
//! look the same holding a `&Cap<ProcSignal>`. So the witness carries the
//! object — acting is a method on the target, and there is no other way to
//! name one.

use slopos_abi::Errno;
use slopos_abi::task::INVALID_TASK_ID;
use slopos_sched::task::{TaskRef, task_find_by_id};

use crate::syscall::signal::{Sender, signal_may_name, signal_permitted};

/// A live task the caller has been authorized to signal.
///
/// Owns a `TaskRef` rather than an id: the target cannot exit and be recycled
/// onto a stranger between the check and the act.
#[must_use = "a resolved signal target that is never acted on is a discarded \
              authorization; drop it explicitly if that is intended"]
pub struct Signalable {
    target: TaskRef,
    id: u32,
}

impl Signalable {
    /// The target's id, for reporting. Never a re-lookup key — the reference
    /// is what names the task.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The target itself.
    #[inline]
    pub fn task(&self) -> &TaskRef {
        &self.target
    }
}

/// Resolve `target_id` and authorize `sender` to send it `signum`, in one
/// step.
///
/// The two cannot be separated by construction, which is the point: there is
/// no `resolve` that skips the check and no `check` that takes an id.
///
/// - `ESRCH` — no such task, or a kernel task. Kernel tasks are excluded from
///   delivery, so naming one could only reach the `SIGKILL` arm and tear down a
///   driver thread holding device state and an interrupt line.
/// - `EPERM` — [`signal_permitted`] refuses.
pub(crate) fn resolve_signal_target(
    sender: &Sender,
    target_id: u32,
    signum: u64,
) -> Result<Signalable, Errno> {
    if target_id == INVALID_TASK_ID || target_id == 0 {
        return Err(Errno::ESRCH);
    }
    let target = task_find_by_id(target_id).ok_or(Errno::ESRCH)?;
    // A kernel task is not a process: `ESRCH` rather than `EPERM`, so the
    // refusal does not disclose that the id names something.
    if !signal_may_name(target.flags) {
        return Err(Errno::ESRCH);
    }
    if !signal_permitted(sender, &target, signum) {
        return Err(Errno::EPERM);
    }
    Ok(Signalable {
        target,
        id: target_id,
    })
}
