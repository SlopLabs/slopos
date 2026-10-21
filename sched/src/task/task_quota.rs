//! A task's charge: one task of its process's account, which the task holds
//! itself (`TaskInner::hold_charge`) and gives back at its exit's latch, as
//! `Task` destruction is deferred to the graveyard.

use slopos_abi::quota::TaskCount;
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Reservation, try_charge};

/// Reserve one task against `account`. Returned rather than recorded, so a
/// caller that fails later unwinds through the reservation's `Drop`.
pub fn reserve(account: AccountId) -> Option<Reservation<TaskCount>> {
    try_charge::<TaskCount>(account, 1).ok()
}
