//! W/L Currency System — The Wheel of Fate's Ledger
//!
//! Tracks wins and losses as the kernel gambles with destiny. The balance
//! reflects how well the system is treating userland: every successful
//! syscall earns a win, every failed syscall takes a loss.
//!
//! # Instrumentation Boundary
//!
//! W/L awards are applied at **system call boundaries** only:
//! - `SyscallContext::ok()`  → `award_win()`
//! - `SyscallContext::err()` → `award_loss()`
//!
//! The sole exception is `fate_api::fate_apply_outcome`, which directly
//! awards wins/losses as part of the Wheel of Fate roulette mechanic.
//!
//! Internal subsystems (heap allocator, filesystem internals, drivers,
//! boot sequences) must **not** call `award_win`/`award_loss` directly.
//! Doing so inflates the balance with internal noise — a single user
//! operation can trigger hundreds of allocations, and each one should
//! not independently move the needle.

use core::sync::atomic::{AtomicI64, Ordering};

static BALANCE: AtomicI64 = AtomicI64::new(0);

pub fn reset() {
    BALANCE.store(0, Ordering::Relaxed);
}

pub fn award_win() {
    BALANCE.fetch_add(10, Ordering::Relaxed);
}

pub fn award_loss() {
    BALANCE.fetch_sub(10, Ordering::Relaxed);
}

pub fn check_balance() -> i64 {
    BALANCE.load(Ordering::Relaxed)
}
