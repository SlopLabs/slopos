//! The people's accounts: one row per person, a child of the root, which
//! every process of theirs debits through, so the tasks and processes one
//! person holds are counted against a ceiling of their own and never spend
//! another's, or the system's.
//!
//! Lock-free as the rest of the arena is: a row is claimed for its uid by one
//! compare-exchange and published once it is live.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use slopos_abi::quota::ResourceKind;

use super::arena::{NO_LIMIT, account_create, root, set_limit};
use crate::process::AccountId;
use crate::process::account::{FIRST_PERSON_SLOT, PERSON_ACCOUNTS, alloc_generation};

/// Each row's person, 0 for none: uid 0 is never a person's.
static PERSONS: [AtomicU32; PERSON_ACCOUNTS] = [const { AtomicU32::new(0) }; PERSON_ACCOUNTS];
/// Each row's id once it is live, 0 until then.
static ROWS: [AtomicU64; PERSON_ACCOUNTS] = [const { AtomicU64::new(0) }; PERSON_ACCOUNTS];

/// How long a spawn waits for another one making the same person's row.
const PUBLISH_SPINS: u32 = 1 << 20;

pub(super) fn is_person_slot(slot: u32) -> bool {
    (FIRST_PERSON_SLOT..FIRST_PERSON_SLOT + PERSON_ACCOUNTS as u32).contains(&slot)
}

/// The account `uid`'s processes debit through, made the first time; the
/// root once every row is a person's.
pub fn person_account(uid: u32) -> AccountId {
    if uid == 0 {
        return root();
    }
    for at in 0..PERSON_ACCOUNTS {
        let claimed =
            match PERSONS[at].compare_exchange(0, uid, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return publish(at),
                Err(holder) => holder,
            };
        if claimed == uid {
            return published(at);
        }
    }
    root()
}

/// The row `uid`'s processes debit through, if they have one yet.
pub fn person_row(uid: u32) -> Option<AccountId> {
    (0..PERSON_ACCOUNTS)
        .find(|&at| uid != 0 && PERSONS[at].load(Ordering::Acquire) == uid)
        .map(published)
        .filter(|&row| row != root())
}

fn publish(at: usize) -> AccountId {
    let id = AccountId::from_parts(FIRST_PERSON_SLOT + at as u32, alloc_generation());
    let id = match account_create(id, root()) {
        Ok(()) => {
            for kind in ResourceKind::ALL {
                set_limit(id, kind, person_limit(kind));
            }
            id
        }
        Err(_) => root(),
    };
    ROWS[at].store(id.raw(), Ordering::Release);
    id
}

fn published(at: usize) -> AccountId {
    for _ in 0..PUBLISH_SPINS {
        match ROWS[at].load(Ordering::Acquire) {
            0 => core::hint::spin_loop(),
            raw => return AccountId::from_raw(raw),
        }
    }
    root()
}

/// A person's share of what the machine has: a quarter of its processes and
/// threads, the share one process's spawn tree has always had, so a fork bomb
/// stops there with the other three quarters free. Every other kind stays a
/// process's own.
fn person_limit(kind: ResourceKind) -> u32 {
    match kind {
        ResourceKind::Process => (slopos_abi::task::MAX_PROCESSES / 4) as u32,
        ResourceKind::Task => (slopos_abi::task::MAX_TASKS / 4) as u32,
        _ => NO_LIMIT,
    }
}

pub(super) fn reset() {
    for at in 0..PERSON_ACCOUNTS {
        ROWS[at].store(0, Ordering::Release);
        PERSONS[at].store(0, Ordering::Release);
    }
}
