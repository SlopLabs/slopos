use super::task::{Task, task_find_by_id};
use slopos_abi::fate::FateResult;
use slopos_kernel_services::platform;
use slopos_ostd::wl_currency::{self, WL_DELTA};

/// Run `f` against a live task, `false` if the id no longer resolves.
///
/// A shared borrow: the syscall names an arbitrary id, so the target is very
/// often a concurrently running task. The fate trio is atomic for that reason.
fn with_task<F, R>(task_id: u32, f: F) -> bool
where
    F: FnOnce(&Task) -> R,
{
    let Some(task) = task_find_by_id(task_id) else {
        return false;
    };
    f(&task);
    true
}
pub fn fate_spin() -> FateResult {
    let val = platform::rng_next() as u32;
    FateResult {
        token: val,
        value: val,
    }
}
pub fn fate_set_pending(res: FateResult, task_id: u32) -> bool {
    with_task(task_id, |t| t.set_fate(res.token, res.value))
}
pub fn fate_take_pending(task_id: u32) -> Option<FateResult> {
    let mut taken = None;
    let _ = with_task(task_id, |t| {
        taken = t
            .take_fate()
            .map(|(token, value)| FateResult { token, value });
    });
    taken
}
pub fn fate_apply_outcome(award: bool) {
    if award {
        wl_currency::adjust_balance(WL_DELTA);
    } else {
        wl_currency::adjust_balance(-WL_DELTA);
    }
}
